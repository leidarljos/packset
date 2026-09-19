//! Memory islands: the natural clusters of the link graph, and the set a cue
//! activates. A persona is a view over everything; an island is what one
//! task touches.

use std::collections::HashMap;

use crate::search::Record;

/// The weight a link carries when nothing has fired over it.
pub const WEIGHT_DEFAULT: f64 = 0.5;
/// How far a co-activated pair moves toward one.
pub const ETA: f64 = 0.1;
/// How much of every other link a fired claim forgets.
pub const LAMBDA: f64 = 0.02;
/// The most links `fire` will add to a claim that has none to spare.
pub const FIRE_LINK_MAX: usize = 8;

/// The link graph over one live set, as positions, each edge with its weight.
pub struct Graph {
    ids: Vec<String>,
    adjacency: Vec<Vec<(usize, f64)>>,
}

impl Graph {
    /// Symmetric edges from every atom's `links`; a link to an id outside the
    /// set is dropped.
    #[must_use]
    pub fn from_atoms(atoms: &[Record]) -> Self {
        Self::from_atoms_as(atoms, None)
    }

    /// [`Self::from_atoms`] through a lens: the weights a persona wrote on
    /// the edges it fired, the shared weight where it wrote none. The nodes
    /// and the links are the pack's; only the weights are the persona's.
    #[must_use]
    pub fn from_atoms_as(atoms: &[Record], lens: Option<&str>) -> Self {
        let ids: Vec<String> = atoms
            .iter()
            .map(|a| {
                a.get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        let index: HashMap<&str, usize> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        let mut adjacency: Vec<HashMap<usize, f64>> = vec![HashMap::new(); atoms.len()];
        for (i, atom) in atoms.iter().enumerate() {
            let Some(links) = atom.get("links").and_then(|v| v.as_array()) else {
                continue;
            };
            for link in links.iter().filter_map(|l| l.as_str()) {
                let Some(&j) = index.get(link) else {
                    continue;
                };
                if i == j {
                    continue;
                }
                let weight = weight_of_as(atom, link, lens);
                // Both sides may carry a weight; the heavier one is the edge's.
                let held = adjacency[i].entry(j).or_insert(0.0);
                *held = held.max(weight);
                let back = adjacency[j].entry(i).or_insert(0.0);
                *back = back.max(weight);
            }
        }
        let adjacency = adjacency
            .into_iter()
            .map(|row| {
                let mut edges: Vec<(usize, f64)> = row.into_iter().collect();
                edges.sort_by_key(|e| e.0);
                edges
            })
            .collect();
        Self { ids, adjacency }
    }

    /// The weight of the edge between two positions, if there is one.
    #[must_use]
    pub fn weight(&self, from: usize, to: usize) -> Option<f64> {
        self.adjacency
            .get(from)?
            .iter()
            .find(|(peer, _)| *peer == to)
            .map(|(_, w)| *w)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    #[must_use]
    pub fn id(&self, at: usize) -> &str {
        &self.ids[at]
    }

    #[must_use]
    pub fn position(&self, id: &str) -> Option<usize> {
        self.ids.iter().position(|held| held == id)
    }
}

/// The weight an atom records for one of its links; absent means the default.
fn weight_of(atom: &Record, peer: &str) -> f64 {
    atom.get("link_weights")
        .and_then(|w| w.get(peer))
        .and_then(|w| w.as_f64())
        .unwrap_or(WEIGHT_DEFAULT)
}

/// [`weight_of`] through a lens: the persona's own weight for the edge
/// under `link_weights_by`, else the shared one.
fn weight_of_as(atom: &Record, peer: &str, lens: Option<&str>) -> f64 {
    lens.and_then(|name| {
        atom.get("link_weights_by")
            .and_then(|by| by.get(name))
            .and_then(|w| w.get(peer))
            .and_then(|w| w.as_f64())
    })
    .unwrap_or_else(|| weight_of(atom, peer))
}

fn set_weight(atom: &mut Record, peer: &str, weight: f64) {
    let entry = atom
        .entry("link_weights")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(map) = entry.as_object_mut() {
        map.insert(peer.to_string(), serde_json::json!(weight));
    }
}

/// [`set_weight`] through a lens: written under `link_weights_by[lens]`,
/// so a persona's fire moves its own paths and nobody else's.
fn set_weight_as(atom: &mut Record, peer: &str, weight: f64, lens: Option<&str>) {
    let Some(name) = lens else {
        return set_weight(atom, peer, weight);
    };
    let by = atom
        .entry("link_weights_by")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(by) = by.as_object_mut() {
        let own = by
            .entry(name.to_string())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(map) = own.as_object_mut() {
            map.insert(peer.to_string(), serde_json::json!(weight));
        }
    }
}

fn has_link(atom: &Record, peer: &str) -> bool {
    atom.get("links")
        .and_then(|l| l.as_array())
        .is_some_and(|links| links.iter().any(|l| l.as_str() == Some(peer)))
}

fn link_count(atom: &Record) -> usize {
    atom.get("links")
        .and_then(|l| l.as_array())
        .map_or(0, Vec::len)
}

fn add_link(atom: &mut Record, peer: &str) {
    let entry = atom
        .entry("links")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if let Some(links) = entry.as_array_mut() {
        links.push(serde_json::Value::String(peer.to_string()));
    }
}

/// Claims that fired together wire together. Every pair among `fired` moves
/// its weight toward one by [`ETA`], gaining a link when neither side is at
/// [`FIRE_LINK_MAX`]; every other link of a fired claim forgets by
/// [`LAMBDA`], the term Oja's rule adds to Hebb (doi:10.1007/BF00275687).
/// Returns the positions whose record changed.
pub fn fire(atoms: &mut [Record], fired: &[usize]) -> Vec<usize> {
    fire_as(atoms, fired, None)
}

/// [`fire`] through a lens: the links made are the pack's, the weights
/// moved are the persona's own, so several personas walking one island
/// each tighten the paths they walked and leave the seat's graph as it was.
pub fn fire_as(atoms: &mut [Record], fired: &[usize], lens: Option<&str>) -> Vec<usize> {
    let mut fired: Vec<usize> = fired.iter().copied().filter(|&i| i < atoms.len()).collect();
    fired.sort_unstable();
    fired.dedup();
    if fired.len() < 2 {
        return Vec::new();
    }
    let ids: Vec<String> = atoms
        .iter()
        .map(|a| {
            a.get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let position: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    let mut changed = std::collections::BTreeSet::new();
    for &i in &fired {
        let peers: Vec<String> = atoms[i]
            .get("links")
            .and_then(|l| l.as_array())
            .map(|links| {
                links
                    .iter()
                    .filter_map(|l| l.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for peer in peers {
            let together = fired.iter().any(|&j| ids[j] == peer);
            if together {
                continue;
            }
            // Both ends carry the edge's weight, so the decay is written on
            // the peer as well when it is in the set.
            let w = weight_of_as(&atoms[i], &peer, lens) * (1.0 - LAMBDA);
            set_weight_as(&mut atoms[i], &peer, w, lens);
            changed.insert(i);
            if let Some(&j) = position.get(peer.as_str()) {
                if j != i && !ids[i].is_empty() {
                    set_weight_as(&mut atoms[j], &ids[i], w, lens);
                    changed.insert(j);
                }
            }
        }
    }
    for (a, &i) in fired.iter().enumerate() {
        for &j in &fired[a + 1..] {
            let (id_i, id_j) = (ids[i].clone(), ids[j].clone());
            if id_i.is_empty() || id_j.is_empty() {
                continue;
            }
            let linked = has_link(&atoms[i], &id_j) || has_link(&atoms[j], &id_i);
            if !linked {
                if link_count(&atoms[i]) >= FIRE_LINK_MAX || link_count(&atoms[j]) >= FIRE_LINK_MAX
                {
                    continue;
                }
                add_link(&mut atoms[i], &id_j);
                add_link(&mut atoms[j], &id_i);
            } else {
                if !has_link(&atoms[i], &id_j) {
                    add_link(&mut atoms[i], &id_j);
                }
                if !has_link(&atoms[j], &id_i) {
                    add_link(&mut atoms[j], &id_i);
                }
            }
            let w = weight_of_as(&atoms[i], &id_j, lens).max(weight_of_as(&atoms[j], &id_i, lens));
            let w = (w + ETA * (1.0 - w)).min(1.0);
            set_weight_as(&mut atoms[i], &id_j, w, lens);
            set_weight_as(&mut atoms[j], &id_i, w, lens);
            changed.insert(i);
            changed.insert(j);
        }
    }
    changed.into_iter().collect()
}

/// Rounds of label propagation before the labels are taken as they stand.
const PROPAGATION_ROUNDS: usize = 20;

/// The damping of the hub walk: the share of each step that follows a
/// link rather than jumping anywhere, as in the original.
pub const HUB_DAMPING: f64 = 0.85;

/// Which claims the link graph turns on: a weighted PageRank (Brin and
/// Page, Computer Networks 30, 1998) over the links, each step
/// following a link with probability proportional to its weight. A claim
/// many well-linked claims link to stands high; an isolated claim keeps
/// the jump floor. Returns every claim with its score, highest first; the
/// scores sum to one. This is the graph's own answer to what matters, as
/// opposed to what a query asks for.
#[must_use]
pub fn hubs(graph: &Graph) -> Vec<(usize, f64)> {
    let n = graph.len();
    if n == 0 {
        return Vec::new();
    }
    let uniform = 1.0 / n as f64;
    let mut score = vec![uniform; n];
    let totals: Vec<f64> = graph
        .adjacency
        .iter()
        .map(|edges| edges.iter().map(|(_, w)| *w).sum::<f64>())
        .collect();
    for _ in 0..100 {
        let mut next = vec![(1.0 - HUB_DAMPING) * uniform; n];
        for (i, edges) in graph.adjacency.iter().enumerate() {
            if totals[i] <= 0.0 {
                // A claim with no links spreads its score everywhere.
                for x in next.iter_mut() {
                    *x += HUB_DAMPING * score[i] * uniform;
                }
                continue;
            }
            for (j, w) in edges {
                next[*j] += HUB_DAMPING * score[i] * (w / totals[i]);
            }
        }
        let diff: f64 = next.iter().zip(&score).map(|(a, b)| (a - b).abs()).sum();
        score = next;
        if diff < 1e-9 {
            break;
        }
    }
    let mut ranked: Vec<(usize, f64)> = score.into_iter().enumerate().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
}

/// The islands: communities by label propagation (Raghavan, Albert and
/// Kumara, doi:10.1103/PhysRevE.76.036106). Every node takes the label most
/// of its neighbours held in the previous round, smallest label on a tie, so
/// the answer is deterministic; a lone bridge edge loses to the clique on
/// its far side within two rounds. Largest island first, then by first
/// member.
#[must_use]
pub fn islands(graph: &Graph) -> Vec<Vec<usize>> {
    let n = graph.len();
    let mut label: Vec<usize> = (0..n).collect();
    for _ in 0..PROPAGATION_ROUNDS {
        let next: Vec<usize> = (0..n)
            .map(|node| {
                let peers = &graph.adjacency[node];
                if peers.is_empty() {
                    return label[node];
                }
                let mut counts: HashMap<usize, f64> = HashMap::new();
                for &(peer, weight) in peers {
                    *counts.entry(label[peer]).or_insert(0.0) += weight;
                }
                counts
                    .iter()
                    .max_by(|a, b| {
                        a.1.partial_cmp(b.1)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| b.0.cmp(a.0))
                    })
                    .map_or(label[node], |(&l, _)| l)
            })
            .collect();
        if next == label {
            break;
        }
        label = next;
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (node, &l) in label.iter().enumerate() {
        groups.entry(l).or_default().push(node);
    }
    let mut out: Vec<Vec<usize>> = groups.into_values().collect();
    for group in &mut out {
        group.sort_unstable();
    }
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
    out
}

/// Newman-Girvan modularity of a partition over the weighted graph: the
/// weight inside communities beyond what the degrees alone would put
/// there (doi:10.1103/PhysRevE.69.026113). Zero for one community; one is
/// the unreachable ideal; label propagation is judged against it.
#[must_use]
pub fn modularity(graph: &Graph, communities: &[Vec<usize>]) -> f64 {
    let degree: Vec<f64> = graph
        .adjacency
        .iter()
        .map(|peers| peers.iter().map(|(_, w)| w).sum())
        .collect();
    let m2: f64 = degree.iter().sum();
    if m2 <= 0.0 {
        return 0.0;
    }
    let mut of = vec![usize::MAX; graph.len()];
    for (c, members) in communities.iter().enumerate() {
        for &m in members {
            if m < of.len() {
                of[m] = c;
            }
        }
    }
    // Per community: the weight inside it, and the degree it holds; the
    // expected inside weight is the square of the held share.
    let mut inside = vec![0.0; communities.len()];
    let mut total = vec![0.0; communities.len()];
    for (i, peers) in graph.adjacency.iter().enumerate() {
        if of[i] == usize::MAX {
            continue;
        }
        total[of[i]] += degree[i];
        for &(j, w) in peers {
            if of[j] == of[i] {
                inside[of[i]] += w;
            }
        }
    }
    inside
        .iter()
        .zip(&total)
        .map(|(inn, tot)| inn / m2 - (tot / m2) * (tot / m2))
        .sum()
}

/// Communities by greedy modularity optimisation, the Louvain method
/// (Blondel, Guillaume, Lambiotte and Lefebvre,
/// doi:10.1088/1742-5468/2008/10/P10008): every node moves to the
/// neighbouring community that gains most modularity until none does,
/// the communities become the nodes of a smaller graph, and the two
/// steps repeat until a level gains nothing. Nodes are visited in index
/// order, so the result is the same on every run. Largest first, members
/// sorted, as [`islands`] returns.
#[must_use]
pub fn communities(graph: &Graph) -> Vec<Vec<usize>> {
    let n = graph.len();
    if n == 0 {
        return Vec::new();
    }
    // The current level's graph as weighted adjacency, and which original
    // nodes each level node stands for.
    let mut adjacency: Vec<Vec<(usize, f64)>> = graph.adjacency.clone();
    let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    loop {
        let size = adjacency.len();
        let degree: Vec<f64> = adjacency
            .iter()
            .map(|peers| peers.iter().map(|(_, w)| w).sum())
            .collect();
        let m2: f64 = degree.iter().sum();
        if m2 <= 0.0 {
            break;
        }
        let mut community: Vec<usize> = (0..size).collect();
        let mut total: Vec<f64> = degree.clone();
        let mut moved_any = false;
        loop {
            let mut moved = false;
            for i in 0..size {
                let own = community[i];
                // Weight from i into each neighbouring community.
                let mut into: HashMap<usize, f64> = HashMap::new();
                for &(j, w) in &adjacency[i] {
                    if j != i {
                        *into.entry(community[j]).or_insert(0.0) += w;
                    }
                }
                total[own] -= degree[i];
                let stay = into.get(&own).copied().unwrap_or(0.0) - total[own] * degree[i] / m2;
                let mut best = (own, stay);
                let mut candidates: Vec<(&usize, &f64)> = into.iter().collect();
                candidates.sort_by_key(|(c, _)| **c);
                for (&c, &w_in) in candidates {
                    let gain = w_in - total[c] * degree[i] / m2;
                    if gain > best.1 + 1e-12 {
                        best = (c, gain);
                    }
                }
                total[best.0] += degree[i];
                if best.0 != own {
                    community[i] = best.0;
                    moved = true;
                    moved_any = true;
                }
            }
            if !moved {
                break;
            }
        }
        if !moved_any {
            break;
        }
        // Aggregate: one node per community, self loops for inside weight.
        let mut relabel: HashMap<usize, usize> = HashMap::new();
        for &c in &community {
            let next = relabel.len();
            relabel.entry(c).or_insert(next);
        }
        let levels = relabel.len();
        let mut next_adj: Vec<HashMap<usize, f64>> = vec![HashMap::new(); levels];
        let mut next_members: Vec<Vec<usize>> = vec![Vec::new(); levels];
        for i in 0..size {
            let ci = relabel[&community[i]];
            next_members[ci].extend(members[i].iter().copied());
            for &(j, w) in &adjacency[i] {
                let cj = relabel[&community[j]];
                *next_adj[ci].entry(cj).or_insert(0.0) += w;
            }
        }
        adjacency = next_adj
            .into_iter()
            .map(|row| {
                let mut edges: Vec<(usize, f64)> = row.into_iter().collect();
                edges.sort_by_key(|e| e.0);
                edges
            })
            .collect();
        members = next_members;
        if levels == size {
            break;
        }
    }
    let mut out = members;
    for group in &mut out {
        group.sort_unstable();
    }
    out.retain(|g| !g.is_empty());
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
    out
}

/// Rounds of colour refinement a signature takes.
pub const WL_ROUNDS: usize = 3;

/// A structural signature of a set of nodes: Weisfeiler-Lehman colour
/// refinement (doi:10.1007/978-3-030-79087-9_5 surveys it) over the link
/// graph, starting from each node's degree, [`WL_ROUNDS`] rounds, hashed
/// over the members' final colours. Two islands with the same shape carry
/// the same signature whatever their texts, so a seat can recognise a
/// pattern of memories it holds again after a handover, and the link
/// predictor can read structure as a feature. Refinement is not a
/// complete canonical form; graphs it cannot tell apart are the regular
/// ones, and a canonical labelling (nauty) stands behind it when that
/// matters.
#[must_use]
pub fn signature(graph: &Graph, members: &[usize]) -> u64 {
    let n = graph.len();
    let mut colour: Vec<u64> = (0..n)
        .map(|i| fnv(&[graph.adjacency[i].len() as u64]))
        .collect();
    for _ in 0..WL_ROUNDS {
        let next: Vec<u64> = (0..n)
            .map(|i| {
                let mut around: Vec<u64> =
                    graph.adjacency[i].iter().map(|(j, _)| colour[*j]).collect();
                around.sort_unstable();
                let mut words = vec![colour[i]];
                words.extend(around);
                fnv(&words)
            })
            .collect();
        colour = next;
    }
    let mut own: Vec<u64> = members
        .iter()
        .filter(|&&m| m < n)
        .map(|&m| colour[m])
        .collect();
    own.sort_unstable();
    fnv(&own)
}

/// FNV-1a over 64-bit words, the same on every build.
fn fnv(words: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// How much of a node's activation reaches its neighbours per hop.
pub const HOP_DECAY: f64 = 0.5;

/// Spreading activation from weighted seeds (Collins and Loftus,
/// doi:10.1037/0033-295X.82.6.407). Each hop passes [`HOP_DECAY`] of a
/// node's energy to its neighbours in proportion to the link weights, so a
/// well-worn link carries more and a busy node spreads thinner: Anderson's
/// fan effect, over Hebbian weights. Returns every node that received
/// activation, strongest first.
#[must_use]
pub fn activate(graph: &Graph, seeds: &[(usize, f64)], hops: usize) -> Vec<(usize, f64)> {
    let n = graph.len();
    let mut activation = vec![0.0f64; n];
    let mut frontier = vec![0.0f64; n];
    for &(node, weight) in seeds {
        if node < n && weight > 0.0 {
            activation[node] += weight;
            frontier[node] += weight;
        }
    }
    for _ in 0..hops {
        let mut next = vec![0.0f64; n];
        for (energy, peers) in frontier.iter().zip(&graph.adjacency) {
            if *energy <= 0.0 || peers.is_empty() {
                continue;
            }
            let total: f64 = peers.iter().map(|(_, w)| w).sum();
            if total <= 0.0 {
                continue;
            }
            for &(peer, weight) in peers {
                next[peer] += HOP_DECAY * energy * weight / total;
            }
        }
        for (held, gained) in activation.iter_mut().zip(&next) {
            *held += gained;
        }
        frontier = next;
    }
    let mut out: Vec<(usize, f64)> = activation
        .into_iter()
        .enumerate()
        .filter(|(_, a)| *a > 0.0)
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim every other claim links to stands highest; an isolated
    /// claim keeps the jump floor; the scores sum to one.
    #[test]
    fn hubs_rank_the_linked_to() {
        let atom = |id: &str, links: &[&str]| -> Record {
            serde_json::json!({"id": id, "kind": "conclusion", "text": id, "links": links})
                .as_object()
                .cloned()
                .unwrap()
        };
        let atoms = vec![
            atom("hub", &[]),
            atom("a", &["hub"]),
            atom("b", &["hub"]),
            atom("c", &["hub", "a"]),
            atom("lone", &[]),
        ];
        let graph = Graph::from_atoms(&atoms);
        let ranked = hubs(&graph);
        assert_eq!(ranked.len(), 5);
        assert_eq!(graph.id(ranked[0].0), "hub", "{ranked:?}");
        let total: f64 = ranked.iter().map(|(_, s)| s).sum();
        assert!((total - 1.0).abs() < 1e-6);
        let lone = ranked
            .iter()
            .find(|(i, _)| graph.id(*i) == "lone")
            .unwrap()
            .1;
        assert!(lone > 0.0 && lone < ranked[0].1);
    }
    use serde_json::json;

    fn clique(prefix: &str, n: usize) -> Vec<Record> {
        (0..n)
            .map(|i| {
                let links: Vec<String> = (0..n)
                    .filter(|j| *j != i)
                    .map(|j| format!("{prefix}{j}"))
                    .collect();
                json!({"id": format!("{prefix}{i}"), "links": links})
                    .as_object()
                    .cloned()
                    .unwrap()
            })
            .collect()
    }

    /// Two cliques joined by one edge are two islands, and a cue in one
    /// activates its own clique above the other.
    #[test]
    fn modularity_communities_split_two_cliques_and_beat_propagation() {
        // Two cliques of five joined by one edge.
        let mut atoms = Vec::new();
        for i in 0..10 {
            let side = if i < 5 { 0..5 } else { 5..10 };
            let mut links: Vec<String> =
                side.filter(|&j| j != i).map(|j| format!("n{j}")).collect();
            if i == 4 {
                links.push("n5".into());
            }
            if i == 5 {
                links.push("n4".into());
            }
            atoms.push(
                serde_json::json!({"id": format!("n{i}"), "text": "x", "links": links})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
        let graph = Graph::from_atoms(&atoms);
        let found = communities(&graph);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0], vec![0, 1, 2, 3, 4]);
        assert_eq!(found[1], vec![5, 6, 7, 8, 9]);
        let q = modularity(&graph, &found);
        assert!(q > 0.4, "modularity {q}");
        assert!(q >= modularity(&graph, &islands(&graph)) - 1e-9);
        assert!(
            modularity(&graph, &[(0..10).collect()]).abs() < 1e-9,
            "one community is zero"
        );
        // Two cliques of the same size share a signature; a different size does not.
        assert_eq!(signature(&graph, &found[0]), signature(&graph, &found[1]));
        assert_ne!(
            signature(&graph, &found[0]),
            signature(&graph, &found[0][..3])
        );
    }

    #[test]
    fn two_cliques_are_two_islands() {
        let mut atoms = clique("a", 4);
        atoms.extend(clique("b", 4));
        atoms[0]["links"].as_array_mut().unwrap().push(json!("b0"));
        let graph = Graph::from_atoms(&atoms);
        let found = islands(&graph);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].len(), 4);
        let a1 = graph.position("a1").unwrap();
        let lit = activate(&graph, &[(a1, 1.0)], 2);
        let score = |id: &str| {
            lit.iter()
                .find(|(n, _)| graph.id(*n) == id)
                .map_or(0.0, |(_, a)| *a)
        };
        for own in ["a0", "a2", "a3"] {
            for other in ["b1", "b2", "b3"] {
                assert!(score(own) > score(other), "{own} {other} {lit:?}");
            }
        }
        assert_eq!(lit[0].0, a1);
    }

    /// Firing a pair raises its weight toward one and decays the links they
    /// did not fire with; the heavier link then carries more activation.
    #[test]
    fn a_lens_moves_its_own_weights_and_leaves_the_shared_ones() {
        let mut atoms: Vec<Record> = ["a", "b", "c"]
            .iter()
            .map(|id| {
                serde_json::json!({"id": id, "text": id, "links": []})
                    .as_object()
                    .unwrap()
                    .clone()
            })
            .collect();
        // Through the lens: a and b wire and weigh up under "reviewer" only.
        let changed = fire_as(&mut atoms, &[0, 1], Some("reviewer"));
        assert_eq!(changed.len(), 2);
        assert!(has_link(&atoms[0], "b"), "the link made is the pack's");
        assert_eq!(
            weight_of(&atoms[0], "b"),
            WEIGHT_DEFAULT,
            "the shared weight stands"
        );
        assert!(weight_of_as(&atoms[0], "b", Some("reviewer")) > WEIGHT_DEFAULT);
        assert_eq!(
            weight_of_as(&atoms[0], "b", Some("reader")),
            WEIGHT_DEFAULT,
            "another lens reads the shared weight"
        );
        // The seat's own fire moves the shared weight and not the lens.
        let before = weight_of_as(&atoms[0], "b", Some("reviewer"));
        fire(&mut atoms, &[0, 1]);
        assert!(weight_of(&atoms[0], "b") > WEIGHT_DEFAULT);
        assert_eq!(weight_of_as(&atoms[0], "b", Some("reviewer")), before);
        // The two graphs differ on that edge alone.
        let shared = Graph::from_atoms(&atoms);
        let lensed = Graph::from_atoms_as(&atoms, Some("reviewer"));
        assert_eq!(shared.ids, lensed.ids);
    }

    #[test]
    fn fire_together_wire_together() {
        let mut atoms = clique("a", 3);
        let graph = Graph::from_atoms(&atoms);
        assert_eq!(graph.weight(0, 1), Some(WEIGHT_DEFAULT));
        let changed = fire(&mut atoms, &[0, 1]);
        assert_eq!(changed, vec![0, 1, 2], "the decayed peer is written too");
        let graph = Graph::from_atoms(&atoms);
        let w1 = graph.weight(0, 1).unwrap();
        assert!(w1 > WEIGHT_DEFAULT && w1 < 1.0, "{w1}");
        let w2_cold = graph.weight(0, 2).unwrap();
        assert!(w2_cold < WEIGHT_DEFAULT, "{w2_cold}");
        fire(&mut atoms, &[0, 1]);
        let graph = Graph::from_atoms(&atoms);
        assert!(graph.weight(0, 1).unwrap() > w1);
        let lit = activate(&graph, &[(0, 1.0)], 1);
        let at = |n: usize| lit.iter().find(|(i, _)| *i == n).map_or(0.0, |(_, a)| *a);
        assert!(at(1) > at(2), "{lit:?}");
        // Two lone claims that fire together gain a link.
        let mut pair = vec![
            json!({"id": "x"}).as_object().cloned().unwrap(),
            json!({"id": "y"}).as_object().cloned().unwrap(),
        ];
        assert_eq!(fire(&mut pair, &[0, 1]), vec![0, 1]);
        assert!(has_link(&pair[0], "y") && has_link(&pair[1], "x"));
        assert!(fire(&mut pair, &[0]).is_empty());
    }

    /// An atom with no links is its own island and activates nothing else.
    #[test]
    fn a_lone_atom_is_an_island() {
        let atoms = vec![json!({"id": "solo"}).as_object().cloned().unwrap()];
        let graph = Graph::from_atoms(&atoms);
        assert_eq!(islands(&graph), vec![vec![0]]);
        assert_eq!(activate(&graph, &[(0, 1.0)], 3), vec![(0, 1.0)]);
    }
}
