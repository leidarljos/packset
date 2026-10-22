//! CPU-only paraphrase retrieval eval over a synthetic developer set.
//!
//! The default run generates one deterministic dev set in memory, evaluates
//! search over pack sizes 20, 48 and 100, and reports:
//! - correct: admitted answer ids that include the expected memory
//! - wrong: admitted answer ids that exclude the expected memory
//! - stray: unrelated prompts that still admitted one or more ids
//!
//! The same harness can read a JSON dataset for held-out checks:
//!
//! ```console
//! cargo run --release -p packset-daemon --example paraphrase_eval -- \
//!   --dataset /tmp/paraphrase-heldout.json
//! ```

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use anyhow::Context;
use packset_core::bm25::Index;
use packset_core::panel::Panel;
use packset_core::search::{self, Record};
use serde::Deserialize;
use serde_json::{json, Value};

const DIMENSIONS: usize = 48;
const BASELINE_LINE: f64 = 0.66;
const ALIAS_WINDOW: f64 = 0.05;
const DEFAULT_SIZES: &[usize] = &[20, 48, 100];

#[derive(Clone, Debug, Deserialize)]
struct MemorySpec {
    id: String,
    text: String,
    anchor: String,
}

#[derive(Clone, Debug, Deserialize)]
struct QuestionSpec {
    text: String,
    answer: String,
    /// 0.0..=1.0; lower values make the paraphrase harder to admit.
    hardness: f64,
}

#[derive(Clone, Debug, Deserialize)]
struct Dataset {
    memories: Vec<MemorySpec>,
    distractors: Vec<MemorySpec>,
    questions: Vec<QuestionSpec>,
    unrelated: Vec<String>,
}

#[derive(Clone, Debug)]
struct DenseHit {
    id: String,
    cosine: f64,
    rank: usize,
}

#[derive(Default, Clone, Copy, Debug)]
struct Counts {
    asked: usize,
    correct: usize,
    wrong: usize,
    stray: usize,
}

impl Counts {
    fn misses(self) -> usize {
        self.asked.saturating_sub(self.correct + self.wrong)
    }
}

fn hash64(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn normalise(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector
        .iter()
        .map(|v| f64::from(*v) * f64::from(*v))
        .sum::<f64>()
        .sqrt();
    if norm <= 0.0 {
        return vector;
    }
    for value in &mut vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    vector
}

fn unit_vector(label: &str) -> Vec<f32> {
    let mut state = hash64(label);
    let mut out = Vec::with_capacity(DIMENSIONS);
    for _ in 0..DIMENSIONS {
        let bits = splitmix64(&mut state);
        let u = ((bits >> 11) as f64) / ((1u64 << 53) as f64);
        out.push((2.0 * u - 1.0) as f32);
    }
    normalise(out)
}

fn weighted(parts: &[(&[f32], f64)]) -> Vec<f32> {
    let mut out = vec![0.0f32; DIMENSIONS];
    for (vector, weight) in parts {
        for (slot, value) in out.iter_mut().zip(vector.iter()) {
            *slot += (f64::from(*value) * *weight) as f32;
        }
    }
    normalise(out)
}

fn memory_vector(memory: &MemorySpec) -> Vec<f32> {
    let concept = unit_vector(&format!("concept:{}", memory.anchor));
    let style = unit_vector(&format!("memory-style:{}:{}", memory.id, memory.text));
    weighted(&[(&concept, 0.84), (&style, 0.16)])
}

fn question_vector(question: &QuestionSpec, answer: &MemorySpec) -> Vec<f32> {
    let concept = unit_vector(&format!("concept:{}", answer.anchor));
    let style = unit_vector(&format!("question-style:{}", question.text));
    let drift = unit_vector(&format!("question-drift:{}", answer.id));
    let concept_weight = question.hardness.clamp(0.55, 0.95);
    weighted(&[
        (&concept, concept_weight),
        (&style, 1.0 - concept_weight),
        (&drift, 0.15),
    ])
}

fn unrelated_vector(prompt: &str) -> Vec<f32> {
    let style = unit_vector(&format!("unrelated-style:{prompt}"));
    let drift = unit_vector(&format!("unrelated-drift:{prompt}"));
    weighted(&[(&style, 0.9), (&drift, 0.1)])
}

fn atom_of(memory: &MemorySpec, embedding: &[f32]) -> Record {
    json!({
        "id": memory.id,
        "kind": "lesson",
        "text": memory.text,
        "ts": "2026-10-21T00:00:00Z",
        "embedding": embedding,
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

fn dense_hits(fused: &[Value]) -> Vec<DenseHit> {
    let mut out: Vec<DenseHit> = fused
        .iter()
        .filter_map(|hit| {
            Some(DenseHit {
                id: hit["id"].as_str()?.to_string(),
                cosine: hit["cosine"].as_f64()?,
                rank: hit["dense_rank"].as_u64()? as usize,
            })
        })
        .collect();
    out.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.id.cmp(&b.id)));
    out
}

fn alias_ids(dense: &[DenseHit], top: f64) -> Vec<String> {
    dense
        .iter()
        .filter(|hit| top - hit.cosine <= ALIAS_WINDOW)
        .map(|hit| hit.id.clone())
        .collect()
}

/// The shipped baseline gate: a dense leader at 0.66 or higher.
fn admit_gate66(dense: &[DenseHit]) -> Vec<String> {
    let Some(top) = dense.first() else {
        return Vec::new();
    };
    if top.cosine < BASELINE_LINE {
        return Vec::new();
    }
    vec![top.id.clone()]
}

/// The shipped local gate used in ljos: admit an alias group once the best
/// hit clears 0.66, where aliases are within 0.05 of that best cosine.
fn admit_ljos_alias_group(dense: &[DenseHit]) -> Vec<String> {
    let Some(top) = dense.first() else {
        return Vec::new();
    };
    if top.cosine < BASELINE_LINE {
        return Vec::new();
    }
    alias_ids(dense, top.cosine)
}

fn run_query(
    pack: &[MemorySpec],
    vectors: &HashMap<String, Vec<f32>>,
    query: &str,
    query_vector: &[f32],
) -> Vec<DenseHit> {
    let atoms: Vec<Record> = pack
        .iter()
        .filter_map(|memory| vectors.get(&memory.id).map(|v| atom_of(memory, v)))
        .collect();
    if atoms.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = atoms.iter().map(search::atom_tokens).collect();
    let index = Index::build(docs.iter().map(Vec::as_slice));
    let now = packset_core::clock::utcnow();
    let ask = search::Ask {
        user: "dev",
        memory: "dev",
        atoms: &atoms,
        query,
        limit: atoms.len(),
        set: None,
        kind: None,
        now: &now,
    };
    let lexical = search::search_linear_with(&ask, &docs);
    let bm25 = search::search_bm25(&ask, &index);
    let dense = search::search_dense(&ask, query_vector);
    let panel = Panel::named("combmnz", "none", "off").expect("a shipped panel");
    let mut fused =
        search::merge_ballots(&[lexical, bm25, dense.clone()], atoms.len(), &panel, &now);
    search::mark_dense(&mut fused, &dense);
    dense_hits(&fused)
}

fn evaluate_size(dataset: &Dataset, pack_size: usize) -> anyhow::Result<(Counts, Counts)> {
    let memories = dataset.memories.len().min(pack_size);
    let distractors = pack_size
        .saturating_sub(memories)
        .min(dataset.distractors.len());
    let mut pack = Vec::new();
    pack.extend(dataset.memories.iter().take(memories).cloned());
    pack.extend(dataset.distractors.iter().take(distractors).cloned());

    let vectors: HashMap<String, Vec<f32>> = pack
        .iter()
        .map(|memory| (memory.id.clone(), memory_vector(memory)))
        .collect();
    let answer_map: HashMap<String, MemorySpec> = dataset
        .memories
        .iter()
        .cloned()
        .map(|memory| (memory.id.clone(), memory))
        .collect();
    let in_pack: std::collections::HashSet<String> = pack.iter().map(|m| m.id.clone()).collect();

    let mut gate66 = Counts::default();
    let mut alias = Counts::default();
    for question in dataset
        .questions
        .iter()
        .filter(|q| in_pack.contains(&q.answer))
    {
        let answer = answer_map
            .get(&question.answer)
            .with_context(|| format!("missing answer memory {}", question.answer))?;
        let hits = run_query(
            &pack,
            &vectors,
            &question.text,
            &question_vector(question, answer),
        );
        gate66.asked += 1;
        alias.asked += 1;
        let gate_ids = admit_gate66(&hits);
        let alias_ids = admit_ljos_alias_group(&hits);
        if gate_ids.iter().any(|id| id == &question.answer) {
            gate66.correct += 1;
        } else if !gate_ids.is_empty() {
            gate66.wrong += 1;
        }
        if alias_ids.iter().any(|id| id == &question.answer) {
            alias.correct += 1;
        } else if !alias_ids.is_empty() {
            alias.wrong += 1;
        }
    }

    for prompt in &dataset.unrelated {
        let hits = run_query(&pack, &vectors, prompt, &unrelated_vector(prompt));
        if !admit_gate66(&hits).is_empty() {
            gate66.stray += 1;
        }
        if !admit_ljos_alias_group(&hits).is_empty() {
            alias.stray += 1;
        }
    }

    Ok((gate66, alias))
}

fn dev_dataset() -> Dataset {
    const MEMORY_ACTIONS: [(&str, &str); 12] = [
        ("archive", "store away"),
        ("rotate", "swap in"),
        ("publish", "send out"),
        ("snapshot", "capture"),
        ("lint", "clean up"),
        ("encrypt", "lock down"),
        ("backfill", "patch in"),
        ("mirror", "copy over"),
        ("verify", "double-check"),
        ("prune", "trim back"),
        ("reseed", "refill"),
        ("pin", "lock"),
    ];
    const MEMORY_OBJECTS: [(&str, &str); 14] = [
        ("incident timeline", "postmortem timeline"),
        ("deploy checklist", "release checklist"),
        ("schema migration plan", "database change plan"),
        ("support escalation playbook", "handoff playbook"),
        ("cache invalidation note", "cache reset note"),
        ("billing export job", "invoice export task"),
        ("audit evidence bundle", "audit packet"),
        ("retention policy", "data retention rule"),
        ("restore rehearsal", "recovery dry run"),
        ("on-call handoff note", "pager handoff"),
        ("sandbox fixture set", "test fixture set"),
        ("upgrade worksheet", "upgrade worksheet"),
        ("index rebuild runbook", "index refresh runbook"),
        ("partner sync brief", "partner sync brief"),
    ];
    const MEMORY_PLACES: [(&str, &str); 8] = [
        ("ops vault", "operations vault"),
        ("release notebook", "release notebook"),
        ("team ledger", "team ledger"),
        ("compliance binder", "compliance binder"),
        ("incident journal", "incident journal"),
        ("migration logbook", "migration logbook"),
        ("support queue note", "support queue note"),
        ("readiness board", "readiness board"),
    ];
    const MEMORY_WHEN: [(&str, &str); 8] = [
        (
            "before the weekly release cut",
            "before we cut the weekly release",
        ),
        ("right after triage closes", "once triage is closed"),
        (
            "before touching production data",
            "before anyone edits production data",
        ),
        (
            "at the end of each on-call shift",
            "when the on-call shift ends",
        ),
        ("before rotating credentials", "before rotating keys"),
        ("during the month-end close", "around month-end close"),
        ("after the rollback drill", "after the rollback practice"),
        (
            "before approving partner access",
            "before partner access is approved",
        ),
    ];

    let mut memories = Vec::new();
    let mut questions = Vec::new();
    for i in 0..48usize {
        let action = MEMORY_ACTIONS[(i * 7 + 3) % MEMORY_ACTIONS.len()];
        let object = MEMORY_OBJECTS[(i * 5 + 1) % MEMORY_OBJECTS.len()];
        let place = MEMORY_PLACES[(i * 3 + 2) % MEMORY_PLACES.len()];
        let when = MEMORY_WHEN[(i * 11 + 4) % MEMORY_WHEN.len()];
        let id = format!("m{:02}", i + 1);
        let anchor = format!("dev-{i}-{}-{}-{}", action.0, object.0, place.0);
        let text = match i % 3 {
            0 => format!(
                "Policy {id}: {} the {} in the {} {}, and append one sentence on what changed.",
                action.0, object.0, place.0, when.0
            ),
            1 => format!(
                "{id} records that the team must {} every {} in the {} {}. Keep the prior snapshot for audit replay.",
                action.0, object.0, place.0, when.0
            ),
            _ => format!(
                "{id} states: {} {} {}. Include owner, ticket link, and the validation command that was run.",
                action.0, object.0, when.0
            ),
        };
        let hardness = match i % 6 {
            0 => 0.83,
            1 => 0.77,
            2 => 0.72,
            3 => 0.67,
            4 => 0.63,
            _ => 0.59,
        };
        let question = match i % 3 {
            0 => format!(
                "When {}, where do we {} the {}?",
                when.1, action.1, object.1
            ),
            1 => format!(
                "I need the rule for {} {} {}; which memory says it?",
                action.1, object.1, when.1
            ),
            _ => format!(
                "Which note explains how to {} the {} {}?",
                action.1, object.1, when.1
            ),
        };
        memories.push(MemorySpec {
            id: id.clone(),
            text,
            anchor,
        });
        questions.push(QuestionSpec {
            text: question,
            answer: id,
            hardness,
        });
    }

    let mut distractors = Vec::new();
    for i in 0..80usize {
        let action = MEMORY_ACTIONS[(i * 13 + 2) % MEMORY_ACTIONS.len()];
        let object = MEMORY_OBJECTS[(i * 9 + 6) % MEMORY_OBJECTS.len()];
        let place = MEMORY_PLACES[(i * 5 + 1) % MEMORY_PLACES.len()];
        let when = MEMORY_WHEN[(i * 7 + 3) % MEMORY_WHEN.len()];
        let id = format!("d{:03}", i + 1);
        let anchor = format!("dev-d-{i}-{}-{}-{}", action.0, object.0, place.0);
        let text = match i % 4 {
            0 => format!(
                "{id}: {} the {} {} and file the receipt in the {}.",
                action.0, object.0, when.0, place.0
            ),
            1 => format!(
                "{id}: {} {} under the {}, with two reviewers signing off.",
                action.0, object.0, place.0
            ),
            2 => format!(
                "{id}: {} {} {}. Include the command output and follow-up owner.",
                action.0, object.0, when.0
            ),
            _ => format!(
                "{id}: keep a snapshot of the {} in the {} before any handoff.",
                object.0, place.0
            ),
        };
        distractors.push(MemorySpec { id, text, anchor });
    }

    let unrelated = vec![
        "What coffee beans should we order for the office kitchen?".to_string(),
        "Suggest a lunch spot near the train station.".to_string(),
        "Write a haiku about rainy weekends.".to_string(),
        "How do I tune my acoustic guitar by ear?".to_string(),
        "List three stretches for sore shoulders.".to_string(),
        "What is a good beginner telescope?".to_string(),
        "Plan a two-day road trip with scenic overlooks.".to_string(),
        "How often should houseplants be repotted?".to_string(),
        "Recommend a movie for a family game night.".to_string(),
        "What are the rules of cribbage?".to_string(),
        "Give me a simple pancake recipe.".to_string(),
        "How can I remove rust from bike tools?".to_string(),
        "What are signs of overwatering a fern?".to_string(),
        "Name five novels set in coastal towns.".to_string(),
        "How do I clean a mechanical keyboard?".to_string(),
        "Which camera settings help for sunset photos?".to_string(),
        "Suggest a weekend workout split.".to_string(),
        "How do I stop fog on swim goggles?".to_string(),
        "What is the easiest knot for camping?".to_string(),
        "How do I make cold brew at home?".to_string(),
    ];

    Dataset {
        memories,
        distractors,
        questions,
        unrelated,
    }
}

fn parse_sizes(raw: &str) -> anyhow::Result<Vec<usize>> {
    let mut out = Vec::new();
    for item in raw.split(',') {
        let value: usize = item
            .trim()
            .parse()
            .with_context(|| format!("bad pack size '{item}'"))?;
        if value == 0 {
            continue;
        }
        out.push(value);
    }
    if out.is_empty() {
        anyhow::bail!("at least one pack size is required");
    }
    Ok(out)
}

fn read_dataset(path: &str) -> anyhow::Result<Dataset> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let set: Dataset = serde_json::from_str(&text).with_context(|| format!("parsing {path}"))?;
    if set.memories.is_empty() {
        anyhow::bail!("dataset has no memories");
    }
    if set.questions.is_empty() {
        anyhow::bail!("dataset has no questions");
    }
    Ok(set)
}

fn main() -> anyhow::Result<()> {
    let mut dataset_path: Option<String> = None;
    let mut sizes = DEFAULT_SIZES.to_vec();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--dataset" => {
                i += 1;
                let Some(path) = args.get(i) else {
                    anyhow::bail!("--dataset needs a path");
                };
                dataset_path = Some(path.clone());
            }
            "--sizes" => {
                i += 1;
                let Some(raw) = args.get(i) else {
                    anyhow::bail!("--sizes needs a comma list");
                };
                sizes = parse_sizes(raw)?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: paraphrase_eval [--dataset PATH] [--sizes 20,48,100]\n\
                     Uses the in-repo dev set when --dataset is omitted."
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    let dataset = match dataset_path {
        Some(path) => read_dataset(&path)?,
        None => dev_dataset(),
    };
    println!(
        "dataset: memories={}, distractors={}, questions={}, unrelated={}",
        dataset.memories.len(),
        dataset.distractors.len(),
        dataset.questions.len(),
        dataset.unrelated.len()
    );
    println!(
        "| pack | rule | asked | correct | wrong | miss | stray |\n|---:|:---|---:|---:|---:|---:|---:|"
    );
    for size in sizes {
        let (gate66, alias) = evaluate_size(&dataset, size)?;
        println!(
            "| {size} | 0.66 gate | {} | {} | {} | {} | {} |",
            gate66.asked,
            gate66.correct,
            gate66.wrong,
            gate66.misses(),
            gate66.stray
        );
        println!(
            "| {size} | ljos alias (<= 0.05) | {} | {} | {} | {} | {} |",
            alias.asked,
            alias.correct,
            alias.wrong,
            alias.misses(),
            alias.stray
        );
    }
    Ok(())
}
