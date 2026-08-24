//! Determinantal Point Process after fuse.
//!
//! Diversify slot. Score of a set S is det(L_S) with
//! L_ij = rel_i * rel_j * sim_ij. sim is token Jaccard.
//! Greedy MAP inference adds the item that most increases det.
//!
//! Kulesza, Taskar, Determinantal Point Processes for Machine
//! Learning, 2012. doi:10.1561/2200000044

use crate::atom::entity_jaccard;
use crate::mmr::Ranked;

/// Rerank by greedy MAP of a quality-diversity DPP. `k` is the keep count.
pub fn dpp_rerank(items: &[Ranked], k: usize) -> Vec<String> {
    if items.is_empty() || k == 0 {
        return Vec::new();
    }
    let n = items.len();
    let keep = k.min(n);
    if n < 2 {
        return items.iter().take(keep).map(|i| i.id.clone()).collect();
    }

    let mut kernel = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in i..n {
            let sim = entity_jaccard(
                items[i].tokens.iter().map(String::as_str),
                items[j].tokens.iter().map(String::as_str),
            );
            let v = items[i].rel * items[j].rel * sim;
            kernel[i][j] = v;
            kernel[j][i] = v;
        }
    }

    let mut selected: Vec<usize> = Vec::new();
    let mut rest: Vec<usize> = (0..n).collect();
    while selected.len() < keep {
        let mut best = rest[0];
        let mut best_det = f64::NEG_INFINITY;
        for &i in &rest {
            let mut idx = selected.clone();
            idx.push(i);
            let d = submatrix_det(&kernel, &idx);
            if d > best_det {
                best_det = d;
                best = i;
            }
        }
        selected.push(best);
        rest.retain(|&i| i != best);
    }
    selected.into_iter().map(|i| items[i].id.clone()).collect()
}

fn submatrix_det(kernel: &[Vec<f64>], idx: &[usize]) -> f64 {
    let m = idx.len();
    let mut a = vec![vec![0.0; m]; m];
    for (r, &i) in idx.iter().enumerate() {
        for (c, &j) in idx.iter().enumerate() {
            a[r][c] = kernel[i][j];
        }
    }
    det(&mut a)
}

fn det(a: &mut [Vec<f64>]) -> f64 {
    let n = a.len();
    if n == 0 {
        return 1.0;
    }
    let mut sign = 1.0;
    let mut out = 1.0;
    for i in 0..n {
        let mut pivot = i;
        for r in (i + 1)..n {
            if a[r][i].abs() > a[pivot][i].abs() {
                pivot = r;
            }
        }
        if a[pivot][i].abs() <= 1e-15 {
            return 0.0;
        }
        if pivot != i {
            a.swap(i, pivot);
            sign = -sign;
        }
        let pv = a[i][i];
        out *= pv;
        for r in (i + 1)..n {
            let f = a[r][i] / pv;
            for c in i..n {
                a[r][c] -= f * a[i][c];
            }
        }
    }
    out * sign
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmr::mmr_rerank;

    fn keep_dup_other() -> Vec<Ranked> {
        vec![
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
        ]
    }

    fn three_near_dup_and_novel() -> Vec<Ranked> {
        vec![
            Ranked {
                id: "dup_a".into(),
                rel: 1.0,
                tokens: ["review", "open", "repro"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            Ranked {
                id: "dup_b".into(),
                rel: 0.9,
                tokens: ["review", "open", "repro", "extra"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            Ranked {
                id: "dup_c".into(),
                rel: 0.8,
                tokens: ["review", "open", "repro", "other"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
            Ranked {
                id: "novel".into(),
                rel: 0.7,
                tokens: ["pin", "zircon", "index"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            },
        ]
    }

    #[test]
    fn dpp_after_borda_puts_other_before_dup() {
        let items = keep_dup_other();
        let reranked = dpp_rerank(&items, items.len());
        assert_eq!(reranked, vec!["keep", "other", "dup"]);
    }

    #[test]
    fn three_near_dup_prefers_novel_over_mmr() {
        let items = three_near_dup_and_novel();
        let mmr = mmr_rerank(&items, 0.7);
        let dpp = dpp_rerank(&items, items.len());
        assert_eq!(mmr, vec!["dup_a", "dup_b", "dup_c", "novel"]);
        assert_ne!(dpp, mmr);
        assert_eq!(dpp, vec!["dup_a", "novel", "dup_b", "dup_c"]);
    }

    #[test]
    fn keep_count_truncates() {
        let items = three_near_dup_and_novel();
        assert_eq!(dpp_rerank(&items, 2), vec!["dup_a", "novel"]);
        assert!(dpp_rerank(&items, 0).is_empty());
    }
}
