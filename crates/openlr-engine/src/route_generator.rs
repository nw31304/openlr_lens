use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

use crate::trace::ScoredCandidate;

/// Ord wrapper for finite f64 scores (NaN treated as equal).
#[derive(Clone, Copy, PartialEq)]
struct Score(f64);

impl Eq for Score {}

impl PartialOrd for Score {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(&o.0)
    }
}

impl Ord for Score {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.partial_cmp(o).unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// Lazy iterator over candidate-index combinations in ascending order of total
/// candidate score.
///
/// Given N LRPs, each with a scored, sorted candidate list, the generator
/// yields `Vec<usize>` index vectors — one index per LRP — in order of
/// increasing sum of `candidates[k][indices[k]].score.total`.
///
/// The caller tests whether the yielded combination is actually routable (A*
/// may still fail); the generator simply provides the globally cheapest
/// untried combination at every step without materialising the full Kᴺ
/// Cartesian product.
///
/// **Algorithm** (identical to the approach in `rustlr/openlr/route_generator.rs`):
/// - Seed the heap with `[0, 0, …, 0]` (top candidate at every LRP).
/// - On each `next()`: pop the best combination, then push up-to-N "neighbour"
///   combinations formed by incrementing one index at a time.  A `HashSet`
///   prevents duplicate pushes when two different paths reach the same
///   combination.
///
/// This runs in O(m · N · log(m · N)) time to find the first m routable
/// combinations — polynomial, not exponential.
pub struct RouteGenerator<'a> {
    candidates: &'a [Vec<ScoredCandidate>],
    heap: BinaryHeap<(Reverse<Score>, Vec<usize>)>,
    seen: HashSet<Vec<usize>>,
}

impl<'a> RouteGenerator<'a> {
    pub fn new(candidates: &'a [Vec<ScoredCandidate>]) -> Self {
        let mut heap = BinaryHeap::new();
        let mut seen = HashSet::new();

        if !candidates.is_empty() && candidates.iter().all(|c| !c.is_empty()) {
            let initial: Vec<usize> = vec![0; candidates.len()];
            let score = combo_score(candidates, &initial);
            heap.push((Reverse(Score(score)), initial.clone()));
            seen.insert(initial);
        }

        RouteGenerator { candidates, heap, seen }
    }
}

pub fn combo_score(candidates: &[Vec<ScoredCandidate>], indices: &[usize]) -> f64 {
    indices.iter()
        .enumerate()
        .map(|(lrp, &ci)| candidates[lrp][ci].score.total)
        .sum()
}

impl<'a> Iterator for RouteGenerator<'a> {
    type Item = Vec<usize>;

    fn next(&mut self) -> Option<Vec<usize>> {
        let (_, indices) = self.heap.pop()?;

        for k in 0..indices.len() {
            if indices[k] + 1 < self.candidates[k].len() {
                let mut next = indices.clone();
                next[k] += 1;
                if self.seen.insert(next.clone()) {
                    let score = combo_score(self.candidates, &next);
                    self.heap.push((Reverse(Score(score)), next));
                }
            }
        }

        Some(indices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{CandidateScore, ProjectionResult, ScoredCandidate, TraversalDir};
    use openlr_graph::{NodeId, SegmentId};

    fn cand(score: f64) -> ScoredCandidate {
        ScoredCandidate {
            segment_id: SegmentId(0),
            traversal: TraversalDir::Forward,
            projection: ProjectionResult {
                arc_offset_m: 0.0, point: (0.0, 0.0),
                distance_m: score, bearing_deg: 0.0,
            },
            score: CandidateScore {
                positional_m: score, bearing_excess_deg: 0.0,
                frc_penalty: 0.0, fow_penalty: 0.0, total: score,
            },
            entry_node: NodeId(0),
            exit_node:  NodeId(1),
        }
    }

    #[test]
    fn single_lrp_enumerates_in_order() {
        let candidates = vec![vec![cand(1.0), cand(2.0), cand(3.0)]];
        let out: Vec<_> = RouteGenerator::new(&candidates).collect();
        assert_eq!(out, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn two_lrps_best_first_order() {
        // LRP0: scores 1, 3   LRP1: scores 2, 4
        // Expected order: (0,0)=3, (1,0)=5, (0,1)=5, (1,1)=7
        let candidates = vec![
            vec![cand(1.0), cand(3.0)],
            vec![cand(2.0), cand(4.0)],
        ];
        let scores: Vec<f64> = RouteGenerator::new(&candidates)
            .map(|idx| combo_score(&candidates, &idx))
            .collect();
        assert_eq!(scores, vec![3.0, 5.0, 5.0, 7.0]);
    }

    #[test]
    fn no_duplicates() {
        let candidates = vec![
            vec![cand(1.0), cand(2.0)],
            vec![cand(1.0), cand(2.0)],
        ];
        let out: Vec<_> = RouteGenerator::new(&candidates).collect();
        assert_eq!(out.len(), 4, "should produce exactly 2×2=4 combinations");
        let mut seen = HashSet::new();
        for c in &out {
            assert!(seen.insert(c.clone()), "duplicate combination: {c:?}");
        }
    }

    #[test]
    fn empty_candidate_list_yields_nothing() {
        let candidates: Vec<Vec<ScoredCandidate>> = vec![vec![], vec![]];
        assert!(RouteGenerator::new(&candidates).next().is_none());
    }

    #[test]
    fn three_lrps_total_count() {
        // 2 × 2 × 2 = 8 combinations, no more, no less.
        let candidates = vec![
            vec![cand(1.0), cand(10.0)],
            vec![cand(1.0), cand(10.0)],
            vec![cand(1.0), cand(10.0)],
        ];
        let out: Vec<_> = RouteGenerator::new(&candidates).collect();
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn scores_non_decreasing() {
        let candidates = vec![
            vec![cand(1.0), cand(4.0), cand(9.0)],
            vec![cand(2.0), cand(5.0), cand(8.0)],
            vec![cand(3.0), cand(6.0), cand(7.0)],
        ];
        let scores: Vec<f64> = RouteGenerator::new(&candidates)
            .map(|idx| combo_score(&candidates, &idx))
            .collect();
        for w in scores.windows(2) {
            assert!(w[0] <= w[1], "scores not non-decreasing: {w:?}");
        }
    }
}
