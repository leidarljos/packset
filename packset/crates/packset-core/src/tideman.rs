//! Tideman ranked pairs: lock pairwise victories by margin.
//!
//! d[i,j] is how many ballots rank i above j (top-k). A victory
//! is i over j when d[i,j] > d[j,i]. Sort by margin
//! d[i,j] - d[j,i], then lock a victory if it does not create a
//! cycle. The locked graph is a DAG; topological order is the
//! ranking. Ties break first-seen.
//!
//! Tideman, Independence of clones as a criterion for voting
//! rules, Social Choice and Welfare 1987.
//! doi:10.1007/bf00433944

use std::collections::HashMap;
use std::hash::Hash;

use crate::borda::Ballot;

/// Merge ballots by Tideman ranked pairs. Ties break by first-seen id order.
pub fn ranked_pairs_merge<T>(ballots: &[Ballot<T>], k: usize) -> Vec<T>
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

    let mut victories = Vec::new();
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            if d[i][j] > d[j][i] {
                victories.push(Victory {
                    winner: i,
                    loser: j,
                    margin: d[i][j] - d[j][i],
                    win_votes: d[i][j],
                });
            }
        }
    }
    victories.sort_by(|a, b| {
        b.margin
            .cmp(&a.margin)
            .then(b.win_votes.cmp(&a.win_votes))
            .then(a.winner.cmp(&b.winner))
            .then(a.loser.cmp(&b.loser))
    });

    let mut locked = vec![Vec::new(); n];
    for v in victories {
        if reaches(&locked, v.loser, v.winner) {
            continue;
        }
        locked[v.winner].push(v.loser);
    }

    topo_first_seen(&locked, &first_seen)
}

struct Victory {
    winner: usize,
    loser: usize,
    margin: i64,
    win_votes: i64,
}

fn reaches(adj: &[Vec<usize>], from: usize, to: usize) -> bool {
    if from == to {
        return true;
    }
    let n = adj.len();
    let mut seen = vec![false; n];
    let mut stack = vec![from];
    seen[from] = true;
    while let Some(u) = stack.pop() {
        for &v in &adj[u] {
            if v == to {
                return true;
            }
            if !seen[v] {
                seen[v] = true;
                stack.push(v);
            }
        }
    }
    false
}

fn topo_first_seen<T: Clone>(adj: &[Vec<usize>], first_seen: &[T]) -> Vec<T> {
    let n = first_seen.len();
    let mut indeg = vec![0usize; n];
    for u in 0..n {
        for &v in &adj[u] {
            indeg[v] += 1;
        }
    }
    let mut remaining = vec![true; n];
    let mut ranked = Vec::with_capacity(n);
    for _ in 0..n {
        let pick = (0..n).find(|&i| remaining[i] && indeg[i] == 0);
        let Some(i) = pick else {
            break;
        };
        remaining[i] = false;
        ranked.push(first_seen[i].clone());
        for &v in &adj[i] {
            indeg[v] -= 1;
        }
    }
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

    fn expand(groups: &[(&'static [&'static str], usize)]) -> Vec<Ballot<&'static str>> {
        let mut ballots = Vec::new();
        for &(order, n) in groups {
            for _ in 0..n {
                ballots.push(order.to_vec());
            }
        }
        ballots
    }

    #[test]
    fn condorcet_winner_first_when_borda_midpack() {
        let ballots = condorcet_mid_borda();
        // Borda (4,3,2,1,0): c=14, d=13, a=12. Mid-rank pile elects c.
        assert_eq!(borda_merge(&ballots, 5), vec!["c", "d", "a", "e", "f"]);
        // a beats every other pairwise 3-2. Ranked pairs elects the Condorcet winner.
        assert_eq!(ranked_pairs_merge(&ballots, 5)[0], "a");
    }

    #[test]
    fn cycle_pins_wikipedia_order() {
        // Schulze 2011 / Wikipedia five-candidate cycle. 45 ballots.
        // Ranked pairs skips the cycle-closing locks and ranks a,c,e,b,d.
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
        let ballots = expand(groups);
        assert_eq!(
            ranked_pairs_merge(&ballots, 5),
            vec!["a", "c", "e", "b", "d"]
        );
    }

    #[test]
    fn tennessee_locks_wikipedia_order() {
        // Wikipedia Ranked pairs Tennessee capital. No skipped lock.
        let groups: &[(&[&str], usize)] = &[
            (&["memphis", "nashville", "chattanooga", "knoxville"], 42),
            (&["nashville", "chattanooga", "knoxville", "memphis"], 26),
            (&["chattanooga", "knoxville", "nashville", "memphis"], 15),
            (&["knoxville", "chattanooga", "nashville", "memphis"], 17),
        ];
        let ballots = expand(groups);
        assert_eq!(
            ranked_pairs_merge(&ballots, 4),
            vec!["nashville", "chattanooga", "knoxville", "memphis"]
        );
    }
}
