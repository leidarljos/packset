//! Schulze beatpath: pairwise counts, then strongest paths.
//!
//! d[i,j] is how many ballots rank i above j (top-k). i beats j
//! when the strongest i->j path is stronger than j->i. Floyd-style
//! widest paths. Ties break first-seen.
//!
//! Schulze, A new monotonic, clone-independent, reversal
//! symmetric, and condorcet-consistent single-winner election
//! method, Social Choice and Welfare 2011.
//! doi:10.1007/s00355-010-0475-4

use std::collections::HashMap;
use std::hash::Hash;

use crate::borda::Ballot;

/// Merge ballots by Schulze beatpath. Ties break by first-seen id order.
pub fn schulze_merge<T>(ballots: &[Ballot<T>], k: usize) -> Vec<T>
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

    let mut p = vec![vec![0i64; n]; n];
    for i in 0..n {
        for j in 0..n {
            if i != j && d[i][j] > d[j][i] {
                p[i][j] = d[i][j];
            }
        }
    }
    for mid in 0..n {
        for i in 0..n {
            if i == mid {
                continue;
            }
            for j in 0..n {
                if j == mid || j == i {
                    continue;
                }
                let via = p[i][mid].min(p[mid][j]);
                if via > p[i][j] {
                    p[i][j] = via;
                }
            }
        }
    }

    let mut ranked = first_seen;
    ranked.sort_by(|a, b| {
        let i = index[a];
        let j = index[b];
        p[j][i].cmp(&p[i][j]).then_with(|| i.cmp(&j))
    });
    ranked
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
    fn condorcet_winner_first_when_borda_midpack() {
        let ballots = condorcet_mid_borda();
        // Borda (4,3,2,1,0): c=14, d=13, a=12. Mid-rank pile elects c.
        assert_eq!(borda_merge(&ballots, 5), vec!["c", "d", "a", "e", "f"]);
        // a beats every other pairwise 3-2. Schulze elects the Condorcet winner.
        assert_eq!(schulze_merge(&ballots, 5)[0], "a");
    }

    #[test]
    fn cycle_pins_wikipedia_order() {
        // Schulze 2011 / Wikipedia five-candidate cycle. 45 ballots.
        let groups: &[(&[&str], usize)] = &[
            (&["a", "c", "b", "e", "d"], 5),
            (&["a", "d", "e", "c", "b"], 5),
            (&["b", "e", "d", "a", "c"], 8),
            (&["c", "a", "b", "e", "d"], 3),
            (&["c", "a", "e", "b", "d"], 7),
            (&["c", "b", "a", "d", "e"], 2),
            (&["d", "c", "e", "b", "a"], 7),
            (&["e", "b", "a", "d", "c"], 8),
        ];
        let mut ballots = Vec::new();
        for (order, n) in groups {
            for _ in 0..*n {
                ballots.push(order.to_vec());
            }
        }
        assert_eq!(schulze_merge(&ballots, 5), vec!["e", "a", "c", "b", "d"]);
    }
}
