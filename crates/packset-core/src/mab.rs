//! MemoryAgentBench write protocol: Remember / Prefer / Accept only.
//!
//! The four competencies are always named. A raw record is a refusal, not
//! a zero. Hit rates are only over admitted writes. Retrieval is lexical
//! over the claims those writes produced.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::bm25::Index;
use crate::extract::{admit_seat_write, SeatWrite};
use crate::search::atom_tokens;

/// The benchmark's four competencies, in the paper's order.
pub const COMPETENCIES: &[&str] = &[
    "Accurate_Retrieval",
    "Test_Time_Learning",
    "Long_Range_Understanding",
    "Conflict_Resolution",
];

/// One competency's asked/hit counts. `asked == 0` means the split was
/// not run, not that the system scored zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Competency {
    pub asked: usize,
    pub hit: usize,
}

impl Competency {
    #[must_use]
    pub fn rate(&self) -> Option<f64> {
        if self.asked == 0 {
            None
        } else {
            Some(self.hit as f64 / self.asked as f64)
        }
    }
}

/// Honest report: refusals are counted, not scored as misses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProtocolReport {
    pub admitted: usize,
    pub refused: usize,
    pub by_competency: BTreeMap<String, Competency>,
}

impl ProtocolReport {
    #[must_use]
    pub fn new() -> Self {
        let mut by_competency = BTreeMap::new();
        for name in COMPETENCIES {
            by_competency.insert((*name).to_string(), Competency::default());
        }
        Self {
            admitted: 0,
            refused: 0,
            by_competency,
        }
    }

    /// Attempt to ingest one line. Raw context refuses.
    pub fn ingest(&mut self, text: &str) -> Option<SeatWrite> {
        match admit_seat_write(text) {
            Some(w) => {
                self.admitted += 1;
                Some(w)
            }
            None => {
                self.refused += 1;
                None
            }
        }
    }

    pub fn mark(&mut self, competency: &str, hit: bool) {
        let row = self
            .by_competency
            .entry(competency.to_string())
            .or_default();
        row.asked += 1;
        if hit {
            row.hit += 1;
        }
    }

    #[must_use]
    pub fn refusal_rate(&self) -> Option<f64> {
        let n = self.admitted + self.refused;
        if n == 0 {
            None
        } else {
            Some(self.refused as f64 / n as f64)
        }
    }

    /// Markdown table. Empty competencies print `—`, not 0.000.
    #[must_use]
    pub fn table(&self) -> String {
        let mut s = String::from("| competency | asked | hit | rate |\n|---|---:|---:|---:|\n");
        for name in COMPETENCIES {
            let row = self.by_competency.get(*name).cloned().unwrap_or_default();
            match row.rate() {
                Some(r) => s.push_str(&format!(
                    "| {name} | {} | {} | {:.3} |\n",
                    row.asked, row.hit, r
                )),
                None => s.push_str(&format!("| {name} | 0 | — | — |\n")),
            }
        }
        let n = self.admitted + self.refused;
        let refuse = self
            .refusal_rate()
            .map(|r| format!("{r:.3}"))
            .unwrap_or_else(|| "—".into());
        s.push_str(&format!(
            "| refusal | {n} | {} | {refuse} |\n",
            self.refused
        ));
        s
    }

    /// Offer one unit. A Remember / Prefer / Accept line is admitted as
    /// written. Anything else is refused, then offered again as
    /// `Remember: ` of the same words. Accept produces no searchable claim.
    pub fn keep(&mut self, unit: &str) -> Option<String> {
        let line = one_line(unit);
        if line.is_empty() {
            return None;
        }
        if admit_seat_write(&line).is_some() {
            return claim_of(self.ingest(&line)?);
        }
        self.ingest(&line);
        claim_of(self.ingest(&format!("Remember: {line}"))?)
    }

    /// Rank admitted claims for one question and mark the competency.
    pub fn ask(&mut self, competency: &str, question: &str, answers: &[String], claims: &[String]) {
        self.mark(competency, bearing(claims, question, answers, TOP));
    }

    /// One MemoryAgentBench-shaped record: chunk the context, keep each
    /// unit, ask every question of the admitted claims.
    pub fn run_record(&mut self, competency: &str, rec: &Value) {
        let source = rec["metadata"]["source"].as_str().unwrap_or("");
        let context = rec["context"].as_str().unwrap_or("");
        let mut claims = Vec::new();
        for unit in units(source, context) {
            if let Some(claim) = self.keep(&unit) {
                claims.push(claim);
            }
        }
        let questions = strings(&rec["questions"]);
        let answers: Vec<Vec<String>> = match &rec["answers"] {
            Value::Array(items) => items.iter().map(strings).collect(),
            other => vec![strings(other)],
        };
        for (i, question) in questions.iter().enumerate() {
            if question.is_empty() {
                continue;
            }
            let ans = answers.get(i).cloned().unwrap_or_default();
            self.ask(competency, question, &ans, &claims);
        }
    }

    /// JSONL, one record a line, competency taken from the filename stem
    /// unless the record names one.
    pub fn run_jsonl(&mut self, path: &Path) -> Result<(), String> {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let rec: Value =
                serde_json::from_str(line).map_err(|e| format!("{}: {e}", path.display()))?;
            let competency = rec["competency"].as_str().unwrap_or(&stem);
            self.run_record(competency, &rec);
        }
        Ok(())
    }

    /// Every `{Name}.jsonl` whose stem is a named competency.
    pub fn run_dir(&mut self, dir: &Path) -> Result<(), String> {
        for name in COMPETENCIES {
            let path = dir.join(format!("{name}.jsonl"));
            if path.is_file() {
                self.run_jsonl(&path)?;
            }
        }
        Ok(())
    }
}

/// Hits handed to the reader. The pack's own seam is five claims.
pub const TOP: usize = 5;
/// Words per chunk. The benchmark chunks by 512 model tokens; a token is
/// about three quarters of a word, so this is the same span in words.
pub const CHUNK_WORDS: usize = 384;

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn claim_of(write: SeatWrite) -> Option<String> {
    match write {
        SeatWrite::Lesson(c) | SeatWrite::Preference(c) => Some(c),
        SeatWrite::Accept(_) => None,
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::Array(items) => items.iter().flat_map(strings).collect(),
        Value::String(s) => vec![s.clone()],
        Value::Null => Vec::new(),
        other => vec![other.to_string()],
    }
}

/// A fact list is one document a fact; everything else is chunked by words.
pub fn units(source: &str, context: &str) -> Vec<String> {
    if source.starts_with("factconsolidation") {
        return context
            .lines()
            .map(str::trim)
            .filter(|l| l.chars().next().is_some_and(|c| c.is_ascii_digit()) && l.contains(". "))
            .map(str::to_string)
            .collect();
    }
    context
        .split_whitespace()
        .collect::<Vec<_>>()
        .chunks(CHUNK_WORDS)
        .map(|w| w.join(" "))
        .filter(|s| !s.is_empty())
        .collect()
}

fn normal(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn tokens_of(text: &str) -> Vec<String> {
    atom_tokens(serde_json::json!({"text": text}).as_object().unwrap())
}

/// Whether an answer string appears in the top `k` claims, ranked by BM25
/// of the question against admitted text only.
pub fn bearing(claims: &[String], question: &str, answers: &[String], k: usize) -> bool {
    if claims.is_empty() {
        return false;
    }
    let docs: Vec<Vec<String>> = claims.iter().map(|c| tokens_of(c)).collect();
    let index = Index::build(docs.iter().map(Vec::as_slice));
    let mut ranked = index.score(&tokens_of(question));
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let hay = ranked
        .into_iter()
        .take(k)
        .map(|(i, _)| format!(" {} ", normal(&claims[i])))
        .collect::<String>();
    answers
        .iter()
        .map(|a| normal(a))
        .filter(|a| !a.is_empty())
        .any(|a| hay.contains(&a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_context_is_a_refusal_not_a_zero() {
        let mut p = ProtocolReport::new();
        assert!(p
            .ingest("The user lives in Berlin and likes tea.")
            .is_none());
        assert_eq!(p.refused, 1);
        assert_eq!(p.admitted, 0);
        assert!(p.ingest("Remember: pin the review set").is_some());
        assert_eq!(p.admitted, 1);
        assert_eq!(p.by_competency.len(), 4);
        assert!(p.by_competency["Test_Time_Learning"].rate().is_none());
        let table = p.table();
        assert!(
            table.contains("| Test_Time_Learning | 0 | — | — |"),
            "{table}"
        );
        assert!(table.contains("| refusal |"), "{table}");
    }

    #[test]
    fn a_hit_is_only_counted_on_an_admitted_write() {
        let mut p = ProtocolReport::new();
        p.ingest("The user lives in Berlin.");
        p.mark("Accurate_Retrieval", false);
        p.ingest("Remember: Berlin is the capital of Germany now");
        p.mark("Accurate_Retrieval", true);
        let row = &p.by_competency["Accurate_Retrieval"];
        assert_eq!(row.asked, 2);
        assert_eq!(row.hit, 1);
    }

    fn fixture_report() -> ProtocolReport {
        let mut p = ProtocolReport::new();
        let raw = include_str!("../../../data/mab_protocol.jsonl");
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let rec: Value = serde_json::from_str(line).expect("fixture json");
            let competency = rec["competency"].as_str().expect("competency");
            p.run_record(competency, &rec);
        }
        p
    }

    #[test]
    fn protocol_fixture_names_all_four_and_refusal() {
        let p = fixture_report();
        let table = p.table();
        for name in COMPETENCIES {
            assert!(table.contains(&format!("| {name} |")), "{table}");
            let row = &p.by_competency[*name];
            assert!(row.asked > 0, "{name} asked");
        }
        assert!(table.contains("| refusal |"), "{table}");
        assert!(p.refused > 0, "raw context must refuse");
        assert!(p.admitted > 0, "Remember Prefer Accept must admit");
        assert_eq!(p.by_competency["Accurate_Retrieval"].hit, 1);
        assert_eq!(p.by_competency["Test_Time_Learning"].hit, 1);
        assert_eq!(p.by_competency["Long_Range_Understanding"].hit, 1);
        assert_eq!(p.by_competency["Conflict_Resolution"].hit, 1);
        // The tea line is refused as raw, then kept as Remember, and the
        // dog question still misses: that name was never written. Berlin hits.
        assert_eq!(p.by_competency["Accurate_Retrieval"].asked, 2);
        assert_eq!(p.by_competency["Accurate_Retrieval"].hit, 1);
    }

    #[test]
    fn prefer_and_accept_are_seat_writes_and_raw_is_not() {
        let mut p = ProtocolReport::new();
        assert!(p.keep("Prefer: conventional commits always").is_some());
        assert!(p.keep("Accept: ab12cd").is_none());
        assert_eq!(p.admitted, 2);
        let claim = p.keep("The user lives in Berlin now.");
        assert!(claim.as_deref().is_some_and(|c| c.contains("Berlin")));
        assert_eq!(p.refused, 1);
    }
}
