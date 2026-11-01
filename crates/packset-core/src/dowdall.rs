//! Dowdall (Nauru) Borda: score = 1, 1/2, 1/3, ..., 1/k.
//!
//! First place dominates. Same [`Ballot`] type as Borda.
//!
//! Reilly, Social Choice in the South Pacific, International
//! Political Science Review 2002.
//! doi:10.1177/0192512102023004002

use std::collections::HashMap;
use std::hash::Hash;

use crate::borda::Ballot;

/// Merge ballots by Dowdall. Ties break by first-seen id order.
pub fn dowdall_merge<T>(ballots: &[Ballot<T>], k: usize) -> Vec<T>
where
    T: Clone + Eq + Hash,
{
    if ballots.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut scores: HashMap<T, f64> = HashMap::new();
    let mut first_seen: Vec<T> = Vec::new();
    for ballot in ballots {
        for (pos, id) in ballot.iter().take(k).enumerate() {
            if !scores.contains_key(id) {
                first_seen.push(id.clone());
            }
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (pos as f64 + 1.0);
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
        let a = vec!["a", "b", "c"];
        let b = vec!["b", "c", "a"];
        let borda = borda_merge(&[a.clone(), b.clone()], 3);
        // Borda k-position: a: 3+1=4, b: 2+3=5, c: 1+2=3.
        assert_eq!(borda, vec!["b", "a", "c"]);
        let out = dowdall_merge(&[a, b], 3);
        // a: 1+1/3, b: 1/2+1, c: 1/3+1/2 => b=1.5, a=4/3, c=5/6.
        assert_eq!(out, vec!["b", "a", "c"]);
    }

    #[test]
    fn last_place_pile_flips_borda_not_dowdall() {
        // Three ballots, z last on each, a/b split first.
        let a = vec!["a", "c", "d", "e", "z"];
        let b = vec!["b", "c", "d", "e", "z"];
        let c = vec!["a", "b", "c", "d", "z"];
        let ballots = [a, b, c];
        // Borda (5,4,3,2,1): c=11, a=10, b=9. Mid-rank pile elects c.
        assert_eq!(borda_merge(&ballots, 5)[0], "c");
        // Dowdall: a=2, b=1.5, c=4/3. First place holds.
        assert_eq!(
            dowdall_merge(&ballots, 5),
            vec!["a", "b", "c", "d", "z", "e"]
        );
    }

    #[test]
    fn ties_break_first_seen() {
        let a = vec!["x", "y", "z"];
        let b = vec!["y", "x", "z"];
        let out = dowdall_merge(&[a, b], 3);
        // x: 1+1/2, y: 1/2+1, z: 2/3. Tie x/y: first-seen x.
        assert_eq!(out, vec!["x", "y", "z"]);
    }
}
