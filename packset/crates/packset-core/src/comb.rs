//! CombSUM / CombMNZ score fusion.
//!
//! Each list is min-max normalized. CombSUM sums those values; a
//! missing list contributes 0. CombMNZ multiplies the sum by the
//! number of lists that retrieved the id.
//!
//! Fox, Shaw, Combination of Multiple Searches, TREC-2, 1993.
//! doi:10.6028/nist.sp.500-215.vt
//! Lee, Analyses of multiple evidence combination, SIGIR 1997.
//! doi:10.1145/258525.258587
//! Montague, Aslam, Relevance score normalization for metasearch,
//! CIKM 2001. doi:10.1145/502585.502657

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// One scored list. The f64 is the raw score; order is first-seen only.
pub type ScoredBallot<T> = Vec<(T, f64)>;

enum Comb {
    Sum,
    Mnz,
}

/// Merge scored lists by CombSUM. Ties break by first-seen id order.
pub fn combsum_merge<T>(ballots: &[ScoredBallot<T>]) -> Vec<T>
where
    T: Clone + Eq + Hash,
{
    comb_merge(ballots, Comb::Sum)
}

/// Merge scored lists by CombMNZ. Ties break by first-seen id order.
pub fn combmnz_merge<T>(ballots: &[ScoredBallot<T>]) -> Vec<T>
where
    T: Clone + Eq + Hash,
{
    comb_merge(ballots, Comb::Mnz)
}

fn comb_merge<T>(ballots: &[ScoredBallot<T>], kind: Comb) -> Vec<T>
where
    T: Clone + Eq + Hash,
{
    if ballots.is_empty() {
        return Vec::new();
    }
    let mut scores: HashMap<T, f64> = HashMap::new();
    let mut support: HashMap<T, usize> = HashMap::new();
    let mut first_seen: Vec<T> = Vec::new();
    for ballot in ballots {
        for (id, norm) in normalize_list(ballot) {
            if !scores.contains_key(&id) {
                first_seen.push(id.clone());
            }
            *scores.entry(id.clone()).or_insert(0.0) += norm;
            *support.entry(id).or_insert(0) += 1;
        }
    }
    if matches!(kind, Comb::Mnz) {
        for (id, score) in scores.iter_mut() {
            *score *= *support.get(id).unwrap_or(&0) as f64;
        }
    }
    let mut ranked = first_seen;
    ranked.sort_by(|a, b| scores[b].total_cmp(&scores[a]));
    ranked
}

fn normalize_list<T>(ballot: &ScoredBallot<T>) -> Vec<(T, f64)>
where
    T: Clone + Eq + Hash,
{
    let mut seen = HashSet::new();
    let mut items: Vec<(T, f64)> = Vec::new();
    for (id, raw) in ballot {
        if !raw.is_finite() || !seen.insert(id.clone()) {
            continue;
        }
        items.push((id.clone(), *raw));
    }
    if items.is_empty() {
        return Vec::new();
    }
    let min = items.iter().map(|(_, s)| *s).fold(f64::INFINITY, f64::min);
    let max = items
        .iter()
        .map(|(_, s)| *s)
        .fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;
    if span == 0.0 {
        return items.into_iter().map(|(id, _)| (id, 1.0)).collect();
    }
    items
        .into_iter()
        .map(|(id, s)| (id, (s - min) / span))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combsum_two_lists_tie_after_minmax() {
        let a = vec![("x", 1.0), ("y", 0.2)];
        let b = vec![("x", 0.1), ("y", 0.9)];
        let out = combsum_merge(&[a, b]);
        // After min-max: x = 1+0, y = 0+1. Tie, first-seen x.
        assert_eq!(out, vec!["x", "y"]);
    }

    #[test]
    fn combmnz_lifts_double_hit_over_single_list_champion() {
        let a = vec![("c", 1.0), ("d", 0.4), ("low", 0.0)];
        let b = vec![("hi", 1.0), ("d", 0.25), ("lo", 0.0)];
        // CombSUM: c = 1.0, d = 0.4+0.25 = 0.65. Champion stays first.
        let sum = combsum_merge(&[a.clone(), b.clone()]);
        assert_eq!(sum[0], "c");
        // CombMNZ: c = 1.0*1, d = 0.65*2 = 1.30. Support lifts d.
        let mnz = combmnz_merge(&[a, b]);
        assert_eq!(mnz[0], "d");
    }

    #[test]
    fn missing_list_is_zero() {
        let a = vec![("only", 1.0), ("pad", 0.0)];
        let b: ScoredBallot<&str> = vec![];
        let out = combsum_merge(&[a, b]);
        assert_eq!(out[0], "only");
    }
}
