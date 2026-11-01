//! Host Borda: score = k - position. First place is k-1.
//!
//! Fuse slot. Other fuse voters take the same [`Ballot`] type.

use std::collections::HashMap;
use std::hash::Hash;

/// One ranked list. Position 0 is first.
pub type Ballot<T> = Vec<T>;

/// Merge ballots by Borda. Ties break by first-seen id order.
pub fn borda_merge<T>(ballots: &[Ballot<T>], k: usize) -> Vec<T>
where
    T: Clone + Eq + Hash,
{
    if ballots.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut scores: HashMap<T, i64> = HashMap::new();
    let mut first_seen: Vec<T> = Vec::new();
    let cap = k as i64;
    for ballot in ballots {
        for (pos, id) in ballot.iter().take(k).enumerate() {
            if !scores.contains_key(id) {
                first_seen.push(id.clone());
            }
            *scores.entry(id.clone()).or_insert(0) += cap - pos as i64;
        }
    }
    let mut ranked = first_seen;
    ranked.sort_by(|a, b| {
        scores
            .get(b)
            .cmp(&scores.get(a))
            .then_with(|| scores.get(a).cmp(&scores.get(b)))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_lists_of_three() {
        let a = vec!["x", "y", "z"];
        let b = vec!["y", "x", "z"];
        let out = borda_merge(&[a, b], 3);
        // x: 2+1=3, y: 1+2=3, z: 0+0=0. Tie x/y: first-seen x then y.
        assert_eq!(out, vec!["x", "y", "z"]);
        let a = vec!["a", "b", "c"];
        let b = vec!["b", "c", "a"];
        let out = borda_merge(&[a, b], 3);
        // a: 2+0=2, b: 1+2=3, c: 0+1=1
        assert_eq!(out, vec!["b", "a", "c"]);
    }
}
