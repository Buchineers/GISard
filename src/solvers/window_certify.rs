
use std::collections::HashMap;

use framework::{CoverageGoals, Evaluator, Instance, IntervalSet, Point, Solution, evaluate};
use good_lp::solvers::SolutionStatus;
use good_lp::solvers::highs::highs;
use good_lp::{
    Expression, Solution as LpSolution, SolverModel, Variable, constraint, variable, variables,
};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use rayon::prelude::*;

use crate::coverage::{CoverageConfig, CoverageData, build_coverage, extend_coverage_with};
use crate::sampling::{Candidate, CandidateKind};
use crate::solvers::exact_cover::Atom;

const EPS: f64 = 1e-9;

const INCUMBENT_SNAP: f64 = 1.0e4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowCentres {
    #[default]
    Antenna,
    OpenBuilding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shard {
    pub index: usize,
    pub count: usize,
}

impl Shard {
    pub fn new(index: usize, count: usize) -> Self {
        assert!(count > 0, "shard count must be positive");
        assert!(index < count, "shard index {index} out of range for {count}");
        Shard { index, count }
    }

    fn keeps(&self, position: usize) -> bool {
        position % self.count == self.index
    }
}

pub struct WindowOptions {
    pub antennas: usize,
    pub windows: usize,
    pub time_limit: f64,
    pub reach: f64,
    pub max_radius: Option<f64>,
    pub coverage: CoverageConfig,
    pub centres: WindowCentres,
    pub shard: Option<Shard>,
    pub seed: u64,
    pub verbose: bool,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            antennas: 3,
            windows: 100,
            time_limit: 30.0,
            reach: 1.5,
            max_radius: None,
            coverage: CoverageConfig::default(),
            centres: WindowCentres::default(),
            shard: None,
            seed: 42,
            verbose: false,
        }
    }
}

pub struct WindowResult {
    pub affected_buildings: Vec<u32>,
    pub decisive: usize,
    pub candidates: usize,
    pub incumbent: usize,
    pub best: usize,
    pub proved: bool,
    pub improvement: Option<Vec<Point>>,
    pub freed: Vec<usize>,
}

impl WindowResult {
    pub fn apply(&self, solution: &Solution) -> Option<Solution> {
        let replacement = self.improvement.as_ref()?;
        let mut antennas: Vec<Point> = solution
            .antennas
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.freed.contains(index))
            .map(|(_, &antenna)| antenna)
            .collect();
        antennas.extend(replacement.iter().copied());
        Some(Solution { antennas })
    }
}

impl WindowResult {
    pub fn affected(&self) -> usize {
        self.affected_buildings.len()
    }

    pub fn improved(&self) -> bool {
        self.best > self.incumbent
    }

    pub fn certified(&self) -> bool {
        self.proved && self.best <= self.incumbent
    }
}

pub struct WindowReport {
    pub results: Vec<WindowResult>,
    pub serviced: usize,
}

impl WindowReport {
    pub fn certified(&self) -> usize {
        self.results.iter().filter(|r| r.certified()).count()
    }

    pub fn improved(&self) -> usize {
        self.results.iter().filter(|r| r.improved()).count()
    }

    pub fn inconclusive(&self) -> usize {
        self.results
            .iter()
            .filter(|r| !r.certified() && !r.improved())
            .count()
    }

    pub fn total_gain(&self) -> usize {
        self.results
            .iter()
            .map(|r| r.best.saturating_sub(r.incumbent))
            .sum()
    }
}

pub fn repair_windows(
    instance: &Instance,
    solution: &Solution,
    options: &WindowOptions,
    rounds: usize,
) -> (Solution, Vec<usize>) {
    let mut current = Solution {
        antennas: solution.antennas.clone(),
    };
    let mut history = vec![evaluate(instance, &current).score];
    for round in 0..rounds {
        let seeded = WindowOptions {
            seed: options.seed.wrapping_add(round as u64),
            ..clone_options(options)
        };
        let report = certify_windows(instance, &current, &seeded);
        let (merged, applied) = apply_window_results(instance, &current, &report.results);
        current = merged;
        let score = evaluate(instance, &current).score;
        if options.verbose {
            eprintln!(
                "window repair round {}: applied {applied}, {} serviced",
                round + 1,
                score,
            );
        }
        history.push(score);
        if applied == 0 {
            break;
        }
    }
    (current, history)
}

pub fn apply_window_results(
    instance: &Instance,
    solution: &Solution,
    results: &[WindowResult],
) -> (Solution, usize) {
    let current = Solution {
        antennas: solution.antennas.clone(),
    };
    let mut improvements: Vec<&WindowResult> = results.iter().filter(|r| r.improved()).collect();
    improvements.sort_by(|a, b| {
        (b.best - b.incumbent)
            .cmp(&(a.best - a.incumbent))
            .then_with(|| a.freed.cmp(&b.freed))
    });

    let mut claimed: Vec<u32> = Vec::new();
    let mut freed = vec![false; current.antennas.len()];
    let mut replacements = Vec::new();
    let mut applied = 0;
    for window in improvements {
        if window
            .affected_buildings
            .iter()
            .any(|b| claimed.binary_search(b).is_ok())
            || window.freed.iter().any(|&antenna| freed[antenna])
        {
            continue;
        }
        let Some(replacement) = &window.improvement else {
            continue;
        };
        for &antenna in &window.freed {
            freed[antenna] = true;
        }
        replacements.extend(replacement.iter().copied());
        claimed.extend_from_slice(&window.affected_buildings);
        claimed.sort_unstable();
        applied += 1;
    }
    if applied == 0 {
        return (current, 0);
    }
    let mut antennas: Vec<_> = current
        .antennas
        .iter()
        .enumerate()
        .filter(|(index, _)| !freed[*index])
        .map(|(_, &antenna)| antenna)
        .collect();
    antennas.extend(replacements);
    let proposed = Solution { antennas };
    if evaluate(instance, &proposed).score >= evaluate(instance, &current).score {
        (proposed, applied)
    } else {
        (current, 0)
    }
}

fn clone_options(options: &WindowOptions) -> WindowOptions {
    WindowOptions {
        antennas: options.antennas,
        windows: options.windows,
        time_limit: options.time_limit,
        reach: options.reach,
        max_radius: options.max_radius,
        coverage: options.coverage.clone(),
        centres: options.centres,
        shard: options.shard,
        seed: options.seed,
        verbose: options.verbose,
    }
}

pub fn certify_windows(
    instance: &Instance,
    solution: &Solution,
    options: &WindowOptions,
) -> WindowReport {
    let mut data = build_coverage(instance, &options.coverage);
    let evaluator = Evaluator::new(instance);

    let incumbent_pool: Vec<Candidate> = solution
        .antennas
        .iter()
        .map(|&position| Candidate {
            position,
            building: evaluator
                .snap(position, INCUMBENT_SNAP)
                .map_or(0, |(building, _, _)| building as u32),
            kind: CandidateKind::Incumbent,
        })
        .collect();
    extend_coverage_with(&evaluator, &mut data, incumbent_pool);

    let goals = CoverageGoals::new(&data.perimeters, instance.tau);
    let evaluation = evaluate(instance, solution);

    let views: Vec<Vec<(u32, IntervalSet)>> = solution
        .antennas
        .par_iter()
        .map(|&antenna| evaluator.antenna_view(antenna).covered)
        .collect();

    let specs = match options.centres {
        WindowCentres::Antenna => antenna_centres(solution, options),
        WindowCentres::OpenBuilding => {
            open_building_centres(instance, solution, &evaluation, &goals, options)
        }
    };
    let specs: Vec<WindowSpec> = match options.shard {
        Some(shard) => specs
            .into_iter()
            .enumerate()
            .filter(|(position, _)| shard.keeps(*position))
            .map(|(_, spec)| spec)
            .collect(),
        None => specs,
    };

    let results = specs
        .into_par_iter()
        .map(|spec| certify_one(solution, &data, &goals, &views, &spec, options))
        .collect();

    WindowReport {
        results,
        serviced: evaluation.score,
    }
}

struct WindowSpec {
    freed: Vec<usize>,
    focus: Point,
}

fn nearest_antennas(solution: &Solution, anchor: Point, count: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..solution.antennas.len()).collect();
    order.sort_by(|&a, &b| {
        (solution.antennas[a] - anchor)
            .norm_sq()
            .total_cmp(&(solution.antennas[b] - anchor).norm_sq())
    });
    order.truncate(count);
    order
}

fn antenna_centres(solution: &Solution, options: &WindowOptions) -> Vec<WindowSpec> {
    let mut rng = StdRng::seed_from_u64(options.seed);
    let mut order: Vec<usize> = (0..solution.antennas.len()).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.random_range(0..=i));
    }
    order
        .into_iter()
        .take(options.windows.min(solution.antennas.len().max(1)))
        .map(|centre| {
            let focus = solution.antennas[centre];
            WindowSpec {
                freed: nearest_antennas(solution, focus, options.antennas),
                focus,
            }
        })
        .collect()
}

fn open_building_centres(
    instance: &Instance,
    solution: &Solution,
    evaluation: &framework::Evaluation,
    goals: &CoverageGoals,
    options: &WindowOptions,
) -> Vec<WindowSpec> {
    let mut open: Vec<(usize, f64)> = (0..instance.buildings.len())
        .filter(|&b| !evaluation.serviced[b])
        .filter_map(|b| {
            let need = goals.needs[b];
            (need > 0.0).then(|| (b, evaluation.covered[b].measure() / need))
        })
        .collect();
    open.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut seen: Vec<Vec<usize>> = Vec::new();
    let mut specs = Vec::new();
    for (building, _) in open {
        if specs.len() >= options.windows {
            break;
        }
        let focus = centroid_of(&instance.buildings[building].vertices);
        let mut freed = nearest_antennas(solution, focus, options.antennas);
        freed.sort_unstable();
        if seen.contains(&freed) {
            continue;
        }
        seen.push(freed.clone());
        specs.push(WindowSpec { freed, focus });
    }
    specs
}

fn centroid_of(vertices: &[Point]) -> Point {
    let sum = vertices
        .iter()
        .fold(Point { x: 0.0, y: 0.0 }, |acc, &v| acc + v);
    sum * (1.0 / vertices.len().max(1) as f64)
}

fn window_disc(freed_points: &[Point], focus: Point, options: &WindowOptions) -> (Point, f64) {
    let mut extent = freed_points.to_vec();
    extent.push(focus);
    let (lo, hi) = bbox_of(&extent);
    let mut centre = (lo + hi) * 0.5;
    let own = ((hi - lo).norm() * 0.5).max(1.0);
    let mut radius = own * options.reach.max(1.0);
    if let Some(cap) = options.max_radius {
        radius = own + cap;
    }
    if (focus - centre).norm() > radius {
        centre = focus;
    }
    (centre, radius)
}

fn certify_one(
    solution: &Solution,
    data: &CoverageData,
    goals: &CoverageGoals,
    views: &[Vec<(u32, IntervalSet)>],
    spec: &WindowSpec,
    options: &WindowOptions,
) -> WindowResult {
    let freed = spec.freed.clone();
    let mut is_freed = vec![false; solution.antennas.len()];
    for &antenna in &freed {
        is_freed[antenna] = true;
    }

    let freed_points: Vec<Point> = freed.iter().map(|&a| solution.antennas[a]).collect();
    let (centroid, radius) = window_disc(&freed_points, spec.focus, options);
    let window: Vec<usize> = (0..data.candidates.len())
        .into_par_iter()
        .filter(|&c| (data.candidates[c].position - centroid).norm() <= radius)
        .collect();

    let mut affected: Vec<u32> = window
        .iter()
        .flat_map(|&c| data.visibility[c].iter().map(|(b, _)| *b))
        .chain(freed.iter().flat_map(|&a| views[a].iter().map(|(b, _)| *b)))
        .collect();
    affected.sort_unstable();
    affected.dedup();

    let mut baseline: HashMap<u32, IntervalSet> = HashMap::new();
    for (antenna, view) in views.iter().enumerate() {
        if is_freed[antenna] {
            continue;
        }
        for (building, set) in view {
            if affected.binary_search(building).is_ok() {
                baseline.entry(*building).or_default().union_with(set);
            }
        }
    }
    let empty = IntervalSet::new();
    let baseline_of = |building: u32| baseline.get(&building).unwrap_or(&empty);

    let mut incumbent = 0;
    let mut secured = 0;
    let mut open: Vec<(u32, f64)> = Vec::new();
    for &building in &affected {
        let need = goals.needs[building as usize];
        if need <= 0.0 {
            continue;
        }
        let fixed = baseline_of(building).measure();
        if fixed + EPS >= need {
            secured += 1;
            incumbent += 1;
            continue;
        }
        let mut whole = baseline_of(building).clone();
        for &antenna in &freed {
            if let Some((_, set)) = views[antenna].iter().find(|(b, _)| b == &building) {
                whole.union_with(set);
            }
        }
        if whole.measure() + EPS >= need {
            incumbent += 1;
        }
        open.push((building, need - fixed));
    }

    let residual: HashMap<u32, f64> = open.iter().copied().collect();
    let effective: Vec<Vec<(u32, IntervalSet)>> = window
        .par_iter()
        .map(|&c| {
            data.visibility[c]
                .iter()
                .filter(|(b, _)| residual.contains_key(b))
                .filter_map(|(b, set)| {
                    let gain = set.difference(baseline_of(*b));
                    (!gain.is_empty()).then_some((*b, gain))
                })
                .collect()
        })
        .collect();

    let live: Vec<usize> = (0..window.len())
        .filter(|&slot| !effective[slot].is_empty())
        .collect();
    let pool: Vec<usize> = live.iter().map(|&slot| window[slot]).collect();
    let contributions: Vec<Vec<(u32, IntervalSet)>> =
        live.iter().map(|&slot| effective[slot].clone()).collect();

    let outcome = solve_window(&pool, &contributions, &open, options);
    let best = secured + outcome.serviced;
    let improvement = (best > incumbent).then(|| {
        let mut replacement: Vec<_> = outcome
            .taken
            .iter()
            .map(|&slot| data.candidates[pool[slot]].position)
            .collect();
        replacement.extend(
            freed_points
                .iter()
                .copied()
                .take(freed.len().saturating_sub(replacement.len())),
        );
        replacement
    });
    WindowResult {
        affected_buildings: affected,
        decisive: outcome.decisive,
        candidates: pool.len(),
        incumbent,
        best,
        proved: outcome.proved,
        improvement,
        freed,
    }
}

struct WindowOutcome {
    serviced: usize,
    taken: Vec<usize>,
    proved: bool,
    decisive: usize,
}

fn solve_window(
    window: &[usize],
    effective: &[Vec<(u32, IntervalSet)>],
    open: &[(u32, f64)],
    options: &WindowOptions,
) -> WindowOutcome {
    if open.is_empty() || window.is_empty() {
        return WindowOutcome {
            serviced: 0,
            taken: Vec::new(),
            proved: true,
            decisive: 0,
        };
    }

    let atoms: Vec<(u32, f64, Vec<Atom>)> = open
        .par_iter()
        .map(|&(building, residual)| (building, residual, atoms_over(effective, building)))
        .collect();

    let mut vars = variables!();
    let x: Vec<Variable> = window
        .iter()
        .map(|_| vars.add(variable().binary()))
        .collect();
    let mut objective = Expression::from(0.0);
    let mut rows: Vec<(Expression, f64, Variable)> = Vec::new();
    let mut atom_rows: Vec<(Variable, Vec<usize>)> = Vec::new();
    for (_, residual, atoms) in &atoms {
        if atoms.iter().map(|a| a.length).sum::<f64>() + EPS < *residual {
            continue;
        }
        let y = vars.add(variable().binary());
        objective += y;
        let mut covered = Expression::from(0.0);
        for atom in atoms {
            let z = vars.add(variable().min(0.0).max(1.0));
            atom_rows.push((z, atom.covered_by.iter().map(|&c| c as usize).collect()));
            covered += atom.length * z;
        }
        rows.push((covered, *residual, y));
    }
    let decisive = rows.len();
    if rows.is_empty() {
        return WindowOutcome {
            serviced: 0,
            taken: Vec::new(),
            proved: true,
            decisive: 0,
        };
    }

    let mut problem = vars.maximise(objective.clone()).using(highs);
    problem = problem
        .set_option("primal_feasibility_tolerance", 1e-10)
        .set_option("mip_feasibility_tolerance", 1e-10)
        .set_time_limit(options.time_limit);
    problem.set_verbose(false);

    let budget: Expression = x.iter().copied().map(Expression::from).sum();
    problem = problem.with(constraint!(budget <= options.antennas as f64));
    for (z, coverers) in &atom_rows {
        let taken: Expression = coverers.iter().map(|&c| Expression::from(x[c])).sum();
        problem = problem.with(constraint!(Expression::from(*z) <= taken));
    }
    for (covered, residual, y) in rows {
        problem = problem.with(constraint!(covered >= residual * (1.0 - 1e-9) * y));
    }

    let Ok(solved) = problem.solve() else {
        return WindowOutcome {
            serviced: 0,
            taken: Vec::new(),
            proved: false,
            decisive,
        };
    };
    let taken: Vec<usize> = (0..window.len())
        .filter(|&slot| solved.value(x[slot]) > 0.5)
        .collect();
    WindowOutcome {
        serviced: solved.eval(&objective).round() as usize,
        taken,
        proved: matches!(solved.status(), SolutionStatus::Optimal),
        decisive,
    }
}

fn atoms_over(effective: &[Vec<(u32, IntervalSet)>], building: u32) -> Vec<Atom> {
    let contributors: Vec<(u32, &IntervalSet)> = effective
        .iter()
        .enumerate()
        .filter_map(|(slot, view)| {
            view.iter()
                .find(|(b, _)| *b == building)
                .map(|(_, set)| (slot as u32, set))
        })
        .collect();
    let mut cuts: Vec<f64> = contributors
        .iter()
        .flat_map(|(_, set)| set.spans().iter().flat_map(|&(lo, hi)| [lo, hi]))
        .collect();
    cuts.sort_by(f64::total_cmp);
    cuts.dedup();
    if cuts.len() < 2 {
        return Vec::new();
    }
    let mut covered_by: Vec<Vec<u32>> = vec![Vec::new(); cuts.len() - 1];
    for (slot, set) in &contributors {
        for &(lo, hi) in set.spans() {
            let start = cuts.partition_point(|&cut| cut < lo);
            let end = cuts.partition_point(|&cut| cut < hi);
            for entry in covered_by.iter_mut().take(end).skip(start) {
                entry.push(*slot);
            }
        }
    }
    let mut atoms: Vec<Atom> = Vec::new();
    for (index, covered_by) in covered_by.into_iter().enumerate() {
        let length = cuts[index + 1] - cuts[index];
        if covered_by.is_empty() || length <= 0.0 {
            continue;
        }
        match atoms.last_mut() {
            Some(last) if last.covered_by == covered_by => last.length += length,
            _ => atoms.push(Atom { length, covered_by }),
        }
    }
    atoms
}

fn bbox_of(points: &[Point]) -> (Point, Point) {
    let mut lo = Point::new(f64::INFINITY, f64::INFINITY);
    let mut hi = Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in points {
        lo = Point::new(lo.x.min(p.x), lo.y.min(p.y));
        hi = Point::new(hi.x.max(p.x), hi.y.max(p.y));
    }
    if points.is_empty() {
        (Point::new(0.0, 0.0), Point::new(0.0, 0.0))
    } else {
        (lo, hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use framework::test_utils::square;

    fn instance(k: usize, tau: f64) -> Instance {
        Instance {
            buildings: vec![
                square(0, 0.0, 0.0),
                square(1, 40.0, 0.0),
                square(2, 0.0, 40.0),
                square(3, 40.0, 40.0),
            ],
            k,
            tau,
        }
    }

    fn options(antennas: usize) -> WindowOptions {
        WindowOptions {
            antennas,
            windows: 4,
            time_limit: 10.0,
            reach: 40.0,
            max_radius: None,
            coverage: CoverageConfig {
                candidate_spacing: 5.0,
                ..CoverageConfig::default()
            },
            centres: WindowCentres::default(),
            shard: None,
            seed: 7,
            verbose: false,
        }
    }

    #[test]
    fn a_window_can_always_reproduce_the_incumbent() {
        let instance = instance(2, 0.5);
        let solution = Solution {
            antennas: vec![
                Point {
                    x: 19.731_3,
                    y: -4.284_9,
                },
                Point {
                    x: 20.117_7,
                    y: 44.913_1,
                },
                Point {
                    x: -4.556_2,
                    y: 20.884_4,
                },
            ],
        };
        for centres in [WindowCentres::Antenna, WindowCentres::OpenBuilding] {
            let options = WindowOptions {
                windows: 3,
                centres,
                ..options(2)
            };
            let report = certify_windows(&instance, &solution, &options);
            for result in &report.results {
                assert!(
                    result.best >= result.incumbent,
                    "{centres:?}: oracle scored {} against an incumbent of {} — \
                     the window could not reproduce the placement it was given",
                    result.best,
                    result.incumbent,
                );
            }
        }
    }

    #[test]
    fn sharding_partitions_the_sweep_without_changing_it() {
        let instance = instance(2, 0.5);
        let solution = Solution {
            antennas: vec![
                Point { x: 20.0, y: -5.0 },
                Point { x: 20.0, y: 45.0 },
                Point { x: -5.0, y: 20.0 },
            ],
        };
        for centres in [WindowCentres::Antenna, WindowCentres::OpenBuilding] {
            let base = WindowOptions {
                windows: 3,
                centres,
                ..options(1)
            };
            let whole = certify_windows(&instance, &solution, &base);

            let count = 3;
            let mut sharded: Vec<WindowResult> = Vec::new();
            for index in 0..count {
                let piece = WindowOptions {
                    shard: Some(Shard::new(index, count)),
                    ..clone_options(&base)
                };
                sharded.extend(certify_windows(&instance, &solution, &piece).results);
            }

            assert_eq!(
                whole.results.len(),
                sharded.len(),
                "{centres:?}: shards did not cover the sweep exactly once"
            );
            let mut expected: Vec<_> = whole
                .results
                .iter()
                .map(|r| (r.freed.clone(), r.incumbent, r.best))
                .collect();
            let mut actual: Vec<_> = sharded
                .iter()
                .map(|r| (r.freed.clone(), r.incumbent, r.best))
                .collect();
            expected.sort();
            actual.sort();
            assert_eq!(expected, actual, "{centres:?}: sharded verdicts differ");
        }
    }

    #[test]
    fn a_sharded_sweep_merges_to_the_unsharded_result() {
        let instance = instance(2, 0.5);
        let wasted = Solution {
            antennas: vec![Point { x: 20.0, y: -5.0 }, Point { x: 20.0, y: -5.0 }],
        };
        let base = WindowOptions {
            windows: 4,
            centres: WindowCentres::OpenBuilding,
            ..options(1)
        };

        let whole = certify_windows(&instance, &wasted, &base);
        let (direct, direct_applied) = apply_window_results(&instance, &wasted, &whole.results);

        let count = 2;
        let mut gathered = Vec::new();
        for index in 0..count {
            let piece = WindowOptions {
                shard: Some(Shard::new(index, count)),
                ..clone_options(&base)
            };
            gathered.extend(certify_windows(&instance, &wasted, &piece).results);
        }
        let (merged, merged_applied) = apply_window_results(&instance, &wasted, &gathered);

        assert_eq!(direct_applied, merged_applied);
        assert_eq!(
            evaluate(&instance, &direct).score,
            evaluate(&instance, &merged).score,
        );
    }

    #[test]
    fn shard_strides_are_disjoint_and_complete() {
        let count = 7;
        for total in [0usize, 1, 6, 7, 13, 100] {
            let mut covered: Vec<usize> = Vec::new();
            for index in 0..count {
                let shard = Shard::new(index, count);
                covered.extend((0..total).filter(|p| shard.keeps(*p)));
            }
            covered.sort_unstable();
            assert_eq!(covered, (0..total).collect::<Vec<_>>());
        }
    }

    #[test]
    fn an_optimal_placement_is_never_reported_improvable() {
        let instance = instance(2, 0.5);
        let exact = crate::solvers::ExactCoverSolver::new()
            .with_coverage_config(options(2).coverage)
            .solve_with_bound(&instance);
        let report = certify_windows(&instance, &exact.solution, &options(2));
        assert!(report.results.iter().all(|r| !r.improved()));
        assert_eq!(report.improved(), 0);
    }

    #[test]
    fn a_wasted_antenna_is_found() {
        let instance = instance(2, 0.5);
        let good = crate::solvers::ExactCoverSolver::new()
            .with_coverage_config(options(2).coverage)
            .solve_with_bound(&instance);
        let wasted = Solution {
            antennas: vec![good.solution.antennas[0], good.solution.antennas[0]],
        };
        let wasted_score = evaluate(&instance, &wasted).score;
        assert!(
            wasted_score < good.evaluated,
            "fixture is not actually worse"
        );
        let report = certify_windows(&instance, &wasted, &options(2));
        assert!(
            report.improved() > 0,
            "no window found the wasted antenna ({wasted_score} vs {})",
            good.evaluated
        );
        let repaired = report
            .results
            .iter()
            .find_map(|result| result.apply(&wasted))
            .expect("an improving window has a replacement");
        assert_eq!(repaired.antennas.len(), instance.k);
    }

    #[test]
    fn open_building_centres_anchor_on_neglected_buildings() {
        let instance = instance(2, 0.5);
        let options = WindowOptions {
            centres: WindowCentres::OpenBuilding,
            ..options(2)
        };
        let good = crate::solvers::ExactCoverSolver::new()
            .with_coverage_config(options.coverage.clone())
            .solve_with_bound(&instance);
        let wasted = Solution {
            antennas: vec![good.solution.antennas[0], good.solution.antennas[0]],
        };
        let report = certify_windows(&instance, &wasted, &options);
        assert!(
            report.improved() > 0,
            "no building-centred window found the wasted antenna"
        );
    }

    #[test]
    fn antenna_centres_are_unchanged_by_the_focus() {
        let instance = instance(2, 0.5);
        let solution = crate::solvers::ExactCoverSolver::new()
            .with_coverage_config(options(2).coverage)
            .solve_with_bound(&instance)
            .solution;
        let specs = antenna_centres(&solution, &options(2));
        for spec in &specs {
            let freed: Vec<Point> = spec.freed.iter().map(|&a| solution.antennas[a]).collect();
            let (lo, hi) = bbox_of(&freed);
            let mut widened = freed;
            widened.push(spec.focus);
            assert_eq!((lo, hi), bbox_of(&widened));
        }
    }

    #[test]
    fn a_radius_cap_never_excludes_the_focus() {
        let options = WindowOptions {
            max_radius: Some(60.0),
            ..options(2)
        };
        let focus = Point { x: 0.0, y: 0.0 };
        let freed = [focus, Point { x: 500.0, y: 0.0 }];
        let (centre, radius) = window_disc(&freed, focus, &options);
        assert!((focus - centre).norm() <= radius);
    }

    #[test]
    fn a_window_disc_contains_the_antennas_it_frees() {
        let options = WindowOptions {
            max_radius: Some(60.0),
            ..options(2)
        };
        for freed in [
            vec![Point { x: 0.0, y: 0.0 }],
            vec![Point { x: 0.0, y: 0.0 }, Point { x: 500.0, y: 0.0 }],
            vec![
                Point { x: -300.0, y: 200.0 },
                Point { x: 0.0, y: 0.0 },
                Point { x: 410.0, y: -180.0 },
            ],
        ] {
            let focus = freed[0];
            let (centre, radius) = window_disc(&freed, focus, &options);
            for antenna in &freed {
                assert!(
                    (*antenna - centre).norm() <= radius,
                    "freed antenna {antenna:?} lies outside its own window \
                     (centre {centre:?}, radius {radius})",
                );
            }
            assert!(radius >= 60.0, "no search margin: radius {radius}");
        }
    }
}
