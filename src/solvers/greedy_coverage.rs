use std::cmp::Ordering;
use std::collections::BinaryHeap;

use framework::{CoverageGoals, Instance, IntervalSet, Point, Solution, Solver};
use rayon::prelude::*;

use crate::coverage::{CoverageConfig, CoverageData, build_coverage};

#[derive(Default)]
pub struct GreedyCoverageSolver {
    pub coverage: CoverageConfig,
}

pub struct CoverabilityGreedySolver {
    pub coverage: CoverageConfig,
    pub alpha: f64,
}

impl GreedyCoverageSolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_coverage_config(mut self, coverage: CoverageConfig) -> Self {
        self.coverage = coverage;
        self
    }
}

impl Default for CoverabilityGreedySolver {
    fn default() -> Self {
        Self {
            coverage: CoverageConfig::default(),
            alpha: 1.0,
        }
    }
}

impl CoverabilityGreedySolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_coverage_config(mut self, coverage: CoverageConfig) -> Self {
        self.coverage = coverage;
        self
    }

    pub fn with_alpha(mut self, alpha: f64) -> Self {
        self.alpha = alpha;
        self
    }
}

impl Solver for GreedyCoverageSolver {
    fn solve(&self, instance: Instance) -> Solution {
        let data = build_coverage(&instance, &self.coverage);
        let goals = CoverageGoals::new(&data.perimeters, instance.tau);
        let (chosen, _) = select_indices(&data, &goals, instance.k);
        let antennas = chosen
            .iter()
            .map(|&i| data.candidates[i].position)
            .collect();
        Solution { antennas }
    }
}

impl Solver for CoverabilityGreedySolver {
    fn solve(&self, instance: Instance) -> Solution {
        let data = build_coverage(&instance, &self.coverage);
        let goals = WeightedCoverageGoals::coverability(&data, instance.tau, self.alpha);
        let (chosen, _) = select_indices_with_goals(&data, &goals, instance.k);
        let antennas = chosen
            .iter()
            .map(|&i| data.candidates[i].position)
            .collect();
        Solution { antennas }
    }
}

pub fn select_antennas(data: &CoverageData, k: usize, tau: f64) -> (Vec<Point>, usize) {
    let goals = CoverageGoals::new(&data.perimeters, tau);
    let (chosen, acc) = select_indices(data, &goals, k);
    let antennas = chosen
        .iter()
        .map(|&i| data.candidates[i].position)
        .collect();
    let serviced = (0..goals.needs.len())
        .filter(|&b| serviced(data, &acc, &goals, b))
        .count();
    (antennas, serviced)
}

fn serviced(data: &CoverageData, acc: &[IntervalSet], goals: &CoverageGoals, b: usize) -> bool {
    data.perimeters[b] > 0.0 && acc[b].measure() >= goals.needs[b]
}

#[derive(PartialEq)]
struct Gain(f64);

impl Eq for Gain {}

impl Ord for Gain {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl PartialOrd for Gain {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) fn select_indices(
    data: &CoverageData,
    goals: &CoverageGoals,
    k: usize,
) -> (Vec<usize>, Vec<IntervalSet>) {
    select_indices_with_goals(data, goals, k)
}

pub(crate) fn select_coverability_indices(
    data: &CoverageData,
    k: usize,
    tau: f64,
    alpha: f64,
) -> (Vec<usize>, Vec<IntervalSet>) {
    let goals = WeightedCoverageGoals::coverability(data, tau, alpha);
    select_indices_with_goals(data, &goals, k)
}

trait GreedyGoals: Sync {
    fn gain(&self, view: &[(u32, IntervalSet)], acc: &[IntervalSet], covered: &[f64]) -> f64;
}

impl GreedyGoals for CoverageGoals {
    fn gain(&self, view: &[(u32, IntervalSet)], acc: &[IntervalSet], covered: &[f64]) -> f64 {
        self.gain(view, acc, covered)
    }
}

#[derive(Clone, Debug)]
pub struct WeightedCoverageGoals {
    pub needs: Vec<f64>,
    pub weights: Vec<f64>,
}

impl WeightedCoverageGoals {
    pub fn coverability(data: &CoverageData, tau: f64, alpha: f64) -> Self {
        let needs: Vec<f64> = data.perimeters.iter().map(|&p| tau * p).collect();
        let utility = candidate_utilities(data, &needs);
        let baseline = median_positive(utility.iter().copied()).unwrap_or(1.0);
        let costs: Vec<f64> = utility
            .iter()
            .map(|&u| if u > 0.0 { baseline / u } else { 1_000_000.0 })
            .collect();
        let finish_costs = finish_costs(data, &needs, &costs);
        let median_finish = median_positive(finish_costs.iter().copied()).unwrap_or(1.0);
        let weights: Vec<f64> = finish_costs
            .iter()
            .map(|&cost| {
                if cost.is_finite() && cost > 0.0 {
                    (median_finish / cost).powf(alpha).clamp(0.2, 3.0)
                } else {
                    0.2
                }
            })
            .collect();
        Self { needs, weights }
    }
}

impl GreedyGoals for WeightedCoverageGoals {
    fn gain(&self, view: &[(u32, IntervalSet)], acc: &[IntervalSet], covered: &[f64]) -> f64 {
        view.iter()
            .map(|(b, set)| {
                let b = *b as usize;
                self.weights[b]
                    * (self.needs[b].min(acc[b].union_measure(set)) - self.needs[b].min(covered[b]))
            })
            .sum()
    }
}

fn candidate_utilities(data: &CoverageData, needs: &[f64]) -> Vec<f64> {
    data.visibility
        .par_iter()
        .map(|view| {
            view.iter()
                .map(|(b, set)| needs[*b as usize].min(set.measure()))
                .sum::<f64>()
        })
        .collect()
}

fn finish_costs(data: &CoverageData, needs: &[f64], costs: &[f64]) -> Vec<f64> {
    let mut by_building: Vec<Vec<(usize, &IntervalSet)>> = vec![Vec::new(); needs.len()];
    for (candidate, view) in data.visibility.iter().enumerate() {
        for (b, set) in view {
            by_building[*b as usize].push((candidate, set));
        }
    }
    by_building
        .into_par_iter()
        .enumerate()
        .map(|(b, views)| finish_cost(needs[b], &views, costs))
        .collect()
}

fn finish_cost(need: f64, views: &[(usize, &IntervalSet)], costs: &[f64]) -> f64 {
    if need <= 0.0 {
        return 0.0;
    }
    let mut acc = IntervalSet::new();
    let mut covered = 0.0;
    let mut total_cost = 0.0;
    let mut used = Vec::new();
    for _ in 0..8 {
        let mut best = None;
        let mut best_ratio = 0.0;
        let mut best_gain = 0.0;
        for (pos, &(candidate, set)) in views.iter().enumerate() {
            if used.contains(&pos) {
                continue;
            }
            let gain = acc.union_measure(set) - covered;
            let ratio = gain / costs[candidate].max(1e-9);
            if ratio > best_ratio {
                best_ratio = ratio;
                best_gain = gain;
                best = Some(pos);
            }
        }
        let Some(pos) = best else {
            return f64::INFINITY;
        };
        if best_gain <= 1e-9 {
            return f64::INFINITY;
        }
        used.push(pos);
        let (candidate, set) = views[pos];
        total_cost += costs[candidate];
        acc.union_with(set);
        covered = acc.measure();
        if covered >= need {
            return total_cost;
        }
    }
    f64::INFINITY
}

fn median_positive(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let mut values: Vec<f64> = values
        .into_iter()
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f64::total_cmp);
    Some(values[values.len() / 2])
}

fn select_indices_with_goals(
    data: &CoverageData,
    goals: &impl GreedyGoals,
    k: usize,
) -> (Vec<usize>, Vec<IntervalSet>) {
    let mut acc: Vec<IntervalSet> = vec![IntervalSet::new(); data.perimeters.len()];
    let mut covered = vec![0.0f64; data.perimeters.len()];
    let mut heap: BinaryHeap<(Gain, usize)> = (0..data.candidates.len())
        .into_par_iter()
        .map(|i| (Gain(goals.gain(&data.visibility[i], &acc, &covered)), i))
        .collect::<Vec<_>>()
        .into_iter()
        .collect();
    let mut chosen = Vec::with_capacity(k.min(data.candidates.len()));
    let mut used = vec![false; data.candidates.len()];
    while chosen.len() < k {
        let Some((stale, i)) = heap.pop() else { break };
        if used[i] {
            continue;
        }
        let gain = goals.gain(&data.visibility[i], &acc, &covered);
        debug_assert!(gain <= stale.0 + 1e-9);
        if gain < heap.peek().map_or(0.0, |(top, _)| top.0) {
            heap.push((Gain(gain), i));
            continue;
        }
        used[i] = true;
        chosen.push(i);
        for (b, set) in &data.visibility[i] {
            let b = *b as usize;
            acc[b].union_with(set);
            covered[b] = acc[b].measure();
        }
    }
    (chosen, acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use framework::test_utils::{serviced_ids, square};

    fn vertices_only() -> CoverageConfig {
        CoverageConfig {
            candidate_spacing: 100.0,
            event_candidate_radius: 0.0,
            event_candidate_limit: 0,
        }
    }

    #[test]
    fn one_corner_services_square_at_half_tau() {
        let instance = Instance {
            buildings: vec![square(5, 0.0, 0.0)],
            k: 1,
            tau: 0.5,
        };
        let solver = GreedyCoverageSolver::new().with_coverage_config(vertices_only());
        let solution = solver.solve(instance.clone());
        assert_eq!(solution.antennas.len(), 1);
        assert_eq!(serviced_ids(&instance, &solution), vec![5]);
    }

    #[test]
    fn high_tau_unreachable_with_one_antenna() {
        let instance = Instance {
            buildings: vec![square(5, 0.0, 0.0)],
            k: 1,
            tau: 0.75,
        };
        let solver = GreedyCoverageSolver::new().with_coverage_config(vertices_only());
        let solution = solver.solve(instance.clone());
        assert_eq!(solution.antennas.len(), 1);
        assert!(serviced_ids(&instance, &solution).is_empty());
    }

    #[test]
    fn surrogate_diverts_to_both_clusters() {
        let instance = Instance {
            buildings: vec![square(1, 0.0, 0.0), square(2, 100.0, 0.0)],
            k: 4,
            tau: 0.75,
        };
        let solver = GreedyCoverageSolver::new().with_coverage_config(vertices_only());
        let solution = solver.solve(instance.clone());
        assert_eq!(solution.antennas.len(), 4);
        let left = solution.antennas.iter().filter(|a| a.x <= 10.0).count();
        let right = solution.antennas.iter().filter(|a| a.x >= 100.0).count();
        assert_eq!((left, right), (2, 2));
        assert_eq!(serviced_ids(&instance, &solution), vec![1, 2]);
    }

    #[test]
    fn places_exactly_k_even_when_saturated() {
        let instance = Instance {
            buildings: vec![square(5, 0.0, 0.0)],
            k: 3,
            tau: 0.25,
        };
        let solver = GreedyCoverageSolver::new().with_coverage_config(vertices_only());
        let solution = solver.solve(instance.clone());
        assert_eq!(solution.antennas.len(), 3);
        for (i, a) in solution.antennas.iter().enumerate() {
            for b in &solution.antennas[i + 1..] {
                assert_ne!(a, b, "duplicate antenna");
            }
        }
        assert_eq!(serviced_ids(&instance, &solution), vec![5]);
    }

    #[test]
    fn k_larger_than_candidate_pool() {
        let instance = Instance {
            buildings: vec![square(5, 0.0, 0.0)],
            k: 10,
            tau: 0.5,
        };
        let solver = GreedyCoverageSolver::new().with_coverage_config(vertices_only());
        let solution = solver.solve(instance.clone());
        assert_eq!(solution.antennas.len(), 4);
        assert_eq!(serviced_ids(&instance, &solution), vec![5]);
    }
}
