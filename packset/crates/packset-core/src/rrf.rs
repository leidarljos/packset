//! Reciprocal Rank Fusion: score(i) = sum_b 1 / (k0 + rank_b(i)).
//!
//! Rank is 1-based. Missing ranks do not contribute. No score
//! calibration. k0 is typically 60.
//!
//! Cormack, Clarke, Buettcher, Reciprocal Rank Fusion outperforms
//! Condorcet and individual Rank Learning Methods, SIGIR 2009.
//! doi:10.1145/1571941.1572114

use std::collections::HashMap;
use std::hash::Hash;

use crate::borda::Ballot;

/// Merge ballots by Reciprocal Rank Fusion. Ties break by first-seen id order.
pub fn rrf_merge<T>(ballots: &[Ballot<T>], k0: usize) -> Vec<T>
where
    T: Clone + Eq + Hash,
{
    if ballots.is_empty() {
        return Vec::new();
    }
    let k0 = k0 as f64;
    let mut scores: HashMap<T, f64> = HashMap::new();
    let mut first_seen: Vec<T> = Vec::new();
    for ballot in ballots {
        for (pos, id) in ballot.iter().enumerate() {
            if !scores.contains_key(id) {
                first_seen.push(id.clone());
            }
            let rank = (pos + 1) as f64;
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (k0 + rank);
        }
    }
    let mut ranked = first_seen;
    ranked.sort_by(|a, b| scores[b].total_cmp(&scores[a]));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::borda::borda_merge;

    #[test]
    fn two_lists_of_three() {
        let a = vec!["x", "y", "z"];
        let b = vec!["y", "x", "z"];
        let out = rrf_merge(&[a, b], 60);
        // x: 1/61 + 1/62, y: 1/62 + 1/61, z: 2/63. Tie x/y: first-seen x.
        assert_eq!(out, vec!["x", "y", "z"]);
    }

    #[test]
    fn omitted_rank_lifts_z_above_borda_last() {
        let a = vec!["x", "y", "z"];
        let b = vec!["y", "x", "z"];
        let c = vec!["z"];
        let borda = borda_merge(&[a.clone(), b.clone(), c.clone()], 3);
        // Borda last-place dump: z stays last on the three-way tie.
        assert_eq!(borda, vec!["x", "y", "z"]);
        let out = rrf_merge(&[a, b, c], 60);
        assert_eq!(out[0], "z");
    }
}
