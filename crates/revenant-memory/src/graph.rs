//! The graph leg: Personalized PageRank over the entity neighborhood
//! (HippoRAG-style). The expensive knowledge-graph build happens at write
//! time; this read path is a few matrix-vector iterations over ≤512 edges —
//! microseconds, no LLM.

use crate::index::EdgeRow;
use petgraph::graphmap::DiGraphMap;
use std::collections::HashMap;

/// Damping. 0.5 (vs the web-classic 0.85) keeps the query's seed entities
/// dominant — facts about the entity you asked about must outrank facts
/// about its neighbors — while still propagating useful multi-hop mass.
const ALPHA: f32 = 0.5;
const MAX_ITERS: usize = 30;
const EPSILON: f32 = 1e-6;
/// Reverse-direction edges count, slightly discounted.
const REVERSE_DISCOUNT: f32 = 0.7;

/// Personalized PageRank seeded at `seeds` (entity id -> teleport weight).
pub fn personalized_pagerank(
    edges: &[EdgeRow],
    seeds: &HashMap<i64, f32>,
) -> HashMap<i64, f32> {
    if edges.is_empty() || seeds.is_empty() {
        return seeds.clone();
    }

    let mut graph: DiGraphMap<i64, f32> = DiGraphMap::new();
    for edge in edges {
        // Accumulate parallel edges; add discounted reverse direction.
        let forward = graph.edge_weight(edge.src, edge.dst).copied().unwrap_or(0.0);
        graph.add_edge(edge.src, edge.dst, forward + edge.weight);
        let backward = graph.edge_weight(edge.dst, edge.src).copied().unwrap_or(0.0);
        graph.add_edge(edge.dst, edge.src, backward + edge.weight * REVERSE_DISCOUNT);
    }
    for &seed in seeds.keys() {
        graph.add_node(seed);
    }

    let nodes: Vec<i64> = graph.nodes().collect();
    let index: HashMap<i64, usize> = nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();

    // Teleport vector, normalized.
    let total: f32 = seeds.values().sum();
    let mut teleport = vec![0.0f32; nodes.len()];
    for (&node, &weight) in seeds {
        if let Some(&i) = index.get(&node) {
            teleport[i] = weight / total;
        }
    }

    // Out-weight sums for normalization.
    let mut out_weight = vec![0.0f32; nodes.len()];
    for &node in &nodes {
        let i = index[&node];
        out_weight[i] = graph
            .edges(node)
            .map(|(_, _, w)| *w)
            .sum();
    }

    let mut rank = teleport.clone();
    let mut next = vec![0.0f32; nodes.len()];
    for _ in 0..MAX_ITERS {
        next.copy_from_slice(&teleport);
        for x in next.iter_mut() {
            *x *= 1.0 - ALPHA;
        }
        for &node in &nodes {
            let i = index[&node];
            if rank[i] == 0.0 || out_weight[i] == 0.0 {
                // Dangling mass teleports back to seeds.
                for (j, t) in teleport.iter().enumerate() {
                    next[j] += ALPHA * rank[i] * t;
                }
                continue;
            }
            let share = ALPHA * rank[i] / out_weight[i];
            for (_, dst, w) in graph.edges(node) {
                next[index[&dst]] += share * w;
            }
        }
        let delta: f32 = rank.iter().zip(&next).map(|(a, b)| (a - b).abs()).sum();
        std::mem::swap(&mut rank, &mut next);
        if delta < EPSILON {
            break;
        }
    }

    nodes.iter().map(|&n| (n, rank[index[&n]])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(src: i64, dst: i64, weight: f32) -> EdgeRow {
        EdgeRow { src, dst, weight }
    }

    #[test]
    fn ppr_decays_with_distance_from_seed() {
        // Chain 1 -> 2 -> 3 -> 4 seeded at 1: rank decays monotonically with
        // distance, and the seed itself stays on top (α=0.5 guarantees it).
        let edges = vec![edge(1, 2, 1.0), edge(2, 3, 1.0), edge(3, 4, 1.0)];
        let seeds = HashMap::from([(1i64, 1.0f32)]);
        let ranks = personalized_pagerank(&edges, &seeds);
        assert!(ranks[&1] > ranks[&2]);
        assert!(ranks[&2] > ranks[&3]);
        assert!(ranks[&3] > ranks[&4]);
    }

    #[test]
    fn multi_seed_blends() {
        let edges = vec![edge(1, 2, 1.0), edge(3, 4, 1.0)];
        let seeds = HashMap::from([(1i64, 1.0f32), (3i64, 0.5f32)]);
        let ranks = personalized_pagerank(&edges, &seeds);
        assert!(ranks[&2] > 0.0);
        assert!(ranks[&4] > 0.0);
        assert!(ranks[&2] > ranks[&4]); // stronger seed side wins
    }
}
