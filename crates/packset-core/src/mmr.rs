//! Maximal marginal relevance after fuse.
//!
//! Diversify slot. MMR(d) = lambda * rel'(d) - (1-lambda) * max Jaccard(d, selected).

use std::collections::HashSet;

#[derive(Clone, Debug)]
pub struct Ranked {
    pub id: String,
    pub rel: f64,
    pub tokens: HashSet<String>,
}

/// The items in MMR order. Each pair meets once: a candidate keeps the
/// Jaccard to its nearest selected item, and one pass after each
/// selection brings that number up to date, so a list of n items costs
/// n(n-1)/2 set meetings. The sets meet as sorted slices in one pass,
/// not through a hash lookup per token: with eighty candidates the
/// hashing was three quarters of a search's time in the writer.
pub fn mmr_rerank(items: &[Ranked], lambda: f64) -> Vec<String> {
    if items.len() < 2 || !(0.0..1.0).contains(&lambda) {
        return items.iter().map(|i| i.id.clone()).collect();
    }
    let min = items.iter().map(|i| i.rel).fold(f64::INFINITY, f64::min);
    let max = items
        .iter()
        .map(|i| i.rel)
        .fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(1e-9);
    let rel = |r: f64| (r - min) / span;

    // Every item's tokens, sorted, in one allocation; `bounds[i]` is
    // item i's run of it.
    let mut arena: Vec<&str> = Vec::with_capacity(items.iter().map(|i| i.tokens.len()).sum());
    let mut bounds: Vec<(usize, usize)> = Vec::with_capacity(items.len());
    for item in items {
        let start = arena.len();
        arena.extend(item.tokens.iter().map(String::as_str));
        arena[start..].sort_unstable();
        bounds.push((start, arena.len()));
    }
    let tokens = |i: usize| &arena[bounds[i].0..bounds[i].1];
    // The Jaccard from each candidate to the selected item nearest it.
    let mut nearest = vec![0.0_f64; items.len()];
    let mut selected: Vec<usize> = Vec::with_capacity(items.len());
    let mut rest: Vec<usize> = (0..items.len()).collect();
    while !rest.is_empty() {
        let mut best = rest[0];
        let mut best_s = f64::NEG_INFINITY;
        for &i in &rest {
            let score = lambda * rel(items[i].rel) - (1.0 - lambda) * nearest[i];
            if score > best_s {
                best_s = score;
                best = i;
            }
        }
        selected.push(best);
        rest.retain(|&i| i != best);
        for &i in &rest {
            nearest[i] = nearest[i].max(jaccard(tokens(i), tokens(best)));
        }
    }
    selected.into_iter().map(|i| items[i].id.clone()).collect()
}

/// Jaccard of two sorted, duplicate-free slices, in one pass over both.
fn jaccard(a: &[&str], b: &[&str]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let mut inter = 0usize;
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let uni = a.len() + b.len() - inter;
    if uni == 0 {
        0.0
    } else {
        inter as f64 / uni as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::borda::borda_merge;

    #[test]
    fn mmr_after_borda() {
        let a = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        let b = vec!["y".to_string(), "x".to_string(), "z".to_string()];
        let order = borda_merge(&[a, b], 3);
        assert_eq!(order, vec!["x", "y", "z"]);
        let items = vec![
            Ranked {
                id: "keep".into(),
                rel: 1.0,
                tokens: ["review", "open", "repro"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            Ranked {
                id: "dup".into(),
                rel: 0.55,
                tokens: ["review", "open", "repro"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            Ranked {
                id: "other".into(),
                rel: 0.5,
                tokens: ["pin", "zircon", "index"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ];
        let reranked = mmr_rerank(&items, 0.7);
        assert_eq!(reranked, vec!["keep", "other", "dup"]);
    }

    /// The definition, one Jaccard per (candidate, selected) pair per
    /// round, over hash sets. What `mmr_rerank` has to agree with.
    fn by_definition(items: &[Ranked], lambda: f64) -> Vec<String> {
        let min = items.iter().map(|i| i.rel).fold(f64::INFINITY, f64::min);
        let max = items
            .iter()
            .map(|i| i.rel)
            .fold(f64::NEG_INFINITY, f64::max);
        let span = (max - min).max(1e-9);
        let set_jaccard = |a: &HashSet<String>, b: &HashSet<String>| -> f64 {
            if a.is_empty() && b.is_empty() {
                return 0.0;
            }
            a.intersection(b).count() as f64 / a.union(b).count() as f64
        };
        let mut selected: Vec<usize> = Vec::new();
        let mut rest: Vec<usize> = (0..items.len()).collect();
        while !rest.is_empty() {
            let mut best = rest[0];
            let mut best_s = f64::NEG_INFINITY;
            for &i in &rest {
                let novelty = selected
                    .iter()
                    .map(|&j| set_jaccard(&items[i].tokens, &items[j].tokens))
                    .fold(0.0, f64::max);
                let score = lambda * (items[i].rel - min) / span - (1.0 - lambda) * novelty;
                if score > best_s {
                    best_s = score;
                    best = i;
                }
            }
            selected.push(best);
            rest.retain(|&i| i != best);
        }
        selected.into_iter().map(|i| items[i].id.clone()).collect()
    }

    /// Eighty candidates with overlapping vocabularies, ties in relevance
    /// and a few empty token sets: the one-pass order is the definition's.
    #[test]
    fn the_one_pass_order_is_the_definitions() {
        let words = [
            "review", "open", "repro", "pin", "zircon", "index", "seat", "pack", "atom", "vote",
            "ticket", "issue", "claim", "deed", "lesson", "rule", "sync", "age", "milli", "dense",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let items: Vec<Ranked> = (0..80)
            .map(|n| {
                let count = (next() % 9) as usize;
                let tokens: HashSet<String> = (0..count)
                    .map(|_| words[(next() % words.len() as u64) as usize].to_string())
                    .collect();
                Ranked {
                    id: format!("h{n}"),
                    rel: ((next() % 7) as f64) / 7.0,
                    tokens,
                }
            })
            .collect();
        for lambda in [0.0, 0.3, 0.7, 0.95] {
            assert_eq!(
                mmr_rerank(&items, lambda),
                by_definition(&items, lambda),
                "lambda {lambda}"
            );
        }
    }
}
