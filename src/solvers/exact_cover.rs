use std::collections::HashMap;

use framework::{CoverageGoals, Instance, Point, Solution, Solver};
use good_lp::solvers::SolutionStatus;
use good_lp::solvers::highs::highs;
use good_lp::{
    Expression, Solution as LpSolution, SolverModel, Variable, constraint, variable, variables,
};
use highs::HighsModelStatus;
use rayon::prelude::*;

use crate::coverage::{CoverageConfig, CoverageData, build_coverage};
use crate::solvers::greedy_coverage::select_indices;

#[derive(Default)]
pub struct ExactCoverSolver {
    pub coverage: CoverageConfig,
    pub time_limit: Option<f64>,
    pub cuts: Cuts,
    pub warm_start: Option<Vec<Point>>,
    pub verbose: bool,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Cuts {
    pub min_antennas: bool,
    pub capped_measure: bool,
    pub submodular: bool,
    pub knapsack_cover: bool,
}

impl Cuts {
    pub fn all() -> Self {
        Self {
            min_antennas: true,
            capped_measure: true,
            submodular: true,
            knapsack_cover: true,
        }
    }

    fn needs_reference(self) -> bool {
        self.submodular || self.knapsack_cover
    }
}

impl ExactCoverSolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_coverage_config(mut self, coverage: CoverageConfig) -> Self {
        self.coverage = coverage;
        self
    }

    pub fn with_time_limit(mut self, seconds: f64) -> Self {
        self.time_limit = Some(seconds);
        self
    }

    pub fn with_cuts(mut self, cuts: Cuts) -> Self {
        self.cuts = cuts;
        self
    }

    pub fn with_warm_start(mut self, antennas: Vec<Point>) -> Self {
        self.warm_start = Some(antennas);
        self
    }

    pub fn verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }
}

pub struct Atom {
    pub length: f64,
    pub covered_by: Vec<u32>,
}

pub fn atoms_of(data: &CoverageData, building: usize) -> Vec<Atom> {
    let viewers: Vec<u32> = data.by_building[building]
        .iter()
        .map(|&(candidate, _)| candidate as u32)
        .collect();
    let spans_of = |candidate: u32| -> &[(f64, f64)] {
        data.visibility[candidate as usize]
            .iter()
            .find(|(b, _)| *b as usize == building)
            .map_or(&[], |(_, set)| set.spans())
    };

    let mut cuts: Vec<f64> = viewers
        .iter()
        .flat_map(|&candidate| spans_of(candidate).iter().flat_map(|&(lo, hi)| [lo, hi]))
        .collect();
    cuts.sort_by(f64::total_cmp);
    cuts.dedup();
    if cuts.len() < 2 {
        return Vec::new();
    }

    let mut covered_by: Vec<Vec<u32>> = vec![Vec::new(); cuts.len() - 1];
    for &candidate in &viewers {
        for &(lo, hi) in spans_of(candidate) {
            let start = cuts.partition_point(|&cut| cut < lo);
            let end = cuts.partition_point(|&cut| cut < hi);
            for slot in covered_by.iter_mut().take(end).skip(start) {
                slot.push(candidate);
            }
        }
    }

    let mut atoms: Vec<Atom> = Vec::new();
    for (i, covered_by) in covered_by.into_iter().enumerate() {
        let length = cuts[i + 1] - cuts[i];
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

pub fn min_antennas_for(data: &CoverageData, building: usize, need: f64) -> Option<usize> {
    let mut measures: Vec<f64> = data.by_building[building]
        .iter()
        .map(|&(_, measure)| measure)
        .collect();
    measures.sort_by(|a, b| b.total_cmp(a));
    let mut covered = 0.0;
    for (taken, measure) in measures.into_iter().enumerate() {
        covered += measure;
        if covered + 1e-9 >= need {
            return Some(taken + 1);
        }
    }
    None
}

fn warm_start_indices(data: &CoverageData, antennas: &[Point]) -> Vec<usize> {
    let mut exact: HashMap<(u64, u64), usize> = HashMap::new();
    for (index, candidate) in data.candidates.iter().enumerate() {
        exact
            .entry((
                candidate.position.x.to_bits(),
                candidate.position.y.to_bits(),
            ))
            .or_insert(index);
    }
    let mut indices: Vec<usize> = antennas
        .iter()
        .filter_map(|&position| {
            exact
                .get(&(position.x.to_bits(), position.y.to_bits()))
                .copied()
                .or_else(|| {
                    data.candidates
                        .iter()
                        .enumerate()
                        .min_by(|(_, a), (_, b)| {
                            (a.position - position)
                                .norm_sq()
                                .total_cmp(&(b.position - position).norm_sq())
                        })
                        .map(|(index, _)| index)
                })
        })
        .collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

struct Solved {
    antennas: Vec<Point>,
    objective: f64,
    dual_bound: f64,
    proved: bool,
}

struct Marginals {
    covered_by_reference: f64,
    gain_outside: Vec<(usize, f64)>,
    exclusive_inside: Vec<(usize, f64)>,
    residual: f64,
}

fn marginals(atoms: &[Atom], need: f64, in_reference: &[bool]) -> Marginals {
    let mut covered_by_reference = 0.0;
    let mut gain: HashMap<usize, f64> = HashMap::new();
    let mut exclusive: HashMap<usize, f64> = HashMap::new();
    for atom in atoms {
        if atom.covered_by.iter().any(|&c| in_reference[c as usize]) {
            covered_by_reference += atom.length;
        } else {
            for &c in &atom.covered_by {
                *gain.entry(c as usize).or_default() += atom.length;
            }
        }
        if let [only] = atom.covered_by[..] {
            *exclusive.entry(only as usize).or_default() += atom.length;
        }
    }
    let mut gain_outside: Vec<(usize, f64)> = gain.into_iter().collect();
    gain_outside.sort_unstable_by_key(|&(c, _)| c);
    let mut exclusive_inside: Vec<(usize, f64)> = exclusive
        .into_iter()
        .filter(|&(c, _)| in_reference[c])
        .collect();
    exclusive_inside.sort_unstable_by_key(|&(c, _)| c);
    Marginals {
        covered_by_reference,
        gain_outside,
        exclusive_inside,
        residual: need - covered_by_reference,
    }
}

pub struct AtomModel {
    pub atoms: Vec<Vec<Atom>>,
    pub reachable: Vec<bool>,
}

impl AtomModel {
    pub fn build(data: &CoverageData, goals: &CoverageGoals) -> Self {
        let atoms: Vec<Vec<Atom>> = (0..goals.needs.len())
            .into_par_iter()
            .map(|building| {
                if goals.needs[building] > 0.0 {
                    atoms_of(data, building)
                } else {
                    Vec::new()
                }
            })
            .collect();
        let reachable = atoms
            .iter()
            .enumerate()
            .map(|(building, atoms)| {
                goals.needs[building] > 0.0
                    && atoms.iter().map(|a| a.length).sum::<f64>() + 1e-9 >= goals.needs[building]
            })
            .collect();
        Self { atoms, reachable }
    }

    pub fn atom_count(&self) -> usize {
        self.atoms
            .iter()
            .zip(&self.reachable)
            .filter(|&(_, &r)| r)
            .map(|(a, _)| a.len())
            .sum()
    }

    pub fn reachable_count(&self) -> usize {
        self.reachable.iter().filter(|&&r| r).count()
    }
}

pub struct ExactOutcome {
    pub solution: Solution,
    pub evaluated: usize,
    pub value: usize,
    pub bound: f64,
    pub atom_count: usize,
    pub reachable: usize,
}

impl ExactOutcome {
    pub fn is_proven_optimal(&self) -> bool {
        self.bound < self.evaluated as f64 + 1.0 - 1e-6
    }
}

impl ExactCoverSolver {
    pub fn solve_with_bound(&self, instance: &Instance) -> ExactOutcome {
        let data = build_coverage(instance, &self.coverage);
        let goals = CoverageGoals::new(&data.perimeters, instance.tau);
        let model = AtomModel::build(&data, &goals);
        if self.verbose {
            eprintln!(
                "exact: {} buildings ({} reachable), {} candidates, {} atoms",
                instance.buildings.len(),
                model.reachable_count(),
                data.candidates.len(),
                model.atom_count(),
            );
        }

        let integer = self.run(instance, &data, &goals, &model, true);
        let value = integer.objective;
        let solution = Solution {
            antennas: integer.antennas,
        };
        let evaluated = framework::evaluate(instance, &solution).score;
        let bound = if integer.proved {
            value
        } else {
            let relaxed = self.run(instance, &data, &goals, &model, false).objective;
            integer.dual_bound.min(relaxed)
        };
        ExactOutcome {
            solution,
            evaluated,
            value: value.round() as usize,
            bound,
            atom_count: model.atom_count(),
            reachable: model.reachable_count(),
        }
    }

    fn run(
        &self,
        instance: &Instance,
        data: &CoverageData,
        goals: &CoverageGoals,
        model: &AtomModel,
        integral: bool,
    ) -> Solved {
        let mut vars = variables!();
        let unit = || {
            if integral {
                variable().binary()
            } else {
                variable().min(0.0).max(1.0)
            }
        };
        let x: Vec<Variable> = (0..data.candidates.len())
            .map(|_| vars.add(unit()))
            .collect();
        let y: Vec<Variable> = (0..instance.buildings.len())
            .map(|_| vars.add(unit()))
            .collect();
        let z: Vec<Vec<Variable>> = model
            .atoms
            .iter()
            .zip(&model.reachable)
            .map(|(atoms, &reachable)| {
                if reachable {
                    atoms
                        .iter()
                        .map(|_| vars.add(variable().min(0.0).max(1.0)))
                        .collect()
                } else {
                    Vec::new()
                }
            })
            .collect();

        let y_base = x.len();
        let mut next = y_base + y.len();
        let z_columns: Vec<Vec<usize>> = z
            .iter()
            .map(|vars| {
                let columns = (next..next + vars.len()).collect();
                next += vars.len();
                columns
            })
            .collect();
        let variable_count = next;
        let objective: Expression = y
            .iter()
            .zip(&model.reachable)
            .filter(|&(_, &r)| r)
            .map(|(&var, _)| Expression::from(var))
            .sum();
        let mut problem = vars.maximise(objective.clone()).using(highs);
        if let Some(limit) = self.time_limit
            && integral
        {
            problem = problem.set_time_limit(limit);
        }
        problem = problem
            .set_option("primal_feasibility_tolerance", 1e-10)
            .set_option("mip_feasibility_tolerance", 1e-10);
        if !integral {
            problem = problem
                .set_option("solver", "ipm")
                .set_option("output_flag", self.verbose);
        }
        problem.set_verbose(self.verbose);

        let mut in_reference = vec![false; data.candidates.len()];
        if self.cuts.needs_reference() {
            for candidate in select_indices(data, goals, instance.k).0 {
                in_reference[candidate] = true;
            }
        }

        let budget: Expression = x.iter().copied().map(Expression::from).sum();
        let mut problem = problem.with(constraint!(budget <= instance.k as f64));
        for (building, atoms) in model.atoms.iter().enumerate() {
            if !model.reachable[building] {
                continue;
            }
            let mut covered = Expression::from(0.0);
            for (atom, &z) in atoms.iter().zip(&z[building]) {
                let taken: Expression = atom
                    .covered_by
                    .iter()
                    .map(|&candidate| Expression::from(x[candidate as usize]))
                    .sum();
                problem = problem.with(constraint!(z <= taken));
                covered += atom.length * z;
            }
            let need = goals.needs[building];
            let admitted = need * (1.0 - 1e-9);
            problem = problem.with(constraint!(covered.clone() >= admitted * y[building]));
            if self.cuts.min_antennas
                && let Some(least) = min_antennas_for(data, building, need)
            {
                let viewers: Expression = data.by_building[building]
                    .iter()
                    .map(|&(candidate, _)| Expression::from(x[candidate]))
                    .sum();
                problem = problem.with(constraint!(viewers >= least as f64 * y[building]));
            }
            if self.cuts.capped_measure {
                let capped: Expression = data.by_building[building]
                    .iter()
                    .map(|&(candidate, measure)| measure.min(need) * x[candidate])
                    .sum();
                problem = problem.with(constraint!(capped >= need * y[building]));
            }
            if self.cuts.needs_reference() {
                let marginals = marginals(atoms, need, &in_reference);
                if self.cuts.submodular {
                    let mut nw = Expression::from(marginals.covered_by_reference);
                    for &(candidate, gain) in &marginals.gain_outside {
                        nw += gain * x[candidate];
                    }
                    for &(candidate, exclusive) in &marginals.exclusive_inside {
                        nw += exclusive * x[candidate] - exclusive;
                    }
                    problem = problem.with(constraint!(covered.clone() <= nw));
                }
                if self.cuts.knapsack_cover && marginals.residual > 1e-9 {
                    let residual = marginals.residual;
                    let cover: Expression = atoms
                        .iter()
                        .zip(&z[building])
                        .filter(|(atom, _)| {
                            !atom.covered_by.iter().any(|&c| in_reference[c as usize])
                        })
                        .map(|(atom, &z)| atom.length.min(residual) * z)
                        .sum();
                    problem = problem.with(constraint!(cover >= residual * y[building]));
                }
            }
        }

        let warm_start = self
            .warm_start
            .as_ref()
            .filter(|_| integral)
            .map(|antennas| {
                let taken = warm_start_indices(data, antennas);
                let mut chosen = vec![false; data.candidates.len()];
                for &candidate in &taken {
                    chosen[candidate] = true;
                }
                let mut columns = vec![0.0; variable_count];
                for (candidate, slot) in columns.iter_mut().take(x.len()).enumerate() {
                    *slot = f64::from(u8::from(chosen[candidate]));
                }
                for (building, atoms) in model.atoms.iter().enumerate() {
                    if !model.reachable[building] {
                        continue;
                    }
                    let mut covered = 0.0;
                    for (atom, &column) in atoms.iter().zip(&z_columns[building]) {
                        let lit = atom.covered_by.iter().any(|&c| chosen[c as usize]);
                        if lit {
                            covered += atom.length;
                        }
                        columns[column] = f64::from(u8::from(lit));
                    }
                    let serviced = covered >= goals.needs[building];
                    columns[y_base + building] = f64::from(u8::from(serviced));
                }
                if self.verbose {
                    eprintln!(
                        "exact: warm start from {} antennas ({} matched candidates)",
                        antennas.len(),
                        taken.len(),
                    );
                }
                columns
            });

        if !integral {
            let solved = problem.solve().expect("atom relaxation solve failed");
            let raw = solved.eval(&objective);
            return Solved {
                antennas: Vec::new(),
                objective: raw,
                dual_bound: raw,
                proved: matches!(solved.status(), SolutionStatus::Optimal),
            };
        }

        let mut highs_model = problem.into_inner();
        highs_model.set_option("output_flag", self.verbose);
        if self.verbose {
            highs_model.set_option("log_to_console", true);
        }
        highs_model.set_option("primal_feasibility_tolerance", 1e-10);
        highs_model.set_option("mip_feasibility_tolerance", 1e-10);
        if let Some(limit) = self.time_limit {
            highs_model.set_option("time_limit", limit);
        }
        if let Some(start) = &warm_start {
            highs_model.set_solution(Some(start), None, None, None);
        }
        let solved = highs_model.try_solve().expect("atom model solve failed");
        let values = solved.get_solution();
        let columns = values.columns();
        let raw: f64 = model
            .reachable
            .iter()
            .enumerate()
            .filter(|&(_, &r)| r)
            .map(|(building, _)| columns[y_base + building])
            .sum();
        let proved = matches!(solved.status(), HighsModelStatus::Optimal);
        let gap = solved.mip_gap();
        let dual_bound = if proved {
            raw
        } else if gap.is_finite() && raw.is_finite() {
            raw + gap * raw.abs()
        } else {
            f64::INFINITY
        };
        let antennas: Vec<Point> = (0..x.len())
            .filter(|&candidate| columns[candidate] > 0.5)
            .map(|candidate| data.candidates[candidate].position)
            .collect();
        Solved {
            antennas,
            objective: raw,
            dual_bound,
            proved,
        }
    }
}

impl Solver for ExactCoverSolver {
    fn solve(&self, instance: Instance) -> Solution {
        self.solve_with_bound(&instance).solution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solvers::GreedyCoverageSolver;
    use framework::test_utils::square;

    fn vertices_only() -> CoverageConfig {
        CoverageConfig {
            candidate_spacing: 100.0,
            event_candidate_radius: 0.0,
            event_candidate_limit: 0,
        }
    }

    #[test]
    fn atoms_reproduce_candidate_measures() {
        let instance = Instance {
            buildings: vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)],
            k: 2,
            tau: 0.5,
        };
        let data = build_coverage(&instance, &vertices_only());
        for building in 0..instance.buildings.len() {
            let atoms = atoms_of(&data, building);
            for &(candidate, measure) in &data.by_building[building] {
                let from_atoms: f64 = atoms
                    .iter()
                    .filter(|atom| atom.covered_by.contains(&(candidate as u32)))
                    .map(|atom| atom.length)
                    .sum();
                assert!(
                    (from_atoms - measure).abs() < 1e-9,
                    "building {building}, candidate {candidate}: {from_atoms} vs {measure}"
                );
            }
        }
    }

    #[test]
    fn solves_single_building_exactly() {
        let instance = Instance {
            buildings: vec![square(0, 0.0, 0.0)],
            k: 1,
            tau: 0.5,
        };
        let outcome = ExactCoverSolver::new()
            .with_coverage_config(vertices_only())
            .solve_with_bound(&instance);
        assert_eq!(outcome.value, 1);
        assert_eq!(framework::evaluate(&instance, &outcome.solution).score, 1);
    }

    #[test]
    fn model_value_matches_exact_evaluation() {
        let scenes = [
            vec![square(0, 0.0, 0.0)],
            vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)],
            vec![square(0, 0.0, 0.0), square(1, 500.0, 0.0)],
            vec![
                square(0, 0.0, 0.0),
                square(1, 20.0, 0.0),
                square(2, 0.0, 20.0),
                square(3, 20.0, 20.0),
            ],
        ];
        for (scene, buildings) in scenes.into_iter().enumerate() {
            for k in 1..=3 {
                for tau in [0.5, 0.75, 1.0] {
                    let instance = Instance {
                        buildings: buildings.clone(),
                        k,
                        tau,
                    };
                    let outcome = ExactCoverSolver::new()
                        .with_coverage_config(vertices_only())
                        .solve_with_bound(&instance);
                    assert!(
                        outcome.solution.antennas.len() <= k,
                        "scene {scene}, k={k}, tau={tau}: over budget"
                    );
                    let context = format!("scene {scene}, k={k}, tau={tau}");
                    assert!(
                        outcome.evaluated as f64 <= outcome.bound + 1e-6,
                        "{context}: achieved {} above the bound {}",
                        outcome.evaluated,
                        outcome.bound
                    );
                    assert!(
                        outcome.evaluated.abs_diff(outcome.value) <= 1,
                        "{context}: model {} vs evaluated {}",
                        outcome.value,
                        outcome.evaluated
                    );
                }
            }
        }
    }

    #[test]
    fn cuts_preserve_the_optimum() {
        let buildings = vec![
            square(0, 0.0, 0.0),
            square(1, 20.0, 0.0),
            square(2, 0.0, 20.0),
            square(3, 45.0, 12.0),
        ];
        for k in 1..=4 {
            for tau in [0.45, 0.55, 0.7] {
                let instance = Instance {
                    buildings: buildings.clone(),
                    k,
                    tau,
                };
                let solve = |cuts: Cuts| {
                    ExactCoverSolver::new()
                        .with_coverage_config(vertices_only())
                        .with_cuts(cuts)
                        .solve_with_bound(&instance)
                };
                let plain = solve(Cuts::default());
                let cut = solve(Cuts::all());
                assert_eq!(
                    plain.value, cut.value,
                    "k={k}, tau={tau}: cuts changed the optimum"
                );
                assert!(
                    cut.bound <= plain.bound + 1e-6,
                    "k={k}, tau={tau}: cuts loosened the bound, {} vs {}",
                    cut.bound,
                    plain.bound
                );
            }
        }
    }

    #[test]
    fn min_antennas_counts_unions_not_sums() {
        let instance = Instance {
            buildings: vec![square(0, 0.0, 0.0)],
            k: 4,
            tau: 0.5,
        };
        let data = build_coverage(&instance, &vertices_only());
        let perimeter = data.perimeters[0];
        assert_eq!(min_antennas_for(&data, 0, 0.5 * perimeter), Some(1));
        assert_eq!(min_antennas_for(&data, 0, 0.75 * perimeter), Some(2));
        assert_eq!(min_antennas_for(&data, 0, 1.0 * perimeter), Some(2));
        assert_eq!(min_antennas_for(&data, 0, 10.0 * perimeter), None);
    }

    #[test]
    fn optimum_dominates_heuristics_and_grows_with_budget() {
        let buildings = vec![
            square(0, 0.0, 0.0),
            square(1, 20.0, 0.0),
            square(2, 0.0, 20.0),
            square(3, 45.0, 12.0),
        ];
        let mut previous = 0;
        for k in 1..=4 {
            let instance = Instance {
                buildings: buildings.clone(),
                k,
                tau: 0.75,
            };
            let optimum = ExactCoverSolver::new()
                .with_coverage_config(vertices_only())
                .solve_with_bound(&instance)
                .evaluated;
            let greedy = GreedyCoverageSolver::new()
                .with_coverage_config(vertices_only())
                .solve(instance.clone());
            assert!(
                optimum >= framework::evaluate(&instance, &greedy).score,
                "k={k}: greedy beat the optimum"
            );
            assert!(
                optimum >= previous,
                "k={k}: optimum shrank with more budget"
            );
            previous = optimum;
        }
    }

    #[test]
    fn combined_coverage_services_what_one_antenna_cannot() {
        let single = Instance {
            buildings: vec![square(0, 0.0, 0.0)],
            k: 1,
            tau: 0.75,
        };
        assert_eq!(
            ExactCoverSolver::new()
                .with_coverage_config(vertices_only())
                .solve_with_bound(&single)
                .value,
            0
        );

        let paired = Instance { k: 2, ..single };
        let outcome = ExactCoverSolver::new()
            .with_coverage_config(vertices_only())
            .solve_with_bound(&paired);
        assert_eq!(outcome.value, 1);
        assert_eq!(framework::evaluate(&paired, &outcome.solution).score, 1);
    }
}
