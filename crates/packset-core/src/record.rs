//! One atom as it sits in the store: a JSON object. Fields this module does
//! not model round-trip untouched.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::atom::{check_entity, EntityRefusal};
use crate::clock;
use crate::prose;

/// Schema name for a pack atom.
pub const SCHEMA: &str = crate::atom::SCHEMA;
/// Characters allowed in `USER.md`.
pub const USER_CAP: usize = 1375;
/// Characters allowed in a workspace `MEMORY.md`.
pub const MEMORY_CAP: usize = 2200;
/// Characters allowed in one atom's text.
pub const TEXT_SOFT_CAP: usize = 500;
/// Jaccard at or above which two atoms link.
pub const LINK_THRESHOLD: f64 = 0.3;
/// The most peers one atom names; without a cap every shared entity is a clique.
pub const LINK_MAX: usize = 8;
/// Review interval when nothing has been graded yet.
pub const DEFAULT_REVIEW_INTERVAL_S: i64 = 86_400;
/// SM-2 style ease, kept for readers of the review block.
pub const REVIEW_EASE: f64 = 2.5;
/// Starting stability, in days.
pub const DEFAULT_STABILITY: f64 = 1.0;
/// Starting difficulty, on a one to ten scale.
pub const DEFAULT_DIFFICULTY: f64 = 5.0;

/// What an atom may claim to be.
pub const KINDS: &[&str] = &[
    "voice",
    "habit",
    "cache-pointer",
    "preference",
    "lesson",
    "goal",
    "conclusion",
    "card_line",
    "summary",
    "correction",
    "belief",
    "trust",
    "persona",
    "prediction",
    "rule",
];

/// Whether the claim was stated or inferred.
pub const LEVELS: &[&str] = &["explicit", "derived"];

/// Prefixes a deed accession can open with: `deed-<kind>-<slug>` or a `sha256:`.
const DEED_PREFIXES: &[&str] = &["deed-", "sha256:"];

/// Whether an entity has the shape of a deed accession. The store is not asked.
#[must_use]
pub fn is_accession(value: &str) -> bool {
    let value = value.trim();
    if value.contains(|c: char| c.is_whitespace() || c == ',') {
        return false;
    }
    DEED_PREFIXES.iter().any(|prefix| {
        value
            .strip_prefix(*prefix)
            .is_some_and(|rest| !rest.is_empty())
    })
}

/// Quote as the error messages do: single quotes; double when the value has a
/// single quote and no double; backslash escapes when it has both. Clients
/// parse these, so the rule is part of the API.
#[must_use]
pub fn quoted(value: &str) -> String {
    let has_single = value.contains('\'');
    let has_double = value.contains('"');
    let quote = if has_single && !has_double { '"' } else { '\'' };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Why a record cannot be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomError(pub String);

impl std::fmt::Display for AtomError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AtomError {}

impl From<prose::ProseError> for AtomError {
    fn from(err: prose::ProseError) -> Self {
        Self(err.0)
    }
}

/// Zero-width and bidirectional controls, which hide text from a reader.
fn has_invisible(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32,
            0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x206f | 0xfeff)
    })
}

const SECRET_KEYS: &[&str] = &[
    "api_key", "api-key", "apikey", "secret", "password", "token",
];

/// Whether the text carries something credential-shaped: a key assigned to a
/// name, a bearer token, or an `sk-` prefix.
#[must_use]
pub fn looks_like_a_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    for key in SECRET_KEYS {
        let mut from = 0usize;
        while let Some(at) = lower[from..].find(key) {
            let idx = from + at;
            let before_is_word = idx
                .checked_sub(1)
                .and_then(|i| lower.as_bytes().get(i))
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-');
            let after = lower[idx + key.len()..].trim_start();
            if !before_is_word && (after.starts_with('=') || after.starts_with(':')) {
                return true;
            }
            from = idx + key.len();
        }
    }
    if let Some(at) = lower.find("bearer ") {
        let rest = lower[at + 7..].trim_start();
        let run = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .count();
        if run >= 8 {
            return true;
        }
    }
    let mut from = 0usize;
    while let Some(at) = lower[from..].find("sk-") {
        let idx = from + at;
        let run = lower[idx + 3..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .count();
        if run >= 8 {
            return true;
        }
        from = idx + 3;
    }
    false
}

/// Refuse text a reader cannot see or should never have been handed.
///
/// # Errors
///
/// Returns [`AtomError`] for invisible unicode or credential-shaped text.
pub fn reject_unsafe(text: &str) -> Result<(), AtomError> {
    if has_invisible(text) {
        return Err(AtomError("invisible unicode is rejected".into()));
    }
    if looks_like_a_secret(text) {
        return Err(AtomError("credential-shaped text is rejected".into()));
    }
    Ok(())
}

fn refusal_message(value: &str, why: EntityRefusal) -> String {
    match why {
        EntityRefusal::Empty => "an entity cannot be empty".to_string(),
        EntityRefusal::BarePrefix => {
            let prefix = crate::atom::ACCESSION_PREFIXES
                .iter()
                .find(|p| value.trim().starts_with(**p))
                .copied()
                .unwrap_or("");
            format!(
                "{} is a bare {} and names no deed",
                quoted(value),
                quoted(prefix)
            )
        }
        EntityRefusal::Separator(bad) => {
            format!(
                "{} carries {}, which would split the entity into two",
                quoted(value),
                quoted(&bad.to_string())
            )
        }
    }
}

/// Check a record and normalise the fields that have one legal form.
///
/// # Errors
///
/// Returns [`AtomError`] for an unknown kind or level, missing text or
/// workspace, text past the soft cap, a bad set name, an entity that opens like
/// an accession and is not one, or prose too complex for one claim.
pub fn validate(atom: &mut Map<String, Value>) -> Result<(), AtomError> {
    let kind = atom.get("kind").and_then(Value::as_str).unwrap_or("");
    if !KINDS.contains(&kind) {
        let shown = atom.get("kind").map_or("None".into(), value_repr);
        return Err(AtomError(format!("unknown atom kind: {shown}")));
    }
    let trust = kind == "trust";
    let persona = kind == "persona";
    let prediction = kind == "prediction";
    let rule = kind == "rule";
    let level = atom
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("explicit");
    if !LEVELS.contains(&level) {
        let shown = atom.get("level").map_or("None".into(), value_repr);
        return Err(AtomError(format!("unknown atom level: {shown}")));
    }
    let text = match atom.get("text").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => return Err(AtomError("atom text is required".into())),
    };
    reject_unsafe(&text)?;
    if text.chars().count() > TEXT_SOFT_CAP {
        return Err(AtomError(format!(
            "atom text exceeds soft cap {TEXT_SOFT_CAP}"
        )));
    }
    if atom
        .get("workspace")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return Err(AtomError("atom workspace is required".into()));
    }

    match atom.get("set").and_then(Value::as_str) {
        Some(raw) if !raw.is_empty() => {
            let named = crate::set_name::check(raw).map_err(AtomError)?;
            atom.insert("set".into(), Value::String(named));
        }
        _ => {
            atom.remove("set");
        }
    }

    if let Some(raw) = atom.get("entities").cloned() {
        if let Some(items) = raw.as_array() {
            let mut checked = Vec::with_capacity(items.len());
            for item in items {
                let text = item
                    .as_str()
                    .map_or_else(|| value_text(item), str::to_string);
                match check_entity(&text) {
                    Ok(kept) => checked.push(Value::String(kept.to_string())),
                    Err(why) => return Err(AtomError(refusal_message(&text, why))),
                }
            }
            atom.insert("entities".into(), Value::Array(checked));
        }
    }

    if trust {
        check_trust(atom)?;
    }
    if persona {
        check_persona(atom)?;
    }
    if prediction {
        check_prediction(atom)?;
    }
    if rule {
        check_rule(atom)?;
    }

    let report = prose::refuse(&text, prose::Role::Atom)?;
    atom.insert("prose".into(), prose_value(&report));
    Ok(())
}

/// A `prediction` atom is one voter's forecast on one issue: `issue`,
/// `agent`, and `expect`, an option name or an object of option to share.
/// The surprisingly popular rule reads these beside the ballots.
fn check_prediction(atom: &Map<String, Value>) -> Result<(), AtomError> {
    for key in ["issue", "agent"] {
        match atom.get(key).and_then(Value::as_str).map(str::trim) {
            Some(v) if !v.is_empty() => {}
            _ => return Err(AtomError(format!("prediction atom needs {key}"))),
        }
    }
    match atom.get("expect") {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(()),
        Some(Value::Object(map))
            if !map.is_empty() && map.values().all(|v| v.as_f64().is_some_and(|f| f >= 0.0)) =>
        {
            Ok(())
        }
        _ => Err(AtomError(
            "prediction atom: expect is an option or an object of option to share".into(),
        )),
    }
}

/// A `rule` atom is argv law in the pack: `pattern`, a glob over the command
/// line, and `verdict`, `deny` or `ask`. The text is the reason a reader
/// sees when the rule fires. Rules are memory too: dated, supersedable,
/// exported with the rest.
fn check_rule(atom: &Map<String, Value>) -> Result<(), AtomError> {
    match atom.get("pattern").and_then(Value::as_str).map(str::trim) {
        Some(p) if !p.is_empty() => {}
        _ => return Err(AtomError("rule atom needs a pattern".into())),
    }
    match atom.get("verdict").and_then(Value::as_str) {
        Some("deny" | "ask") => Ok(()),
        _ => Err(AtomError("rule atom: verdict is deny or ask".into())),
    }
}

/// A `persona` atom names a voter and its anchor: `name`, and `anchor` in
/// `[0, 1]`, how far the persona moves off its own ballot in a settle. The
/// text is its view, the entities the domains it speaks to.
fn check_persona(atom: &Map<String, Value>) -> Result<(), AtomError> {
    match atom.get("name").and_then(Value::as_str).map(str::trim) {
        Some(n) if !n.is_empty() => {}
        _ => return Err(AtomError("persona atom needs a name".into())),
    }
    match atom.get("anchor").and_then(Value::as_f64) {
        Some(a) if (0.0..=1.0).contains(&a) => Ok(()),
        _ => Err(AtomError(
            "persona atom: anchor must be a number in [0, 1]".into(),
        )),
    }
}

/// A `trust` atom names `from`, `to` and a `weight` in `(0, 1]`; it is one
/// row of the influence graph a consensus settles over.
fn check_trust(atom: &Map<String, Value>) -> Result<(), AtomError> {
    let name = |key: &str| -> Result<String, AtomError> {
        match atom.get(key).and_then(Value::as_str).map(str::trim) {
            Some(v) if !v.is_empty() => Ok(v.to_string()),
            _ => Err(AtomError(format!("trust atom needs {key}"))),
        }
    };
    let (from, to) = (name("from")?, name("to")?);
    if from == to {
        return Err(AtomError(
            "trust atom: from and to are the same agent".into(),
        ));
    }
    match atom.get("weight").and_then(Value::as_f64) {
        Some(w) if w > 0.0 && w <= 1.0 => Ok(()),
        _ => Err(AtomError(
            "trust atom: weight must be a number in (0, 1]".into(),
        )),
    }
}

fn value_repr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn prose_value(report: &prose::Report) -> Value {
    let mut out = Map::new();
    out.insert("words".into(), report.words.into());
    out.insert("sentences".into(), report.sentences.into());
    out.insert("grade".into(), number_or_null(report.grade));
    out.insert("ease".into(), number_or_null(report.ease));
    out.insert("adverbs".into(), report.adverbs.into());
    out.insert(
        "adverb_ratio".into(),
        number_or_null(Some(report.adverb_ratio)),
    );
    out.insert("passives".into(), report.passives.into());
    out.insert("hard_sentences".into(), report.hard_sentences.into());
    out.insert(
        "very_hard_sentences".into(),
        report.very_hard_sentences.into(),
    );
    Value::Object(out)
}

fn number_or_null(v: Option<f64>) -> Value {
    v.and_then(serde_json::Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

/// A stored timestamp field, or none when it is missing, null, or empty.
fn field_stamp<'a>(atom: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    match atom.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

/// Live set: not tombstoned, and `valid_to` missing or still open.
#[must_use]
pub fn is_live(atom: &Map<String, Value>, now: &str) -> bool {
    crate::atom::is_live(
        atom.get("tombstone")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        field_stamp(atom, "valid_to"),
        now,
    )
}

/// Live at `at`: start is `valid_from`, else `ts`, else open; `valid_to` is
/// the exclusive end as in [`is_live`].
#[must_use]
pub fn is_live_at(atom: &Map<String, Value>, at: &str) -> bool {
    crate::atom::is_live_at(
        atom.get("tombstone")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        field_stamp(atom, "valid_from").or_else(|| field_stamp(atom, "ts")),
        field_stamp(atom, "valid_to"),
        at,
    )
}

/// Review clock. A missing `due_at` is not due, and `valid_to` is not consulted.
#[must_use]
pub fn is_due(atom: &Map<String, Value>, now: &str) -> bool {
    match atom.get("due_at") {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) if s.is_empty() => false,
        Some(other) => value_text(other).as_str() <= now,
    }
}

/// The names an atom is about: the declared `entities`, else capitalised runs
/// and backtick names.
#[must_use]
pub fn entities_of(atom: &Map<String, Value>) -> BTreeSet<String> {
    if let Some(Value::Array(items)) = atom.get("entities") {
        return items
            .iter()
            .map(|item| value_text(item).trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    let text = atom.get("text").and_then(Value::as_str).unwrap_or("");
    let mut names: BTreeSet<String> = capitalized_runs(text);
    names.extend(backtick_names(text));
    names
}

/// `\b[A-Z][A-Za-z0-9]{1,}\b`
fn capitalized_runs(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let boundary = i == 0 || !is_word_byte(bytes[i - 1]);
        if boundary && bytes[i].is_ascii_uppercase() {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            // {1,} after the first character means at least two in total, and
            // the match must end on a word boundary.
            if i - start >= 2 && (i == bytes.len() || !is_word_byte(bytes[i])) {
                out.insert(text[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Text between backticks, trimmed, empties dropped.
fn backtick_names(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else { break };
        let inner = after[..close].trim();
        if !inner.is_empty() {
            out.insert(inner.to_string());
        }
        rest = &after[close + 1..];
    }
    out
}

/// One candidate neighbour: how alike it is, its id, and what it is about.
struct Candidate<'a> {
    overlap: f64,
    id: &'a str,
    /// Order among equals, from the pair rather than from the id alone.
    tie: u64,
    entities: BTreeSet<String>,
}

/// FNV-1a, written out: the order it decides is part of the stored graph.
fn fnv1a(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            hash = (hash ^ 0xff).wrapping_mul(0x0000_0100_0000_01b3);
        }
        for byte in part.as_bytes() {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// How many sorted candidates the diversifying selection looks at; the
/// selection is quadratic in this.
const LINK_POOL: usize = LINK_MAX * 8;

/// Relative-neighbourhood pruning (the HNSW neighbour heuristic): a candidate
/// earns an edge only when no kept neighbour is closer to it than the atom is.
/// Candidates arrive sorted by decreasing overlap; rejected ones fill any
/// places left, in order.
fn diversified(candidates: &[Candidate<'_>], cap: usize) -> Vec<String> {
    let mut kept: Vec<&Candidate<'_>> = Vec::with_capacity(cap);
    let mut rejected: Vec<&Candidate<'_>> = Vec::new();
    for candidate in candidates {
        if kept.len() >= cap {
            break;
        }
        let refs: Vec<&str> = candidate.entities.iter().map(String::as_str).collect();
        let spreads = kept.iter().all(|near| {
            let theirs: Vec<&str> = near.entities.iter().map(String::as_str).collect();
            candidate.overlap
                > crate::atom::entity_jaccard(refs.iter().copied(), theirs.iter().copied())
        });
        if spreads {
            kept.push(candidate);
        } else {
            rejected.push(candidate);
        }
    }
    let mut out: Vec<String> = kept.iter().map(|c| c.id.to_string()).collect();
    for filler in rejected {
        if out.len() >= cap {
            break;
        }
        out.push(filler.id.to_string());
    }
    out
}

/// Rank peers by overlap with `mine`, best first. Ties break on a hash of the
/// pair, not the id: an id tie-break lets the first-sorting atoms win every
/// tie and the graph collapses onto them.
fn ranked<'a>(
    base: &str,
    mine: &BTreeSet<String>,
    peers: impl IntoIterator<Item = (&'a str, BTreeSet<String>)>,
    threshold: f64,
) -> Vec<Candidate<'a>> {
    let mine_refs: Vec<&str> = mine.iter().map(String::as_str).collect();
    let mut scored: Vec<Candidate<'a>> = peers
        .into_iter()
        .filter_map(|(id, entities)| {
            let theirs: Vec<&str> = entities.iter().map(String::as_str).collect();
            let overlap =
                crate::atom::entity_jaccard(mine_refs.iter().copied(), theirs.iter().copied());
            (overlap >= threshold).then_some(Candidate {
                overlap,
                id,
                tie: fnv1a(&[base, id]),
                entities,
            })
        })
        .collect();
    scored.sort_by(|a, b| {
        b.overlap
            .partial_cmp(&a.overlap)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.tie.cmp(&b.tie))
            .then_with(|| a.id.cmp(b.id))
    });
    scored.truncate(LINK_POOL);
    scored
}

/// The live peers this atom is most about, at most [`LINK_MAX`], chosen by
/// `diversified`; deterministic over a corpus.
#[must_use]
pub fn link_targets(
    atom: &Map<String, Value>,
    peers: &[&Map<String, Value>],
    threshold: f64,
    now: &str,
) -> Vec<String> {
    let atom_id = atom.get("id").and_then(Value::as_str);
    let mine = entities_of(atom);
    let candidates = ranked(
        atom_id.unwrap_or_default(),
        &mine,
        peers.iter().filter_map(|other| {
            let other_id = other.get("id").and_then(Value::as_str)?;
            if Some(other_id) == atom_id || !is_live(other, now) {
                return None;
            }
            Some((other_id, entities_of(other)))
        }),
        threshold,
    );
    diversified(&candidates, LINK_MAX)
}

/// Set overlap links on `atom`, symmetrically, and return the peers to write
/// back. A peer pushed past [`LINK_MAX`] by incoming edges is re-selected by
/// the same rule, and each dropped edge goes from both ends.
pub fn apply_links(
    atom: &mut Map<String, Value>,
    live: &[Map<String, Value>],
    threshold: f64,
    now: &str,
) -> Vec<Map<String, Value>> {
    let atom_id = atom
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // Borrowed; only the peers that change are cloned.
    let peers: Vec<&Map<String, Value>> = live
        .iter()
        .filter(|other| {
            other.get("id").and_then(Value::as_str) != Some(atom_id.as_str()) && is_live(other, now)
        })
        .collect();
    let mut targets: BTreeSet<String> = link_targets(atom, &peers, threshold, now)
        .into_iter()
        .collect();

    // What every atom in play is about, so a link id can be scored without
    // going back to the store for it.
    let mut about: BTreeMap<String, BTreeSet<String>> = peers
        .iter()
        .filter_map(|peer| {
            let id = peer.get("id").and_then(Value::as_str)?;
            Some((id.to_string(), entities_of(peer)))
        })
        .collect();
    about.insert(atom_id.clone(), entities_of(atom));

    let before: BTreeMap<String, BTreeSet<String>> = peers
        .iter()
        .filter_map(|peer| {
            let id = peer.get("id").and_then(Value::as_str)?;
            Some((id.to_string(), links_of(peer)))
        })
        .collect();
    let mut after = before.clone();
    for (id, links) in &mut after {
        if targets.contains(id) {
            links.insert(atom_id.clone());
        } else {
            links.remove(&atom_id);
        }
    }

    // An id nothing live answers to is left for [`filter_live_links`].
    let mut cut: Vec<(String, String)> = Vec::new();
    for (id, links) in &after {
        if links.len() <= LINK_MAX {
            continue;
        }
        let Some(mine) = about.get(id) else { continue };
        let candidates = ranked(
            id,
            mine,
            links
                .iter()
                .filter_map(|link| Some((link.as_str(), about.get(link)?.clone()))),
            0.0,
        );
        let keep: BTreeSet<String> = diversified(&candidates, LINK_MAX).into_iter().collect();
        for link in links {
            if about.contains_key(link) && !keep.contains(link) {
                cut.push((id.clone(), link.clone()));
            }
        }
    }
    for (from, to) in cut {
        if let Some(links) = after.get_mut(&from) {
            links.remove(&to);
        }
        if to == atom_id {
            targets.remove(&from);
        } else if let Some(links) = after.get_mut(&to) {
            links.remove(&from);
        }
    }

    atom.insert(
        "links".into(),
        Value::Array(
            targets
                .iter()
                .map(|id| Value::String(id.clone()))
                .collect::<Vec<_>>(),
        ),
    );

    let mut rewritten = Vec::new();
    for other in peers {
        let Some(other_id) = other.get("id").and_then(Value::as_str) else {
            continue;
        };
        let (Some(was), Some(now_links)) = (before.get(other_id), after.get(other_id)) else {
            continue;
        };
        if was == now_links {
            continue;
        }
        let mut changed = other.clone();
        changed.insert(
            "links".into(),
            Value::Array(now_links.iter().cloned().map(Value::String).collect()),
        );
        rewritten.push(changed);
    }
    rewritten
}

/// The ids one atom links to.
/// The ids a claim links to.
pub fn links_of(atom: &Map<String, Value>) -> BTreeSet<String> {
    atom.get("links")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(value_text).collect())
        .unwrap_or_default()
}

/// Drop links pointing outside the supplied live set.
pub fn filter_live_links(atoms: &mut [Map<String, Value>]) {
    let live: BTreeSet<String> = atoms
        .iter()
        .filter_map(|a| a.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    for atom in atoms.iter_mut() {
        let kept: Vec<Value> = atom
            .get("links")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| live.contains(&value_text(item)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        atom.insert("links".into(), Value::Array(kept));
    }
}

/// How a review turned out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    /// First scheduling, or a re-schedule that is not a review.
    Initial,
    /// The atom came back.
    Recalled,
    /// It did not.
    Lapsed,
}

/// Words used to tell a rewrite from a neighbour.
pub fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Jaccard on the token sets.
#[must_use]
pub fn token_jaccard(left: &str, right: &str) -> f64 {
    let a = tokens(left);
    let b = tokens(right);
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(&b).count() as f64;
    let union = a.union(&b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// The words of a claim in order, lowercased, punctuation dropped and the
/// function words kept: the shape [`same_head`] compares.
#[must_use]
pub fn head_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The least a shared head must cover of the shorter claim.
pub const HEAD_SHARE: f64 = 0.6;
/// The least words a shared head has.
pub const HEAD_MIN: usize = 3;

/// Whether two claims say the same thing about the same subject with a
/// different object: they open with the same words for at least
/// [`HEAD_MIN`] words and [`HEAD_SHARE`] of the shorter claim, and each
/// goes on to say something the other does not. `The default fuse is
/// Borda` and `The default fuse is CombMNZ` share a head; so do `Roy
/// Rogers is married to Dale Evans` and `Roy Rogers is married to John
/// McVie`, where a set measure misses them because the object is two
/// words. Two claims that open alike and then diverge for most of their
/// length are two claims.
#[must_use]
pub fn same_head(a: &[String], b: &[String]) -> bool {
    let shared = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let shorter = a.len().min(b.len());
    if shorter == 0 || shared < HEAD_MIN || shared == a.len() || shared == b.len() {
        return false;
    }
    (shared as f64) >= HEAD_SHARE * (shorter as f64) && a[shared..] != b[shared..]
}

/// Whether `new` is a replacement for `old`, not a neighbour and not a retry.
///
/// Same kind, different text, and either an explicit `supersedes` id, a
/// `correction` that shares an entity, a rewrite of the same claim (token
/// Jaccard at least 0.6), or the same head with a new object
/// ([`same_head`]). When both carry entities they must share one; a claim
/// without entities is read by its text alone, because most claims a seat
/// remembers name none. Linked atoms about the same entities with
/// different sentences stay both live.
#[must_use]
pub fn replaces(new: &Map<String, Value>, old: &Map<String, Value>) -> bool {
    replaces_shaped(new, &Shape::of(new), old, &Shape::of(old))
}

/// What the replacement and linking rules read of a claim, computed once.
/// A claim's text never changes under its id, so a writer keeps one of
/// these per id and stops tokenising the whole pack on every write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub tokens: BTreeSet<String>,
    pub head: Vec<String>,
    pub entities: BTreeSet<String>,
}

impl Shape {
    #[must_use]
    pub fn of(atom: &Map<String, Value>) -> Self {
        let text = atom.get("text").and_then(Value::as_str).unwrap_or("");
        Self {
            tokens: tokens(text),
            head: head_tokens(text),
            entities: entities_of(atom),
        }
    }
}

/// The overlap of two token sets, as [`token_jaccard`] reads it.
#[must_use]
pub fn shape_jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// [`replaces`] with the shapes already in hand.
pub fn replaces_shaped(
    new: &Map<String, Value>,
    new_shape: &Shape,
    old: &Map<String, Value>,
    old_shape: &Shape,
) -> bool {
    if new.get("kind") != old.get("kind") {
        return false;
    }
    let new_text = new.get("text").and_then(Value::as_str).unwrap_or("");
    let old_text = old.get("text").and_then(Value::as_str).unwrap_or("");
    if new_text.is_empty() || new_text == old_text {
        return false;
    }
    let old_id = old.get("id").and_then(Value::as_str).unwrap_or("");
    if !old_id.is_empty() {
        if let Some(Value::Array(ids)) = new.get("supersedes") {
            if ids.iter().any(|v| value_text(v) == old_id) {
                return true;
            }
        }
        if let Some(Value::String(id)) = new.get("supersedes") {
            if id == old_id {
                return true;
            }
        }
    }
    let shared = new_shape
        .entities
        .intersection(&old_shape.entities)
        .next()
        .is_some();
    if !new_shape.entities.is_empty() && !old_shape.entities.is_empty() && !shared {
        return false;
    }
    if new.get("kind").and_then(Value::as_str) == Some("correction") && shared {
        return true;
    }
    shape_jaccard(&new_shape.tokens, &old_shape.tokens) >= 0.6
        || same_head(&new_shape.head, &old_shape.head)
}

/// Close the live window. Search already drops atoms whose `valid_to` is past.
pub fn close_valid_to(atom: &mut Map<String, Value>, now: &str) {
    atom.insert("valid_to".into(), Value::String(now.to_string()));
}

/// Set `due_at` from stability and difficulty: a lapse halves stability, a
/// recall grows it by how overdue the atom was. `valid_to` is left alone.
pub fn schedule_review(
    atom: &mut Map<String, Value>,
    now: &str,
    grade: Grade,
    interval_s: Option<i64>,
) {
    let previous = atom
        .get("review")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let read = |key: &str, fallback: f64| -> f64 {
        previous
            .get(key)
            .and_then(Value::as_f64)
            .filter(|v| *v != 0.0)
            .unwrap_or(fallback)
    };
    let mut stability = read("stability", DEFAULT_STABILITY);
    let mut difficulty = read("difficulty", DEFAULT_DIFFICULTY);

    let mut review = Map::new();
    let span;
    match grade {
        Grade::Lapsed => {
            difficulty = (difficulty + 0.2).clamp(1.0, 10.0);
            stability = (stability * 0.5).max(0.1);
            span = (stability.max(1.0) * 86_400.0) as i64;
            review.insert("reps".into(), 0.into());
            review.insert("interval_s".into(), span.into());
            review.insert("ease".into(), number_or_null(Some(REVIEW_EASE)));
            review.insert("stability".into(), number_or_null(Some(stability)));
            review.insert("difficulty".into(), number_or_null(Some(difficulty)));
            review.insert("last".into(), Value::String(now.to_string()));
        }
        Grade::Recalled => {
            let reps = previous
                .get("reps")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .saturating_add(1);
            let last = previous.get("last").and_then(Value::as_str).unwrap_or("");
            let elapsed = if last.is_empty() {
                0.0
            } else {
                clock::elapsed_days(last, now)
            };
            let retr = if stability > 0.0 {
                0.9f64.powf(elapsed / stability)
            } else {
                0.0
            }
            .clamp(0.01, 0.99);
            difficulty = (difficulty - 0.15).clamp(1.0, 10.0);
            stability *= 1.0 + (1.0 - difficulty / 10.0).exp() * (1.0 - retr);
            span = (stability.max(1.0) * 86_400.0) as i64;
            review.insert("reps".into(), reps.into());
            review.insert("interval_s".into(), span.into());
            review.insert("ease".into(), number_or_null(Some(REVIEW_EASE)));
            review.insert("stability".into(), number_or_null(Some(stability)));
            review.insert("difficulty".into(), number_or_null(Some(difficulty)));
            review.insert("last".into(), Value::String(now.to_string()));
        }
        Grade::Initial => {
            span = interval_s.unwrap_or(DEFAULT_REVIEW_INTERVAL_S);
            review = previous;
            review.entry("reps").or_insert_with(|| 0.into());
            review.entry("interval_s").or_insert_with(|| span.into());
            review
                .entry("ease")
                .or_insert_with(|| number_or_null(Some(REVIEW_EASE)));
            review
                .entry("stability")
                .or_insert_with(|| number_or_null(Some(DEFAULT_STABILITY)));
            review
                .entry("difficulty")
                .or_insert_with(|| number_or_null(Some(DEFAULT_DIFFICULTY)));
            review
                .entry("last")
                .or_insert_with(|| Value::String(now.to_string()));
        }
    }
    if let Some(due) = clock::shift(now, span) {
        atom.insert("due_at".into(), Value::String(due));
    }
    atom.insert("review".into(), Value::Object(review));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prediction names its issue, agent and forecast; a rule names a
    /// pattern and a verdict that is deny or ask.
    #[test]
    fn predictions_and_rules_are_checked() {
        let ok = |v: Value| validate(&mut atom(v)).is_ok();
        assert!(ok(
            json!({"kind": "prediction", "text": "a expects ship.", "workspace": "w",
            "issue": "p-1", "agent": "a", "expect": "ship"})
        ));
        assert!(ok(
            json!({"kind": "prediction", "text": "a expects ship.", "workspace": "w",
            "issue": "p-1", "agent": "a", "expect": {"ship": 0.7, "hold": 0.3}})
        ));
        assert!(!ok(
            json!({"kind": "prediction", "text": "a expects ship.", "workspace": "w",
            "issue": "p-1", "agent": "a"})
        ));
        assert!(!ok(
            json!({"kind": "prediction", "text": "a expects ship.", "workspace": "w",
            "agent": "a", "expect": "ship"})
        ));
        assert!(ok(
            json!({"kind": "rule", "text": "Never outside tmp.", "workspace": "w",
            "pattern": "rm -rf *", "verdict": "deny"})
        ));
        assert!(ok(
            json!({"kind": "rule", "text": "Ask first.", "workspace": "w",
            "pattern": "git push*", "verdict": "ask"})
        ));
        assert!(!ok(
            json!({"kind": "rule", "text": "Ask first.", "workspace": "w",
            "pattern": "rm *", "verdict": "allow"})
        ));
        assert!(!ok(
            json!({"kind": "rule", "text": "Ask first.", "workspace": "w",
            "verdict": "deny"})
        ));
    }
    use serde_json::json;

    fn atom(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn quoting_prefers_single_quotes() {
        assert_eq!(quoted("plain"), "'plain'");
        assert_eq!(quoted("it's"), "\"it's\"");
        assert_eq!(quoted("say \"hi\""), "'say \"hi\"'");
        assert_eq!(quoted("both ' and \""), "'both \\' and \"'");
        assert_eq!(quoted("a\nb"), "'a\\nb'");
    }

    #[test]
    fn a_credential_shape_is_caught_and_a_word_containing_one_is_not() {
        assert!(looks_like_a_secret("api_key=abcd1234"));
        assert!(looks_like_a_secret("Secret: hunter2"));
        assert!(looks_like_a_secret("authorization bearer abcdefghij"));
        assert!(looks_like_a_secret("use sk-abcdefghij for this"));
        // The word alone is not a leak, and neither is a longer name that
        // merely ends in one.
        assert!(!looks_like_a_secret("the token is rotated weekly"));
        assert!(!looks_like_a_secret("my_secret_sauce = onions"));
        assert!(!looks_like_a_secret("sk-short"));
    }

    #[test]
    fn invisible_unicode_is_refused() {
        assert!(reject_unsafe("plain text").is_ok());
        assert!(reject_unsafe("hidden\u{200b}text").is_err());
        assert!(reject_unsafe("\u{feff}bom").is_err());
    }

    #[test]
    fn the_shaped_rule_agrees_with_the_rule() {
        let pairs = [
            ("The default fuse is Borda.", "The default fuse is CombMNZ."),
            (
                "The Parser reads the Header.",
                "The Header comes before the Parser body.",
            ),
            (
                "A wholly different claim about nothing shared.",
                "The default fuse is CombMNZ.",
            ),
        ];
        for (a, b) in pairs {
            let old = atom(json!({"id": "o", "kind": "lesson", "text": a}));
            let new = atom(json!({"id": "n", "kind": "lesson", "text": b}));
            assert_eq!(
                replaces(&new, &old),
                replaces_shaped(&new, &Shape::of(&new), &old, &Shape::of(&old)),
                "{a} / {b}"
            );
        }
    }

    #[test]
    fn a_rewrite_of_the_same_claim_replaces_and_a_neighbour_does_not() {
        let old = atom(json!({
            "text": "The default fuse is Borda.",
            "kind": "habit",
            "entities": ["fuse", "Borda"]
        }));
        let rewrite = atom(json!({
            "text": "The default fuse is CombMNZ.",
            "kind": "habit",
            "entities": ["fuse", "CombMNZ"]
        }));
        // Same entities, different sentence: a neighbour, not a replacement.
        let neighbour = atom(json!({
            "text": "The Header comes before the Parser body.",
            "kind": "habit",
            "entities": ["Parser", "Header"]
        }));
        let first = atom(json!({
            "text": "The Parser reads the Header.",
            "kind": "habit",
            "entities": ["Parser", "Header"]
        }));
        assert!(replaces(&rewrite, &old), "shared stem, new object");
        assert!(
            !replaces(&neighbour, &first),
            "linked claims stay both live"
        );
        assert!(
            !replaces(&old, &old),
            "the same text is a retry, not a close"
        );
    }

    #[test]
    fn a_new_object_under_the_same_head_replaces_without_entities() {
        let old = atom(json!({"text": "Roy Rogers is married to Dale Evans.", "kind": "lesson"}));
        let new = atom(json!({"text": "Roy Rogers is married to John McVie.", "kind": "lesson"}));
        assert!(replaces(&new, &old), "same head, two-word object");
        let fuse_old = atom(json!({"text": "The default fuse is Borda.", "kind": "lesson"}));
        let fuse_new = atom(json!({"text": "The default fuse is CombMNZ.", "kind": "lesson"}));
        assert!(replaces(&fuse_new, &fuse_old));
        let other = atom(json!({
            "text": "The pack refuses free text where a deed accession belongs.",
            "kind": "lesson"
        }));
        let alike = atom(json!({
            "text": "The pack refuses a claim over two sentences.",
            "kind": "lesson"
        }));
        assert!(!replaces(&alike, &other), "alike openings, two claims");
        // Entities on both sides still have to meet.
        let tagged_old =
            atom(json!({"text": "The capital is Oslo.", "kind": "lesson", "entities": ["norway"]}));
        let tagged_new =
            atom(json!({"text": "The capital is Bern.", "kind": "lesson", "entities": ["swiss"]}));
        assert!(
            !replaces(&tagged_new, &tagged_old),
            "different subjects by entity"
        );
    }

    #[test]
    fn a_head_is_shared_by_order_not_by_set() {
        let h = |t: &str| head_tokens(t);
        assert!(same_head(
            &h("X is located in the continent of Asia"),
            &h("X is located in the continent of Europe")
        ));
        assert!(
            !same_head(&h("a b c"), &h("a b c")),
            "a retry is not a rewrite"
        );
        assert!(
            !same_head(&h("a b c d"), &h("a b c")),
            "a prefix of the other is not a new object"
        );
        assert!(
            !same_head(&h("the cat sat"), &h("the cat ran far away from home now")),
            "the head must cover the shorter"
        );
    }

    #[test]
    fn the_live_set_reads_the_tombstone_and_the_window() {
        let now = "2026-01-01T00:00:00.000Z";
        assert!(is_live(&atom(json!({})), now));
        assert!(is_live(&atom(json!({"valid_to": null})), now));
        assert!(is_live(&atom(json!({"valid_to": ""})), now));
        assert!(is_live(
            &atom(json!({"valid_to": "2099-01-01T00:00:00.000Z"})),
            now
        ));
        assert!(!is_live(
            &atom(json!({"valid_to": "2020-01-01T00:00:00.000Z"})),
            now
        ));
        assert!(!is_live(&atom(json!({"tombstone": true})), now));
    }

    #[test]
    fn a_dated_retrieve_reads_the_window_not_now() {
        let at = "2024-06-01T00:00:00.000Z";
        let closed = atom(json!({
            "valid_from": "2024-01-01T00:00:00.000Z",
            "valid_to": "2024-12-01T00:00:00.000Z"
        }));
        let later = atom(json!({
            "valid_from": "2025-01-01T00:00:00.000Z"
        }));
        let open = atom(json!({
            "valid_from": "2024-01-01T00:00:00.000Z"
        }));
        let by_ts = atom(json!({"ts": "2024-03-01T00:00:00.000Z"}));
        let too_new = atom(json!({"ts": "2025-01-01T00:00:00.000Z"}));
        assert!(is_live_at(&closed, at), "closed later, live then");
        assert!(!is_live(&closed, "2026-01-01T00:00:00.000Z"));
        assert!(!is_live_at(&later, at), "not yet valid");
        assert!(is_live_at(&open, at));
        assert!(
            is_live_at(&by_ts, at),
            "ts is the start when valid_from is missing"
        );
        assert!(!is_live_at(&too_new, at));
        assert!(!is_live_at(
            &atom(json!({"tombstone": true, "valid_from": "2020-01-01T00:00:00.000Z"})),
            at
        ));
    }

    #[test]
    fn the_review_clock_ignores_the_live_window() {
        let now = "2026-01-01T00:00:00.000Z";
        assert!(!is_due(&atom(json!({})), now));
        assert!(!is_due(&atom(json!({"due_at": ""})), now));
        assert!(is_due(
            &atom(json!({"due_at": "2025-01-01T00:00:00.000Z"})),
            now
        ));
        assert!(!is_due(
            &atom(json!({"due_at": "2099-01-01T00:00:00.000Z"})),
            now
        ));
        // Tombstoned and due at once: the two questions do not consult each
        // other, which is why the store asks both.
        let both = atom(json!({"tombstone": true, "due_at": "2025-01-01T00:00:00.000Z"}));
        assert!(!is_live(&both, now));
        assert!(is_due(&both, now));
    }

    #[test]
    fn a_declared_entity_list_wins_over_the_text() {
        let declared = atom(json!({"text": "The Parser reads it.", "entities": ["only-this"]}));
        assert_eq!(
            entities_of(&declared).into_iter().collect::<Vec<_>>(),
            vec!["only-this".to_string()]
        );
        // An empty declared list is still a declaration, so the text is not
        // mined behind the author's back.
        let empty = atom(json!({"text": "The Parser reads it.", "entities": []}));
        assert!(entities_of(&empty).is_empty());
    }

    #[test]
    fn validate_normalizes_the_set_name_and_drops_an_empty_one() {
        let mut a =
            atom(json!({"kind": "voice", "text": "A claim.", "workspace": "w", "set": "Review"}));
        validate(&mut a).unwrap();
        assert_eq!(a["set"], json!("review"));

        let mut b = atom(json!({"kind": "voice", "text": "A claim.", "workspace": "w", "set": ""}));
        validate(&mut b).unwrap();
        assert!(!b.contains_key("set"));
    }

    #[test]
    fn validate_leaves_fields_it_does_not_own_alone() {
        // The store is shared with another writer, so an unmodelled field has
        // to survive the round trip rather than be dropped as unknown.
        let mut a = atom(json!({
            "kind": "voice",
            "text": "A claim.",
            "workspace": "w",
            "something_else": {"nested": [1, 2, 3]}
        }));
        validate(&mut a).unwrap();
        assert_eq!(a["something_else"], json!({"nested": [1, 2, 3]}));
    }

    #[test]
    fn a_trust_atom_is_one_weighted_edge() {
        let row = |from: &str, to: &str, weight: Value| {
            atom(json!({
                "kind": "trust",
                "text": format!("{from} trusts {to}."),
                "workspace": "w",
                "from": from,
                "to": to,
                "weight": weight,
            }))
        };
        assert!(validate(&mut row("a", "b", json!(0.5))).is_ok());
        assert!(validate(&mut row("a", "b", json!(1))).is_ok());
        assert!(validate(&mut row("a", "a", json!(0.5))).is_err());
        assert!(validate(&mut row("a", "", json!(0.5))).is_err());
        assert!(validate(&mut row("a", "b", json!(0))).is_err());
        assert!(validate(&mut row("a", "b", json!(1.5))).is_err());
        assert!(validate(&mut row("a", "b", json!("0.5"))).is_err());
        let mut bare = atom(json!({"kind": "trust", "text": "a trusts b.", "workspace": "w"}));
        assert!(validate(&mut bare).is_err());
    }

    #[test]
    fn a_persona_atom_is_a_named_anchor() {
        let who = |anchor: Value| {
            atom(json!({
                "kind": "persona", "text": "Reads for the general reader.", "workspace": "w",
                "name": "broad", "anchor": anchor,
            }))
        };
        assert!(validate(&mut who(json!(0.8))).is_ok());
        assert!(validate(&mut who(json!(0))).is_ok());
        assert!(validate(&mut who(json!(1.2))).is_err());
        assert!(validate(&mut who(json!("0.5"))).is_err());
        let mut nameless =
            atom(json!({"kind": "persona", "text": "A view.", "workspace": "w", "anchor": 0.5}));
        assert!(validate(&mut nameless).is_err());
    }

    #[test]
    fn the_text_cap_counts_characters_not_bytes() {
        let wide = "\u{4e00}".repeat(TEXT_SOFT_CAP);
        let mut ok = atom(json!({"kind": "voice", "text": wide, "workspace": "w"}));
        assert!(validate(&mut ok).is_ok());
        let over = "\u{4e00}".repeat(TEXT_SOFT_CAP + 1);
        let mut bad = atom(json!({"kind": "voice", "text": over, "workspace": "w"}));
        assert!(validate(&mut bad).is_err());
    }

    #[test]
    fn scheduling_never_touches_the_live_window() {
        let mut a = atom(json!({"id": "a", "valid_to": "2099-01-01T00:00:00.000Z"}));
        schedule_review(&mut a, "2026-01-01T00:00:00.000Z", Grade::Recalled, None);
        assert_eq!(a["valid_to"], json!("2099-01-01T00:00:00.000Z"));
        assert!(a.contains_key("due_at"));
    }

    #[test]
    fn a_lapse_shortens_and_a_recall_lengthens() {
        let now = "2026-01-01T00:00:00.000Z";
        let block = json!({"reps": 3, "stability": 4.0, "difficulty": 6.0, "last": "2025-12-20T00:00:00.000Z"});
        let mut lapsed = atom(json!({"id": "a", "review": block}));
        let mut recalled = lapsed.clone();
        schedule_review(&mut lapsed, now, Grade::Lapsed, None);
        schedule_review(&mut recalled, now, Grade::Recalled, None);
        let s_lapsed = lapsed["review"]["stability"].as_f64().unwrap();
        let s_recalled = recalled["review"]["stability"].as_f64().unwrap();
        assert!(s_lapsed < 4.0, "{s_lapsed}");
        assert!(s_recalled > 4.0, "{s_recalled}");
        assert_eq!(
            lapsed["review"]["reps"],
            json!(0),
            "a lapse restarts the count"
        );
        assert_eq!(recalled["review"]["reps"], json!(4));
    }

    /// An atom about `n` things, so overlap is a set question with a knob.
    fn about(id: &str, entities: &[&str]) -> Map<String, Value> {
        atom(serde_json::json!({
            "id": id,
            "workspace": "w",
            "kind": "conclusion",
            "text": format!("Atom {id} says something."),
            "entities": entities,
        }))
    }

    fn links(atom: &Map<String, Value>) -> Vec<String> {
        links_of(atom).into_iter().collect()
    }

    /// The neighbourhood reaches both clusters, not eight from the larger.
    #[test]
    fn a_neighbourhood_spreads_over_what_an_atom_is_about() {
        let mut subject = about("mine", &["parser", "overlay"]);
        let mut peers: Vec<Map<String, Value>> = Vec::new();
        for n in 0..12 {
            peers.push(about(&format!("parser{n}"), &["parser", "shared"]));
        }
        for n in 0..12 {
            peers.push(about(&format!("overlay{n}"), &["overlay", "other"]));
        }
        apply_links(&mut subject, &peers, 0.2, &clock::utcnow());

        let chosen = links(&subject);
        assert_eq!(chosen.len(), LINK_MAX);
        assert!(
            chosen.iter().any(|id| id.starts_with("parser")),
            "{chosen:?}"
        );
        assert!(
            chosen.iter().any(|id| id.starts_with("overlay")),
            "{chosen:?}"
        );
    }

    /// Every atom that picks the same peer adds an edge to it, so the bound has
    /// to hold on the peer's side too.
    #[test]
    fn no_atom_collects_more_neighbours_than_the_bound() {
        let hub = about("hub", &["parser"]);
        let mut store: Vec<Map<String, Value>> = vec![hub];
        for n in 0..40 {
            let mut fresh = about(&format!("a{n}"), &["parser"]);
            let rewritten = apply_links(&mut fresh, &store, LINK_THRESHOLD, &clock::utcnow());
            for peer in rewritten {
                let id = peer.get("id").and_then(Value::as_str).unwrap().to_string();
                if let Some(slot) = store
                    .iter_mut()
                    .find(|s| s.get("id").and_then(Value::as_str) == Some(id.as_str()))
                {
                    *slot = peer;
                }
            }
            store.push(fresh);
        }
        for held in &store {
            let degree = links_of(held).len();
            assert!(
                degree <= LINK_MAX,
                "{} has {degree}",
                held.get("id").and_then(Value::as_str).unwrap_or("?")
            );
        }
    }

    /// A link the pack cannot walk in both directions is half an edge, and a
    /// dropped one has to go from both ends.
    #[test]
    fn dropping_an_edge_drops_it_on_both_sides() {
        let mut store: Vec<Map<String, Value>> = Vec::new();
        for n in 0..24 {
            let mut fresh = about(&format!("a{n}"), &["parser"]);
            let rewritten = apply_links(&mut fresh, &store, LINK_THRESHOLD, &clock::utcnow());
            for peer in rewritten {
                let id = peer.get("id").and_then(Value::as_str).unwrap().to_string();
                if let Some(slot) = store
                    .iter_mut()
                    .find(|s| s.get("id").and_then(Value::as_str) == Some(id.as_str()))
                {
                    *slot = peer;
                }
            }
            store.push(fresh);
        }
        let by_id: std::collections::BTreeMap<String, BTreeSet<String>> = store
            .iter()
            .map(|a| {
                (
                    a.get("id").and_then(Value::as_str).unwrap().to_string(),
                    links_of(a),
                )
            })
            .collect();
        for (id, theirs) in &by_id {
            for link in theirs {
                assert!(
                    by_id[link].contains(id),
                    "{id} links {link} but not the other way"
                );
            }
        }
    }

    /// A corpus too dense for the spread rule still gets a full neighbourhood.
    #[test]
    fn identical_atoms_still_fill_the_places() {
        let mut subject = about("mine", &["parser"]);
        let peers: Vec<Map<String, Value>> = (0..20)
            .map(|n| about(&format!("same{n}"), &["parser"]))
            .collect();
        apply_links(&mut subject, &peers, LINK_THRESHOLD, &clock::utcnow());
        assert_eq!(links(&subject).len(), LINK_MAX);
    }

    /// Ties settled by id alone collapse the graph onto whichever atoms sort
    /// first: they win every tie, and then drop the newcomer that chose them.
    #[test]
    fn a_newcomer_to_a_saturated_corpus_still_has_neighbours() {
        let mut store: Vec<Map<String, Value>> = Vec::new();
        for n in 0..60 {
            let mut fresh = about(&format!("a{n:03}"), &["parser"]);
            let rewritten = apply_links(&mut fresh, &store, LINK_THRESHOLD, &clock::utcnow());
            for peer in rewritten {
                let id = peer.get("id").and_then(Value::as_str).unwrap().to_string();
                if let Some(slot) = store
                    .iter_mut()
                    .find(|s| s.get("id").and_then(Value::as_str) == Some(id.as_str()))
                {
                    *slot = peer;
                }
            }
            store.push(fresh);
        }
        let isolated = store.iter().filter(|a| links_of(a).is_empty()).count();
        assert_eq!(
            isolated,
            0,
            "{isolated} of {} have no neighbour",
            store.len()
        );
        let edges: usize = store.iter().map(|a| links_of(a).len()).sum();
        assert!(edges > store.len() * 4, "only {edges} edges");
    }
}
