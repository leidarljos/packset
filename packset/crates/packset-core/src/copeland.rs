//! Copeland: pairwise wins minus losses.
//!
//! d[i,j] is how many ballots rank i above j (top-k). i beats j
//! when d[i,j] > d[j,i]. Score is wins minus losses. No path
//! step. Ties break first-seen.
//!
//! Nurmi, Comparing Voting Systems, 1987.
//! doi:10.1007/978-94-009-3985-1
//! Young, Condorcet's Theory of Voting, APSR 1988.
//! doi:10.2307/1961757
//! Aslam and Montague, Models for metasearch, SIGIR 2001.
//! doi:10.1145/383952.384007

use std::collections::HashMap;
use std::hash::Hash;

use crate::borda::Ballot;

/// Merge ballots by Copeland. Ties break by first-seen id order.
pub fn copeland_merge<T>(ballots: &[Ballot<T>], k: usize) -> Vec<T>
where
    T: Clone + Eq + Hash,
{
    let (mut ranked, scores, index) = copeland_table(ballots, k);
    ranked.sort_by(|a, b| {
        let i = index[a];
        let j = index[b];
        scores[j].cmp(&scores[i]).then_with(|| i.cmp(&j))
    });
    ranked
}

/// First-seen ids, Copeland scores, and id-to-index.
fn copeland_table<T>(ballots: &[Ballot<T>], k: usize) -> (Vec<T>, Vec<i64>, HashMap<T, usize>)
where
    T: Clone + Eq + Hash,
{
    if ballots.is_empty() || k == 0 {
        return (Vec::new(), Vec::new(), HashMap::new());
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
        return (Vec::new(), Vec::new(), HashMap::new());
    }

    let mut d = vec![vec![0i64; n]; n];
    for ballot in ballots {
        let mut prefix = Vec::new();
        let mut on_ballot = vec![false; n];
        for id in ballot.iter().take(k) {
            let Some(&i) = index.get(id) else {
                continue;
            };
            if on_ballot[i] {
                continue;
            }
            on_ballot[i] = true;
            prefix.push(i);
        }
        for (rank, &i) in prefix.iter().enumerate() {
            for &j in prefix.iter().skip(rank + 1) {
                d[i][j] += 1;
            }
            for j in 0..n {
                if !on_ballot[j] {
                    d[i][j] += 1;
                }
            }
        }
    }

    let mut scores = vec![0i64; n];
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            if d[i][j] > d[j][i] {
                scores[i] += 1;
            } else if d[i][j] < d[j][i] {
                scores[i] -= 1;
            }
        }
    }
    (first_seen, scores, index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::borda::borda_merge;

    fn condorcet_mid_borda() -> [Ballot<&'static str>; 5] {
        [
            vec!["a", "c", "d", "e", "f"],
            vec!["a", "c", "d", "e", "f"],
            vec!["a", "c", "d", "e", "f"],
            vec!["c", "d", "e", "f", "a"],
            vec!["d", "e", "f", "c", "a"],
        ]
    }

    #[test]
    fn condorcet_winner_matches_hand_pairwise() {
        let ballots = condorcet_mid_borda();
        // Borda (4,3,2,1,0): c=14, d=13, a=12. Mid-rank pile elects c.
        assert_eq!(borda_merge(&ballots, 5), vec!["c", "d", "a", "e", "f"]);
        // Hand pairwise, 5 ballots, missing treated as last:
        // a vs c/d/e/f: 3-2. a is the Condorcet winner.
        // c vs d/e/f: 4-1. d vs e/f: 5-0. e vs f: 5-0.
        // Copeland (W-L): a=4, c=2, d=0, e=-2, f=-4.
        let (_ids, scores, index) = copeland_table(&ballots, 5);
        assert_eq!(scores[index[&"a"]], 4);
        assert_eq!(scores[index[&"c"]], 2);
        assert_eq!(scores[index[&"d"]], 0);
        assert_eq!(scores[index[&"e"]], -2);
        assert_eq!(scores[index[&"f"]], -4);
        assert_eq!(copeland_merge(&ballots, 5), vec!["a", "c", "d", "e", "f"]);
    }

    #[test]
    fn cycle_of_three_scores_zero_first_seen() {
        let ballots = [
            vec!["a", "b", "c"],
            vec!["b", "c", "a"],
            vec!["c", "a", "b"],
        ];
        // a>b 2-1, b>c 2-1, c>a 2-1. Each W-L is 0.
        let (_ids, scores, _index) = copeland_table(&ballots, 3);
        assert_eq!(scores, vec![0, 0, 0]);
        assert_eq!(copeland_merge(&ballots, 3), vec!["a", "b", "c"]);

        let rotated = [
            vec!["c", "a", "b"],
            vec!["a", "b", "c"],
            vec!["b", "c", "a"],
        ];
        let (_ids, scores, _index) = copeland_table(&rotated, 3);
        assert_eq!(scores, vec![0, 0, 0]);
        assert_eq!(copeland_merge(&rotated, 3), vec!["c", "a", "b"]);
    }
}
