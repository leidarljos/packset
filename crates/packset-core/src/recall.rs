//! Include-first recall over live atoms.
//!
//! A small pack returns everything live: ranking a set the reader will see in
//! full only costs the reader ordering they did not ask for. Past the limit it
//! walks one hop along `links` from the seeds, so what comes back is what the
//! pack itself says is related rather than what a scorer guessed.

use serde_json::{Map, Value};

use crate::record;

/// Atoms returned when the caller names no limit, and the ceiling on any.
pub const DEFAULT_LIMIT: usize = 64;

/// The most of a recall budget the due queue may take when the caller
/// asked about something: a quarter, one at least. A pack a herd writes
/// into holds hundreds of due claims, and a recall that spent its whole
/// budget on them answered no cue at all (the forgetting corpus: the kept
/// claim ranked nowhere while retrievability alone ranked it first).
#[must_use]
pub fn due_share(cap: usize) -> usize {
    (cap / 4).max(1)
}
/// Characters of atom text the answer may carry.
pub const TEXT_BUDGET: usize = 32_000;

/// One atom.
pub type Record = Map<String, Value>;

/// What the caller said they are doing.
#[derive(Debug, Clone, Default)]
pub struct Hints {
    /// Free text: the user's message, a query, tool names.
    pub text: String,
    /// Entities the caller already knows are in play.
    pub entities: Vec<String>,
}

impl Hints {
    /// Whether the caller gave anything to match on.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.entities.is_empty()
    }
}

fn trust_of(atom: &Record) -> f64 {
    atom.get("trust").and_then(Value::as_f64).unwrap_or(0.0)
}

fn text_of(atom: &Record) -> &str {
    atom.get("text").and_then(Value::as_str).unwrap_or("")
}

fn id_of(atom: &Record) -> &str {
    atom.get("id").and_then(Value::as_str).unwrap_or("")
}

fn ts_of(atom: &Record) -> &str {
    atom.get("ts").and_then(Value::as_str).unwrap_or("")
}

/// Due queue first, then trust descending, then newest, then id.
///
/// Due comes first because a review that is late is the one piece of the pack
/// with a deadline; everything after it is preference.
#[must_use]
pub fn sort_atoms(atoms: &[Record], now: &str) -> Vec<Record> {
    let mut out = atoms.to_vec();
    out.sort_by(|a, b| {
        let due = record::is_due(b, now).cmp(&record::is_due(a, now));
        due.then_with(|| {
            trust_of(b)
                .partial_cmp(&trust_of(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            retrievability_of(b, now)
                .partial_cmp(&retrievability_of(a, now))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| ts_of(b).cmp(ts_of(a)))
        .then_with(|| id_of(a).cmp(id_of(b)))
    });
    out
}

/// The review model's odds the claim is still recalled: from the last
/// review, or the write, and the claim's stability. Within a tier this
/// ranks a claim the seat kept recalling above a newer one it never asked
/// for, where recency alone ranked the newer one first.
fn retrievability_of(atom: &Record, now: &str) -> f64 {
    let review = atom.get("review");
    let last = review
        .and_then(|r| r.get("last"))
        .and_then(Value::as_str)
        .or_else(|| atom.get("ts").and_then(Value::as_str))
        .unwrap_or(now);
    let stability = review
        .and_then(|r| r.get("stability"))
        .and_then(Value::as_f64)
        .unwrap_or(record::DEFAULT_STABILITY);
    crate::decay::retrievability(crate::clock::elapsed_days(last, now), stability)
}

/// Take atoms until the text budget is spent, always keeping the first.
///
/// Always the first, because an answer of nothing is worse than an answer of
/// one thing too long: the caller can see and shorten what came back.
#[must_use]
pub fn apply_budget(atoms: Vec<Record>, budget: usize) -> Vec<Record> {
    let mut out: Vec<Record> = Vec::new();
    let mut used = 0usize;
    for atom in atoms {
        let n = text_of(&atom).chars().count();
        if !out.is_empty() && used + n > budget {
            break;
        }
        used += n;
        out.push(atom);
    }
    out
}

/// A caller's limit, floored at zero and capped at [`DEFAULT_LIMIT`].
#[must_use]
pub fn cap_limit(limit: Option<i64>) -> usize {
    match limit {
        None => DEFAULT_LIMIT,
        Some(v) if v < 0 => 0,
        Some(v) => (v as usize).min(DEFAULT_LIMIT),
    }
}

/// Lowercase tokens of two or more characters.
#[must_use]
pub fn tokens(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphanumeric() {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
            {
                i += 1;
            }
            if i - start >= 2 {
                out.push(lower[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Whether an atom answers to the hints.
#[must_use]
pub fn matches_hints(atom: &Record, hints: &Hints) -> bool {
    let entities = record::entities_of(atom);
    if !hints.entities.is_empty()
        && hints
            .entities
            .iter()
            .any(|wanted| entities.contains(wanted.trim()))
    {
        return true;
    }
    let want = tokens(&hints.text);
    if want.is_empty() {
        return false;
    }
    // At least half the cue's tokens, in the text or the entities. One
    // shared word seeded the whole pack when the cue held a common one.
    let hay = text_of(atom).to_ascii_lowercase();
    let lowered: Vec<String> = entities.iter().map(|e| e.to_ascii_lowercase()).collect();
    let matched = want
        .iter()
        .filter(|tok| hay.contains(tok.as_str()) || lowered.iter().any(|e| e == *tok))
        .count();
    matched * 2 >= want.len()
}

fn resolve_seeds(live: &[Record], seeds: &[String], hints: &Hints) -> Vec<String> {
    if !seeds.is_empty() {
        let live_ids: std::collections::HashSet<&str> =
            live.iter().map(id_of).filter(|i| !i.is_empty()).collect();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for seed in seeds {
            if live_ids.contains(seed.as_str()) && seen.insert(seed.clone()) {
                out.push(seed.clone());
            }
        }
        return out;
    }
    if hints.is_empty() {
        return Vec::new();
    }
    live.iter()
        .filter(|a| !id_of(a).is_empty() && matches_hints(a, hints))
        .map(|a| id_of(a).to_string())
        .collect()
}

/// The seeds themselves and everything one link away from them.
#[must_use]
fn one_hop(live: &[Record], seed_ids: &[String]) -> (Vec<Record>, Vec<Record>) {
    let by_id: std::collections::HashMap<&str, &Record> = live
        .iter()
        .map(|a| (id_of(a), a))
        .filter(|(id, _)| !id.is_empty())
        .collect();
    let seed_set: std::collections::HashSet<&str> = seed_ids
        .iter()
        .map(String::as_str)
        .filter(|id| by_id.contains_key(id))
        .collect();
    let mut neighbour_ids: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for sid in &seed_set {
        let Some(atom) = by_id.get(sid) else { continue };
        let Some(links) = atom.get("links").and_then(Value::as_array) else {
            continue;
        };
        for link in links {
            let Some(lid) = link.as_str() else { continue };
            if by_id.contains_key(lid) && !seed_set.contains(lid) {
                if let Some((key, _)) = by_id.get_key_value(lid) {
                    neighbour_ids.insert(key);
                }
            }
        }
    }
    let seeds: Vec<Record> = seed_ids
        .iter()
        .filter_map(|sid| by_id.get(sid.as_str()).map(|a| (*a).clone()))
        .collect();
    let neighbours: Vec<Record> = neighbour_ids
        .into_iter()
        .filter_map(|nid| by_id.get(nid).map(|a| (*a).clone()))
        .collect();
    (seeds, neighbours)
}

/// The live atoms worth handing to the next action.
///
/// `atoms` is the caller's live set, which is what the store already computed;
/// this function never reads a store of its own.
#[must_use]
pub fn recall(
    atoms: &[Record],
    seeds: &[String],
    hints: &Hints,
    limit: Option<i64>,
    now: &str,
) -> Vec<Record> {
    let cap = cap_limit(limit);
    if cap == 0 {
        return Vec::new();
    }
    let mut live: Vec<Record> = atoms
        .iter()
        .filter(|a| record::is_live(a, now) || record::is_due(a, now))
        .cloned()
        .collect();
    record::filter_live_links(&mut live);

    // While the pack is small the whole thing is the answer, so nothing is
    // ranked away from a reader who was going to see all of it.
    if live.len() <= cap {
        let sorted = sort_atoms(&live, now);
        return apply_budget(sorted.into_iter().take(cap).collect(), TEXT_BUDGET);
    }

    // With a cue in hand the due queue is narrowed to what touches it and
    // bounded to a share of the budget; a bare recall is the review path
    // and takes the whole queue.
    let asked = !seeds.is_empty() || !hints.is_empty();
    let mut due: Vec<Record> = live
        .iter()
        .filter(|a| record::is_due(a, now))
        .filter(|a| !asked || hints.is_empty() || matches_hints(a, hints))
        .cloned()
        .collect();
    let seed_ids = resolve_seeds(&live, seeds, hints);
    if asked {
        due = sort_atoms(&due, now);
        due.truncate(due_share(cap));
    }
    if seed_ids.is_empty() && due.is_empty() {
        return Vec::new();
    }
    let (seed_atoms, neighbours) = if seed_ids.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        one_hop(&live, &seed_ids)
    };

    // Due, then the neighbourhood, then the seeds themselves: the caller
    // already has the seeds in hand, so they are the least worth the budget.
    let mut ranked = sort_atoms(&due, now);
    ranked.extend(sort_atoms(&neighbours, now));
    ranked.extend(sort_atoms(&seed_atoms, now));

    let mut seen = std::collections::HashSet::new();
    let mut picked = Vec::new();
    for atom in ranked {
        let id = id_of(&atom).to_string();
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        picked.push(atom);
        if picked.len() >= cap {
            break;
        }
    }
    apply_budget(picked, TEXT_BUDGET)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: &str = "2026-01-01T00:00:00.000Z";

    fn atom(value: Value) -> Record {
        value.as_object().unwrap().clone()
    }

    fn ids(atoms: &[Record]) -> Vec<String> {
        atoms.iter().map(|a| id_of(a).to_string()).collect()
    }

    #[test]
    fn a_limit_is_floored_and_capped() {
        assert_eq!(cap_limit(None), DEFAULT_LIMIT);
        assert_eq!(cap_limit(Some(-1)), 0);
        assert_eq!(cap_limit(Some(0)), 0);
        assert_eq!(cap_limit(Some(10)), 10);
        assert_eq!(cap_limit(Some(1000)), DEFAULT_LIMIT);
    }

    #[test]
    fn tokens_need_two_characters() {
        assert_eq!(tokens("A ripgrep-2 x"), vec!["ripgrep-2".to_string()]);
        assert_eq!(tokens("a b c"), Vec::<String>::new());
        assert_eq!(tokens("Header_block"), vec!["header_block".to_string()]);
    }

    #[test]
    fn a_small_pack_comes_back_whole() {
        let live: Vec<Record> = (0..5)
            .map(|i| atom(json!({"id": format!("a{i}"), "text": "x"})))
            .collect();
        let got = recall(&live, &[], &Hints::default(), None, NOW);
        assert_eq!(got.len(), 5, "nothing is ranked away from a full reader");
    }

    #[test]
    fn the_due_queue_leads_whatever_the_trust_says() {
        let live = vec![
            atom(json!({"id": "trusted", "text": "x", "trust": 9.0})),
            atom(
                json!({"id": "late", "text": "x", "trust": 0.1, "due_at": "2020-01-01T00:00:00.000Z"}),
            ),
        ];
        let got = sort_atoms(&live, NOW);
        assert_eq!(ids(&got), vec!["late".to_string(), "trusted".to_string()]);
    }

    #[test]
    fn one_common_word_does_not_seed_the_whole_pack() {
        let atom = atom(json!({"id": "a", "text": "The settled answer on the fuse is CombMNZ."}));
        let common = Hints {
            text: "the answer".into(),
            entities: Vec::new(),
        };
        assert!(matches_hints(&atom, &common), "both words are in the text");
        let mostly_other = Hints {
            text: "answer about a topic nobody wrote of".into(),
            entities: Vec::new(),
        };
        assert!(
            !matches_hints(&atom, &mostly_other),
            "one word of six is not a match"
        );
        let half = Hints {
            text: "fuse rewrite".into(),
            entities: Vec::new(),
        };
        assert!(matches_hints(&atom, &half), "half the cue is enough");
    }

    #[test]
    fn a_cue_is_not_buried_under_unrelated_due_claims() {
        // Forty due claims about other matters, one live claim about the
        // cue: the cue's claim is in the answer, and the due queue takes at
        // most its share of the budget.
        let mut live: Vec<Record> = (0..40)
            .map(|i| {
                atom(json!({
                    "id": format!("due{i:02}"),
                    "text": format!("unrelated matter number {i} still pending review"),
                    "due_at": "2020-01-01T00:00:00.000Z",
                    "ts": "2025-12-01T00:00:00.000Z"
                }))
            })
            .collect();
        live.push(atom(json!({
            "id": "kept",
            "text": "The settled answer on the fuse is CombMNZ.",
            "ts": "2025-06-01T00:00:00.000Z",
            "due_at": "2099-01-01T00:00:00.000Z"
        })));
        let hints = Hints {
            text: "fuse settled answer".into(),
            entities: Vec::new(),
        };
        let got = ids(&recall(&live, &[], &hints, Some(10), NOW));
        assert!(got.contains(&"kept".to_string()), "{got:?}");
        let due_taken = got.iter().filter(|i| i.starts_with("due")).count();
        assert!(due_taken <= due_share(10), "{got:?}");
        // Without a cue the whole due queue is the answer, as the review
        // path wants.
        let bare = ids(&recall(&live, &[], &Hints::default(), Some(10), NOW));
        assert_eq!(bare.len(), 10);
        assert!(bare.iter().all(|i| i.starts_with("due")), "{bare:?}");
    }

    #[test]
    fn a_recalled_claim_outranks_a_newer_unreviewed_one() {
        // Kept: written a year ago, reviewed last week, stability of sixty
        // days. Late: written a month ago, never reviewed, stability of one.
        let live = vec![
            atom(
                json!({"id": "late", "text": "x", "trust": 1.0, "ts": "2025-12-01T00:00:00.000Z",
                "due_at": "2099-01-01T00:00:00.000Z"}),
            ),
            atom(
                json!({"id": "kept", "text": "x", "trust": 1.0, "ts": "2025-01-01T00:00:00.000Z",
                "due_at": "2099-01-01T00:00:00.000Z",
                "review": {"last": "2025-12-25T00:00:00.000Z", "stability": 60.0}}),
            ),
        ];
        assert_eq!(
            ids(&sort_atoms(&live, NOW)),
            vec!["kept".to_string(), "late".to_string()]
        );
    }

    #[test]
    fn ties_break_on_recency_then_id() {
        let live = vec![
            atom(json!({"id": "b", "text": "x", "trust": 1.0, "ts": "2026-01-01T00:00:00.000Z"})),
            atom(json!({"id": "a", "text": "x", "trust": 1.0, "ts": "2026-01-01T00:00:00.000Z"})),
            atom(json!({"id": "c", "text": "x", "trust": 1.0, "ts": "2026-02-01T00:00:00.000Z"})),
        ];
        assert_eq!(
            ids(&sort_atoms(&live, NOW)),
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn the_budget_always_keeps_the_first_atom() {
        let huge = atom(json!({"id": "one", "text": "x".repeat(TEXT_BUDGET * 2)}));
        let second = atom(json!({"id": "two", "text": "y"}));
        let got = apply_budget(vec![huge, second], TEXT_BUDGET);
        assert_eq!(ids(&got), vec!["one".to_string()], "an answer beats none");
    }

    #[test]
    fn past_the_limit_it_walks_one_hop_from_the_seeds() {
        let mut live: Vec<Record> = (0..70)
            .map(|i| atom(json!({"id": format!("filler{i}"), "text": "unrelated"})))
            .collect();
        live.push(atom(json!({
            "id": "seed", "text": "the parser", "links": ["near"]
        })));
        live.push(atom(json!({
            "id": "near", "text": "next to it", "links": ["seed"]
        })));
        live.push(atom(json!({"id": "far", "text": "nowhere near"})));

        let got = recall(&live, &["seed".into()], &Hints::default(), Some(8), NOW);
        let got_ids = ids(&got);
        assert!(got_ids.contains(&"seed".to_string()), "{got_ids:?}");
        assert!(got_ids.contains(&"near".to_string()), "{got_ids:?}");
        assert!(!got_ids.contains(&"far".to_string()), "{got_ids:?}");
    }

    #[test]
    fn without_a_seed_or_a_due_atom_a_large_pack_answers_nothing() {
        let live: Vec<Record> = (0..70)
            .map(|i| atom(json!({"id": format!("a{i}"), "text": "x"})))
            .collect();
        assert!(recall(&live, &[], &Hints::default(), Some(8), NOW).is_empty());
    }

    #[test]
    fn hints_can_stand_in_for_a_seed() {
        let mut live: Vec<Record> = (0..70)
            .map(|i| atom(json!({"id": format!("a{i}"), "text": "unrelated"})))
            .collect();
        live.push(atom(json!({"id": "hit", "text": "about ripgrep here"})));
        let hints = Hints {
            text: "ripgrep".into(),
            entities: Vec::new(),
        };
        let got = ids(&recall(&live, &[], &hints, Some(8), NOW));
        assert!(got.contains(&"hit".to_string()), "{got:?}");
    }

    #[test]
    fn an_entity_hint_matches_a_declared_entity() {
        let atom = atom(json!({"id": "a", "text": "nothing in the prose", "entities": ["Parser"]}));
        let by_entity = Hints {
            text: String::new(),
            entities: vec!["Parser".into()],
        };
        assert!(matches_hints(&atom, &by_entity));
        // And case-insensitively through the token path.
        let by_token = Hints {
            text: "parser".into(),
            entities: Vec::new(),
        };
        assert!(matches_hints(&atom, &by_token));
    }

    #[test]
    fn an_expired_atom_stays_out_but_a_due_one_does_not() {
        let live = vec![
            atom(json!({"id": "gone", "text": "x", "valid_to": "2020-01-01T00:00:00.000Z"})),
            atom(json!({
                "id": "due", "text": "x",
                "valid_to": "2020-01-01T00:00:00.000Z",
                "due_at": "2020-01-01T00:00:00.000Z"
            })),
            atom(json!({"id": "live", "text": "x"})),
        ];
        let got = ids(&recall(&live, &[], &Hints::default(), None, NOW));
        assert!(got.contains(&"live".to_string()), "{got:?}");
        assert!(got.contains(&"due".to_string()), "{got:?}");
        assert!(!got.contains(&"gone".to_string()), "{got:?}");
    }

    #[test]
    fn a_zero_limit_answers_nothing() {
        let live = vec![atom(json!({"id": "a", "text": "x"}))];
        assert!(recall(&live, &[], &Hints::default(), Some(0), NOW).is_empty());
    }
}
