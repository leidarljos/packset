//! MemoryAgentBench write protocol: Remember / Prefer / Accept only.
//!
//! The four competencies are always named. A raw record is a refusal, not
//! a zero. Hit rates are only over admitted writes.

use std::collections::BTreeMap;

use crate::extract::{admit_seat_write, SeatWrite};

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_context_is_a_refusal_not_a_zero() {
        let mut p = ProtocolReport::new();
        assert!(p.ingest("The user lives in Berlin and likes tea.").is_none());
        assert_eq!(p.refused, 1);
        assert_eq!(p.admitted, 0);
        assert!(p.ingest("Remember: pin the review set").is_some());
        assert_eq!(p.admitted, 1);
        assert_eq!(p.by_competency.len(), 4);
        assert!(p.by_competency["Test_Time_Learning"].rate().is_none());
        let table = p.table();
        assert!(table.contains("| Test_Time_Learning | 0 | — | — |"), "{table}");
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
}
