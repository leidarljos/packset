//! Kemeny-Young: ranking of minimum total Kendall-tau to the ballots.
//!
//! Exact search over permutations of the union of the top-k ids.
//! Wider than [`EXACT_MAX`] falls back to Borda. Equal distance
//! keeps the first-seen order.
//!
//! Young and Levenglick, A Consistent Extension of Condorcet's
//! Election Principle, SIAM J. Appl. Math. 1978.
//! doi:10.1137/0135023
//! Dwork, Kumar, Naor, Sivakumar, Rank Aggregation Methods for the
//! Web, WWW 2001. doi:10.1145/371920.372165

use std::collections::HashMap;
use std::hash::Hash;

use crate::borda::{borda_merge, Ballot};

/// Largest candidate set searched exactly. `8!` is 40320.
pub const EXACT_MAX: usize = 8;

/// Merge ballots by Kemeny-Young. Ties break by first-seen id order.
pub fn kemeny_merge<T>(ballots: &[Ballot<T>], k: usize) -> Vec<T>
where
    T: Clone + Eq + Hash,
{
    if ballots.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut first_seen: Vec<T> = Vec::new();
    let mut index: HashMap<T, usize> = HashMap::new();
    for ballot in ballots {
        for id in ballot.iter().take(k) {
            if !index.contains_key(id) {
                index.insert(id.clone(), first_seen.len());
                first_seen.push(id.clone());
            }
        }
    }
    let n = first_seen.len();
    if n == 0 {
        return Vec::new();
    }
    if n > EXACT_MAX {
        return borda_merge(ballots, k);
    }

    let pairwise = pref_matrix(ballots, k, &index, n);
    let mut perm: Vec<usize> = (0..n).collect();
    let mut best = perm.clone();
    let mut best_dist = kendall_cost(&perm, &pairwise);
    while next_permutation(&mut perm) {
        let dist = kendall_cost(&perm, &pairwise);
        if dist < best_dist {
            best_dist = dist;
            best.clone_from(&perm);
        }
    }
    best.into_iter().map(|i| first_seen[i].clone()).collect()
}

/// `pairwise[i][j]` is the number of ballots that rank i above j.
/// Present ids beat omitted ids on that ballot.
fn pref_matrix<T>(
    ballots: &[Ballot<T>],
    k: usize,
    index: &HashMap<T, usize>,
    n: usize,
) -> Vec<Vec<u32>>
where
    T: Clone + Eq + Hash,
{
    let mut pairwise = vec![vec![0u32; n]; n];
    for ballot in ballots {
        let mut pos = vec![None; n];
        for (p, id) in ballot.iter().take(k).enumerate() {
            if let Some(&i) = index.get(id) {
                if pos[i].is_none() {
                    pos[i] = Some(p);
                }
            }
        }
        for i in 0..n {
            for j in 0..n {
                if i == j {
                    continue;
                }
                match (pos[i], pos[j]) {
                    (Some(pi), Some(pj)) if pi < pj => pairwise[i][j] += 1,
                    (Some(_), None) => pairwise[i][j] += 1,
                    _ => {}
                }
            }
        }
    }
    pairwise
}

fn kendall_cost(perm: &[usize], pairwise: &[Vec<u32>]) -> u64 {
    let mut dist = 0u64;
    for a in 0..perm.len() {
        for b in (a + 1)..perm.len() {
            dist += u64::from(pairwise[perm[b]][perm[a]]);
        }
    }
    dist
}

fn next_permutation(a: &mut [usize]) -> bool {
    if a.len() < 2 {
        return false;
    }
    let mut i = a.len() - 1;
    while i > 0 && a[i - 1] >= a[i] {
        i -= 1;
    }
    if i == 0 {
        return false;
    }
    let mut j = a.len() - 1;
    while a[j] <= a[i - 1] {
        j -= 1;
    }
    a.swap(i - 1, j);
    a[i..].reverse();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_lists_first_seen() {
        let a = vec!["a", "b", "c"];
        let b = vec!["b", "a", "c"];
        let out = kemeny_merge(&[a, b], 3);
        // Both [a,b,c] and [b,a,c] have Kendall-tau sum 1. First-seen a.
        assert_eq!(out, vec!["a", "b", "c"]);
        let a = vec!["b", "a", "c"];
        let b = vec!["a", "b", "c"];
        let out = kemeny_merge(&[a, b], 3);
        assert_eq!(out, vec!["b", "a", "c"]);
    }

    #[test]
    fn cycle_of_three_pins_first_seen() {
        let a = vec!["a", "b", "c"];
        let b = vec!["b", "c", "a"];
        let c = vec!["c", "a", "b"];
        let out = kemeny_merge(&[a, b, c], 3);
        // Cycle a>b, b>c, c>a. Three Kemeny optima; first-seen [a,b,c].
        assert_eq!(out, vec!["a", "b", "c"]);
    }

    #[test]
    fn wider_than_exact_falls_back_to_borda() {
        let a: Vec<String> = (0..9).map(|i| format!("i{i}")).collect();
        let mut b = a.clone();
        b.reverse();
        let ballots = [a, b];
        assert_eq!(kemeny_merge(&ballots, 9), borda_merge(&ballots, 9));
    }

    #[test]
    fn empty_or_zero_k() {
        let a = vec!["a", "b"];
        assert!(kemeny_merge::<&str>(&[], 3).is_empty());
        assert!(kemeny_merge(&[a], 0).is_empty());
    }
}
