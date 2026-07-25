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

// NEGATIVE RESULT — do not re-attempt typed traversal by reweighting edges.
//
// `EdgeRow.rel` is now read back from the index (it used to be dropped), and the
// obvious use of it is: when the query names a relation ("the pet *belonging to*
// the person who *works at* Nimbus"), multiply that relation's edges so PPR
// follows the intended chain. That was implemented, unit-tested, and measured on
// the retrieval evals. It moved NOTHING: multi-hop stayed 8/9 and MRR stayed
// 0.885, byte-identical to not doing it.
//
// The reason is structural, in the iteration below: mass is distributed as
// `ALPHA * rank[i] / out_weight[i] * w`, i.e. each edge's share is normalized by
// the node's TOTAL out-weight. Scaling a node's edges by a constant cancels in
// that ratio, so a boost only shifts anything when a node has a mix of boosted
// and unboosted out-edges — and even then it competes against α-decay, which
// dominates at the hop distances multi-hop questions care about. Reweighting
// cannot express "follow THIS predicate"; it only re-scores an undirected
// diffusion.
//
// Real typed traversal therefore needs a different shape: walk the named
// predicate chain to identify the ANSWER entity, then retrieve that entity's
// facts directly — path-aware retrieval, not a reweighted random walk. `rel` is
// plumbed through and available for exactly that.
//
// What did fix the multi-hop miss was unrelated to edge weights: a per-entity
// fact cap in the graph leg (see MAX_FACTS_PER_ENTITY in retrieve.rs) — the leg
// was emitting strict entity-rank order, so a distant entity's fact could never
// reach the top-K behind a near entity's many facts.

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
        EdgeRow { src, dst, weight, rel: "mentions".into() }
    }

    /// Guards the negative result documented above: a uniform reweighting of a
    /// node's out-edges cannot steer PPR, because each edge's share is
    /// normalized by the node's total out-weight. Anyone tempted to implement
    /// typed traversal by scaling `EdgeRow.weight` should see this fail-safe
    /// first — scaling every edge 3x leaves the ranking identical.
    #[test]
    fn uniform_edge_reweighting_cannot_steer_ppr() {
        let edges = vec![edge(1, 2, 1.0), edge(2, 3, 1.0), edge(1, 4, 1.0)];
        let scaled: Vec<EdgeRow> =
            edges.iter().map(|e| EdgeRow { weight: e.weight * 3.0, ..e.clone() }).collect();
        let seeds = HashMap::from([(1i64, 1.0f32)]);

        let base = personalized_pagerank(&edges, &seeds);
        let boosted = personalized_pagerank(&scaled, &seeds);
        for node in [1i64, 2, 3, 4] {
            assert!(
                (base[&node] - boosted[&node]).abs() < 1e-6,
                "node {node}: out-weight normalization must cancel a uniform boost ({} vs {})",
                base[&node],
                boosted[&node]
            );
        }
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
