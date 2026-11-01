//! Paper A fixture benches: LongMemEval_S, MemoryAgentBench, MemConflict.
//!
//! Writes go through `admit_seat_write`. A raw line is a refusal. Hit@1 is
//! lexical overlap of the question against admitted claims only. These rows
//! are fixtures, not a published SOTA table.

use crate::extract::admit_seat_write;
use crate::search::atom_tokens;
use serde_json::Value;
use std::collections::BTreeMap;

/// The three named benches the paper eval gate asks for.
pub const BENCHES: &[&str] = &["LongMemEval_S", "MemoryAgentBench", "MemConflict"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BenchRow {
    pub admitted: usize,
    pub refused: usize,
    pub asked: usize,
    pub hit: usize,
}

impl BenchRow {
    #[must_use]
    pub fn hit_rate(&self) -> Option<f64> {
        if self.asked == 0 {
            None
        } else {
            Some(self.hit as f64 / self.asked as f64)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PaperATable {
    pub rows: BTreeMap<String, BenchRow>,
}

impl PaperATable {
    #[must_use]
    pub fn new() -> Self {
        let mut rows = BTreeMap::new();
        for name in BENCHES {
            rows.insert((*name).to_string(), BenchRow::default());
        }
        Self { rows }
    }

    /// Ingest one fixture object: `{bench, write, question, answers}`.
    pub fn add(&mut self, rec: &Value) {
        let bench = rec["bench"].as_str().unwrap_or("").to_string();
        let row = self.rows.entry(bench).or_default();
        let write = rec["write"].as_str().unwrap_or("");
        let admitted = admit_seat_write(write);
        if admitted.is_none() {
            row.refused += 1;
            return;
        }
        row.admitted += 1;
        let question = rec["question"].as_str().unwrap_or("");
        if question.is_empty() {
            return;
        }
        row.asked += 1;
        let answers: Vec<String> = rec["answers"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let claim = match admitted {
            Some(
                crate::extract::SeatWrite::Lesson(c) | crate::extract::SeatWrite::Preference(c),
            ) => c,
            Some(crate::extract::SeatWrite::Accept(_)) => return,
            None => return,
        };
        let claim_rec = serde_json::json!({"text": claim});
        let q_rec = serde_json::json!({"text": question});
        let hay = atom_tokens(claim_rec.as_object().unwrap());
        let qtoks = atom_tokens(q_rec.as_object().unwrap());
        let overlap = !qtoks.is_empty() && qtoks.iter().any(|t| hay.contains(t));
        let span = answers.iter().any(|a| {
            let a = a.to_ascii_lowercase();
            !a.is_empty() && claim.to_ascii_lowercase().contains(&a)
        });
        if overlap && span {
            row.hit += 1;
        }
    }

    #[must_use]
    pub fn table(&self) -> String {
        let mut s = String::from(
            "| bench | admitted | refused | asked | hit@1 |\n|---|---:|---:|---:|---:|\n",
        );
        for name in BENCHES {
            let row = self.rows.get(*name).cloned().unwrap_or_default();
            match row.hit_rate() {
                Some(r) => s.push_str(&format!(
                    "| {name} | {} | {} | {} | {:.3} |\n",
                    row.admitted, row.refused, row.asked, r
                )),
                None => s.push_str(&format!(
                    "| {name} | {} | {} | 0 | — |\n",
                    row.admitted, row.refused
                )),
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn three_named_benches_produce_a_table() {
        let mut t = PaperATable::new();
        t.add(&json!({
            "bench": "LongMemEval_S",
            "write": "Remember: the user's dog is named Rex",
            "question": "What is the dog named?",
            "answers": ["Rex"]
        }));
        t.add(&json!({
            "bench": "MemoryAgentBench",
            "write": "The user lives in Berlin.",
            "question": "Where does the user live?",
            "answers": ["Berlin"]
        }));
        t.add(&json!({
            "bench": "MemConflict",
            "write": "Remember: the capital of Germany is Berlin now",
            "question": "What is the capital of Germany?",
            "answers": ["Berlin"]
        }));
        let table = t.table();
        assert!(table.contains("LongMemEval_S"), "{table}");
        assert!(table.contains("MemoryAgentBench"), "{table}");
        assert!(table.contains("MemConflict"), "{table}");
        assert_eq!(t.rows["LongMemEval_S"].hit, 1);
        assert_eq!(t.rows["MemoryAgentBench"].refused, 1);
        assert_eq!(t.rows["MemoryAgentBench"].asked, 0);
        assert_eq!(t.rows["MemConflict"].hit, 1);
    }
}
