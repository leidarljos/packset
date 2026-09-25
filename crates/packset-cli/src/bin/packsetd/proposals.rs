//! Mining an archived day into proposals a person can accept.
//!
//! Nothing here writes an atom. A proposal is a suggestion with a verdict on
//! it, and the only path from one to a stored claim runs through an explicit
//! accept. That is the whole reason the pack cannot be talked into believing
//! something: text that was merely read never becomes text that is remembered.

use std::collections::BTreeSet;
use std::path::PathBuf;

use packset_core::cheap::{self, CheapJob, CheapWhen};
use packset_core::{clock, extract};
use serde_json::{json, Map, Value};

use crate::cards;
use crate::home::Home;
use crate::store::Record;

/// Schema name for a proposal record.
pub const SCHEMA: &str = "inside.proposal/v1";
/// A claim shorter than this is not worth proposing.
pub const MIN_CLAIM: usize = 8;

/// A cheap-model job is not allowed at this point in the cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheapError(pub String);

impl std::fmt::Display for CheapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CheapError {}

/// Parse a job name, or say it is unknown.
fn parse_job(name: &str) -> Option<CheapJob> {
    match name {
        "extract" => Some(CheapJob::Extract),
        "fidelity" => Some(CheapJob::Fidelity),
        "linkRewrite" => Some(CheapJob::LinkRewrite),
        "dueSuggest" => Some(CheapJob::DueSuggest),
        _ => None,
    }
}

fn parse_when(name: &str) -> Option<CheapWhen> {
    match name {
        "compaction" => Some(CheapWhen::Compaction),
        "onDemand" => Some(CheapWhen::OnDemand),
        _ => None,
    }
}

/// Where one workspace's proposals are kept.
#[must_use]
pub fn proposals_path(home: &Home, workspace: &str) -> PathBuf {
    home.workspace_dir(workspace).join("proposals.jsonl")
}

/// The open proposals, last record per id winning.
///
/// The file is append-only, so accepting one leaves both records and the
/// later status is the answer. The history stays readable.
#[must_use]
pub fn list_open(home: &Home, workspace: &str) -> Vec<Value> {
    let path = proposals_path(home, workspace);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut seen: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        seen.insert(id.to_string(), value);
    }
    seen.into_values()
        .filter(|rec| rec.get("status").and_then(Value::as_str) == Some("open"))
        .collect()
}

fn append(home: &Home, workspace: &str, rec: &Value) -> std::io::Result<()> {
    use std::io::Write;
    let path = proposals_path(home, workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string(rec)?)
}

/// A claim reduced to what two spellings of it have in common.
#[must_use]
pub fn norm_claim(text: &str) -> String {
    text.to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', ',', ';', ':'])
        .to_string()
}

/// Everything already in the splice, which compaction must not re-capture.
///
/// Without this the miner proposes back what the cards and atoms already say,
/// once per day, forever.
#[must_use]
pub fn fence(home: &Home, workspace: &str, live: &[Record]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for path in [home.user_path(), home.memory_path(workspace)] {
        for part in entries(&cards::read_text(&path)) {
            let n = norm_claim(&part);
            if !n.is_empty() {
                out.insert(n);
            }
        }
    }
    for atom in live {
        let n = norm_claim(atom.get("text").and_then(Value::as_str).unwrap_or(""));
        if !n.is_empty() {
            out.insert(n);
        }
    }
    out
}

/// Paragraphs, the unit a card and an archive day are both lists of.
fn entries(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                let joined = current.join("\n").trim().to_string();
                if !joined.is_empty() {
                    out.push(joined);
                }
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        let joined = current.join("\n").trim().to_string();
        if !joined.is_empty() {
            out.push(joined);
        }
    }
    out
}

/// Whether the fence already covers a claim, either way round.
#[must_use]
pub fn is_fenced(claim: &str, wall: &BTreeSet<String>) -> bool {
    let n = norm_claim(claim);
    if n.is_empty() {
        return false;
    }
    wall.iter()
        .any(|item| *item == n || n.contains(item.as_str()) || item.contains(n.as_str()))
}

/// Whether the transcript actually carries the claim.
///
/// Not evidence at all: it says the words were said, not that they were true.
/// Anything without a transcript is `NEI`, and only `SUPPORTED` can be
/// accepted, so a claim nobody can point at stays a suggestion.
#[must_use]
pub fn fidelity_verdict(claim: &str, transcript: Option<&str>) -> &'static str {
    let n = norm_claim(claim);
    if n.is_empty() {
        return "NEI";
    }
    let source = norm_claim(transcript.unwrap_or(""));
    if !source.is_empty()
        && (source == n || source.contains(n.as_str()) || n.contains(source.as_str()))
    {
        "SUPPORTED"
    } else {
        "NEI"
    }
}

/// The head of the text up to its first sentence end.
#[must_use]
pub fn first_sentence(text: &str) -> String {
    let end = text.find(['.', '!', '?']).unwrap_or(text.len());
    text[..end]
        .trim()
        .trim_end_matches(['.', ',', ';', ':'])
        .to_string()
}

/// What a proposal is being mined out of.
#[derive(Debug, Clone, Copy)]
pub struct Mining<'a> {
    /// The workspace the claim belongs to.
    pub workspace: &'a str,
    /// The cheap-model job asking.
    pub job: &'a str,
    /// Where in the cycle it is asking.
    pub when: &'a str,
    /// Everything already in the splice.
    pub wall: &'a BTreeSet<String>,
    /// What was actually said, if the caller has it.
    pub transcript: Option<&'a str>,
}

/// Propose one claim from one piece of text, or nothing.
///
/// # Errors
///
/// [`CheapError`] when the job is not allowed at this point in the cycle.
pub fn propose(
    home: &Home,
    mining: Mining<'_>,
    text: &str,
    new_id: impl FnOnce() -> String,
) -> Result<Option<Value>, CheapError> {
    let Mining {
        workspace,
        job,
        when,
        wall,
        transcript,
    } = mining;
    let allowed = match (parse_job(job), parse_when(when)) {
        (Some(j), Some(w)) => cheap::allowed(j, w),
        _ => false,
    };
    if !allowed {
        return Err(CheapError(format!("{job} is not allowed on {when}")));
    }
    let blob = text.trim();
    if blob.is_empty() || extract::is_tool_dump(blob) {
        return Ok(None);
    }
    let claim = first_sentence(blob);
    if claim.chars().count() < MIN_CLAIM {
        return Ok(None);
    }
    if is_fenced(&claim, wall) {
        return Ok(None);
    }
    let rec = json!({
        "schema": SCHEMA,
        "id": new_id(),
        "workspace": workspace,
        "text": claim,
        "job": job,
        "when": when,
        "status": "open",
        "ts": clock::utcnow(),
        "span": claim,
        "verdict": fidelity_verdict(&claim, transcript),
        "transcript": transcript.unwrap_or(""),
    });
    append(home, workspace, &rec).map_err(|e| CheapError(e.to_string()))?;
    Ok(Some(rec))
}

/// Mine one archived day into proposals.
///
/// # Errors
///
/// [`CheapError`] as [`propose`].
pub fn compact_day(
    home: &Home,
    workspace: &str,
    day: Option<&str>,
    live: &[Record],
    transcript: Option<&str>,
    mut new_id: impl FnMut() -> String,
) -> Result<Vec<Value>, CheapError> {
    let stamp = day.map_or_else(|| clock::utcnow()[..10].to_string(), str::to_string);
    let blob = cards::read_text(&home.archive_path(workspace, &stamp));
    let wall = fence(home, workspace, live);
    let mining = Mining {
        workspace,
        job: "extract",
        when: "compaction",
        wall: &wall,
        transcript,
    };
    let mut out = Vec::new();
    for part in entries(&blob) {
        if let Some(rec) = propose(home, mining, &part, &mut new_id)? {
            out.push(rec);
        }
    }
    Ok(out)
}

/// The atom an accepted proposal becomes, unstored.
///
/// # Errors
///
/// [`CheapError`] when the id is not open, or its verdict is not `SUPPORTED`.
pub fn accept(
    home: &Home,
    workspace: &str,
    proposal_id: &str,
) -> Result<(Record, Value), CheapError> {
    let open = list_open(home, workspace);
    let rec = open
        .into_iter()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(proposal_id))
        .ok_or_else(|| CheapError(format!("no open proposal {proposal_id}")))?;
    let verdict = rec.get("verdict").and_then(Value::as_str).unwrap_or("NEI");
    if verdict != "SUPPORTED" {
        return Err(CheapError(format!("extractAccept rejected: {verdict}")));
    }
    let text = rec.get("text").and_then(Value::as_str).unwrap_or("");
    let now = clock::utcnow();
    let mut atom = Map::new();
    atom.insert("schema".into(), json!(packset_core::record::SCHEMA));
    atom.insert("workspace".into(), json!(workspace));
    atom.insert("about_peer".into(), json!("user"));
    atom.insert("by_peer".into(), json!("extract"));
    atom.insert("kind".into(), json!("lesson"));
    // Derived, not explicit: nobody typed this, the miner found it, and a
    // reader deciding how much to trust it should be able to see that.
    atom.insert("level".into(), json!("derived"));
    atom.insert("text".into(), json!(text.trim()));
    atom.insert("source".into(), Value::Null);
    atom.insert("ts".into(), json!(now));
    atom.insert("valid_from".into(), json!(now));
    atom.insert("valid_to".into(), Value::Null);
    atom.insert("due_at".into(), Value::Null);
    atom.insert("links".into(), json!([]));
    atom.insert("embedding".into(), Value::Null);
    atom.insert("trust".into(), json!(1.0));
    atom.insert("tombstone".into(), json!(false));
    Ok((atom, rec))
}

/// Record that a proposal was accepted, naming the atom it became.
///
/// # Errors
///
/// The append's.
pub fn mark_accepted(
    home: &Home,
    workspace: &str,
    proposal: &Value,
    atom_id: &str,
) -> Result<(), CheapError> {
    let mut rec = proposal.as_object().cloned().unwrap_or_default();
    rec.insert("status".into(), json!("accepted"));
    rec.insert("atom_id".into(), json!(atom_id));
    rec.insert("ts".into(), json!(clock::utcnow()));
    append(home, workspace, &Value::Object(rec)).map_err(|e| CheapError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        (dir, home)
    }

    fn ids() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("p{n}")
        }
    }

    #[test]
    fn a_claim_normalizes_to_what_two_spellings_share() {
        assert_eq!(norm_claim("  Open  The Links.  "), "open the links");
        assert_eq!(norm_claim("Open the links;"), "open the links");
        assert_eq!(norm_claim(""), "");
    }

    #[test]
    fn extraction_is_forbidden_off_compaction() {
        let (_dir, home) = home();
        let wall = BTreeSet::new();
        let mining = Mining {
            workspace: "w",
            job: "extract",
            when: "onDemand",
            wall: &wall,
            transcript: None,
        };
        let err = propose(&home, mining, "A claim worth keeping.", || "p".into()).unwrap_err();
        assert_eq!(err.0, "extract is not allowed on onDemand");
        // And an unknown job is refused rather than waved through.
        let unknown = Mining {
            job: "whatever",
            when: "compaction",
            ..mining
        };
        assert!(propose(&home, unknown, "A claim.", || "p".into()).is_err());
    }

    #[test]
    fn the_first_sentence_is_the_claim() {
        assert_eq!(first_sentence("One thing. Another."), "One thing");
        assert_eq!(first_sentence("No terminator here"), "No terminator here");
        assert_eq!(first_sentence("Trailing comma,"), "Trailing comma");
        assert_eq!(first_sentence("Ends with colon:"), "Ends with colon");
        assert_eq!(first_sentence("a . b"), "a");
        assert_eq!(first_sentence("Q? A."), "Q");
    }

    #[test]
    fn a_short_claim_or_a_tool_dump_proposes_nothing() {
        let (_dir, home) = home();
        let wall = BTreeSet::new();
        let mut next = ids();
        for text in ["short.", "  ", ""] {
            let got = propose(
                &home,
                Mining {
                    workspace: "w",
                    job: "extract",
                    when: "compaction",
                    wall: &wall,
                    transcript: None,
                },
                text,
                &mut next,
            )
            .unwrap();
            assert!(got.is_none(), "{text:?} proposed something");
        }
        let listing = "total 48\n".to_string()
            + &(0..7)
                .map(|i| format!("-rw-r--r-- 1 x x 0 Jan 1 00:00 f{i}"))
                .collect::<Vec<_>>()
                .join("\n");
        assert!(propose(
            &home,
            Mining {
                workspace: "w",
                job: "extract",
                when: "compaction",
                wall: &wall,
                transcript: None,
            },
            &listing,
            &mut next,
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn the_fence_keeps_the_miner_from_re_proposing_the_splice() {
        let (_dir, home) = home();
        let mut wall = BTreeSet::new();
        wall.insert(norm_claim("Open review links after pushing"));
        let mut next = ids();
        assert!(propose(
            &home,
            Mining {
                workspace: "w",
                job: "extract",
                when: "compaction",
                wall: &wall,
                transcript: None,
            },
            "Open review links after pushing.",
            &mut next,
        )
        .unwrap()
        .is_none());
        // A different claim is not fenced.
        assert!(propose(
            &home,
            Mining {
                workspace: "w",
                job: "extract",
                when: "compaction",
                wall: &wall,
                transcript: None,
            },
            "Prefer ripgrep for search.",
            &mut next,
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn a_verdict_needs_the_transcript_to_carry_the_claim() {
        assert_eq!(
            fidelity_verdict("open the links", Some("please open the links now")),
            "SUPPORTED"
        );
        assert_eq!(
            fidelity_verdict("open the links", Some("something else")),
            "NEI"
        );
        assert_eq!(fidelity_verdict("open the links", None), "NEI");
        assert_eq!(fidelity_verdict("", Some("anything")), "NEI");
    }

    #[test]
    fn only_a_supported_proposal_can_be_accepted() {
        let (_dir, home) = home();
        let mut next = ids();
        let wall = BTreeSet::new();
        let bare = Mining {
            workspace: "w",
            job: "extract",
            when: "compaction",
            wall: &wall,
            transcript: None,
        };
        let nei = propose(&home, bare, "A claim with no transcript.", &mut next)
            .unwrap()
            .unwrap();
        assert_eq!(nei["verdict"], json!("NEI"));
        let err = accept(&home, "w", nei["id"].as_str().unwrap()).unwrap_err();
        assert_eq!(err.0, "extractAccept rejected: NEI");

        // The same claim with somebody able to point at it is acceptable.
        let witnessed = Mining {
            transcript: Some("A claim with a transcript."),
            ..bare
        };
        let supported = propose(&home, witnessed, "A claim with a transcript.", &mut next)
            .unwrap()
            .unwrap();
        assert_eq!(supported["verdict"], json!("SUPPORTED"));
        let (atom, _rec) = accept(&home, "w", supported["id"].as_str().unwrap()).unwrap();
        assert_eq!(atom["text"], json!("A claim with a transcript"));
        assert_eq!(atom["kind"], json!("lesson"));
        assert_eq!(
            atom["level"],
            json!("derived"),
            "nobody typed this and a reader should see that"
        );
    }

    #[test]
    fn accepting_closes_the_proposal_without_erasing_it() {
        let (_dir, home) = home();
        let mut next = ids();
        let wall = BTreeSet::new();
        let rec = propose(
            &home,
            Mining {
                workspace: "w",
                job: "extract",
                when: "compaction",
                wall: &wall,
                transcript: Some("A claim to accept."),
            },
            "A claim to accept.",
            &mut next,
        )
        .unwrap()
        .unwrap();
        let id = rec["id"].as_str().unwrap();
        assert_eq!(list_open(&home, "w").len(), 1);
        mark_accepted(&home, "w", &rec, "atom-1").unwrap();
        assert!(list_open(&home, "w").is_empty(), "no longer open");
        // The file is append-only, so the history is still readable.
        let raw = std::fs::read_to_string(proposals_path(&home, "w")).unwrap();
        assert_eq!(raw.lines().count(), 2, "{raw}");
        assert!(raw.contains("atom-1"));
        assert!(accept(&home, "w", id).is_err(), "twice is not open");
    }

    #[test]
    fn a_day_mines_each_paragraph_once() {
        let (_dir, home) = home();
        let day = "2026-01-01";
        let path = home.archive_path("w", day);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "The first claim worth keeping.\n\nThe second claim worth keeping.\n\nshort\n",
        )
        .unwrap();
        let got = compact_day(&home, "w", Some(day), &[], None, ids()).unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(list_open(&home, "w").len(), 2);
    }

    #[test]
    fn a_live_atom_fences_the_day_that_produced_it() {
        let (_dir, home) = home();
        let day = "2026-01-01";
        let path = home.archive_path("w", day);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "The first claim worth keeping.\n").unwrap();
        let live = vec![serde_json::from_str::<Map<String, Value>>(
            r#"{"id":"a","text":"The first claim worth keeping."}"#,
        )
        .unwrap()];
        let got = compact_day(&home, "w", Some(day), &live, None, ids()).unwrap();
        assert!(got.is_empty(), "already remembered: {got:?}");
    }
}
