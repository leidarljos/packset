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

    let mut selected: Vec<usize> = Vec::new();
    let mut rest: Vec<usize> = (0..items.len()).collect();
    while !rest.is_empty() {
        let mut best = rest[0];
        let mut best_s = f64::NEG_INFINITY;
        for &i in &rest {
            let novelty = selected
                .iter()
                .map(|&j| jaccard(&items[i].tokens, &items[j].tokens))
                .fold(0.0, f64::max);
            let score = lambda * rel(items[i].rel) - (1.0 - lambda) * novelty;
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

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let uni = a.union(b).count() as f64;
    if uni == 0.0 {
        0.0
    } else {
        inter / uni
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
}
