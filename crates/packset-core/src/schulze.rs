//! Schulze beatpath: pairwise counts, then strongest paths.
//!
//! `d[i,j]` is how many ballots rank i above j (top-k). i beats j
//! when the strongest i->j path is stronger than j->i. Floyd-style
//! widest paths.
//!
//! The linear order is by how many others a candidate beats that way,
//! first-seen breaking a tie. Ordering by the pairwise comparison
//! itself is not sound: the relation is transitive, but broken by
//! first-seen it is not, and three candidates can tie pairwise in a
//! pattern that sends a before b, b before c and c before a.
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

    // How many others each candidate beats on the strongest paths.
    //
    // Sorting by the beatpath comparison itself is not sound. The relation is
    // transitive, which is Schulze's theorem, but the relation broken by
    // first-seen order is not: three candidates can tie pairwise in a pattern
    // where the tie-break sends a before b, b before c, and c before a. A sort
    // handed that comparator has no correct answer to give, and the standard
    // library says so by panicking rather than returning a wrong order.
    //
    // The win count is an integer, so ordering by it is total by construction,
    // and it never contradicts the relation: if a beats b then a also beats
    // everything b beats, by that same transitivity, plus b itself.
    let wins: Vec<usize> = (0..n)
        .map(|i| (0..n).filter(|j| *j != i && p[i][*j] > p[*j][i]).count())
        .collect();
    let mut ranked = first_seen;
    ranked.sort_by(|a, b| {
        let i = index[a];
        let j = index[b];
        wins[j].cmp(&wins[i]).then_with(|| i.cmp(&j))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::borda::borda_merge;

    /// A comparator that is not a total order has no correct answer to give,
    /// and the sort says so by panicking rather than by returning a wrong one.
    ///
    /// Two ballots never reached this: every contested pair ties, so first-seen
    /// order alone decided and that is total. Three is the first number of
    /// voters where the tie-break can send a before b, b before c and c before
    /// a, which is why the fusion swept over three ballots is what found it.
    #[test]
    fn three_ballots_never_ask_the_sort_for_an_impossible_order() {
        // A small deterministic generator, so a failure is reproducible and no
        // dependency is added for it.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let names: Vec<String> = (0..9).map(|n| format!("id{n}")).collect();
        for _ in 0..500 {
            let ballots: Vec<Ballot<String>> = (0..3)
                .map(|_| {
                    let mut pool = names.clone();
                    // Shuffle, then truncate, so the ballots disagree about
                    // which candidates exist as well as about their order.
                    for at in (1..pool.len()).rev() {
                        pool.swap(at, (next() % (at as u64 + 1)) as usize);
                    }
                    pool.truncate(4 + (next() % 5) as usize);
                    pool
                })
                .collect();
            let ranked = schulze_merge(&ballots, 9);
            let seen: std::collections::HashSet<&String> = ranked.iter().collect();
            assert_eq!(seen.len(), ranked.len(), "a key came back twice");
        }
    }

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
