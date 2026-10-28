use std::{env, time::Instant};

use framework::{CoverageGoals, IntervalSet, parse_antennas, parse_instance};
use gisard::{
    coverage::{CoverageConfig, CoverageData, build_coverage},
    solvers::select_antennas,
};
use good_lp::{
    Expression, Solution as LpSolution, SolverModel, constraint, default_solver, variable,
    variables,
};

fn main() {
    let args: Vec<String> = env::args().collect();
    let [_, instance_path, k, tau, rest @ ..] = args.as_slice() else {
        panic!(
            "usage: bounds <instance.geojson> <k> <tau> [candidate_spacing] [event_radius] [solution.txt|-] [no-lp|lp|lp-top=N]"
        );
    };
    let k: usize = k.parse().expect("invalid k");
    let tau: f64 = tau.parse().expect("invalid tau");
    let mut config = CoverageConfig::default();
    if let Some(v) = rest.first() {
        config.candidate_spacing = v.parse().expect("invalid candidate spacing");
    }
    if let Some(v) = rest.get(1) {
        config.event_candidate_radius = v.parse().expect("invalid event radius");
    }
    let solution_path = rest.get(2).filter(|v| v.as_str() != "-");
    let lp_mode = LpMode::parse(rest.get(3).map(String::as_str).unwrap_or("no-lp"));

    let instance = parse_instance(instance_path, k, tau);
    eprintln!(
        "{} buildings, k={k}, tau={tau}, config {config:?}",
        instance.buildings.len()
    );

    let start = Instant::now();
    let data = build_coverage(&instance, &config);
    eprintln!(
        "coverage built in {:.2?}: {} candidates",
        start.elapsed(),
        data.candidates.len()
    );

    let goals = CoverageGoals::new(&data.perimeters, tau);
    let needs = goals.needs.clone();

    let start = Instant::now();
    let (greedy_antennas, greedy_internal) = select_antennas(&data, k, tau);
    let greedy_exact = framework::evaluate(
        &instance,
        &framework::Solution {
            antennas: greedy_antennas,
        },
    )
    .score;
    eprintln!("greedy lower bound computed in {:.2?}", start.elapsed());

    let solution_exact = solution_path.map(|path| {
        let antennas = parse_antennas(path);
        framework::evaluate(&instance, &framework::Solution { antennas }).score
    });

    let serviceability = serviceability_upper_bound(&data, &needs);
    let capacity = capacity_upper_bound(&data, &needs, k, &serviceability.serviceable);
    let single = best_single_stats(&data, &needs);
    let lp = match lp_mode {
        LpMode::None => None,
        LpMode::Full => {
            let start = Instant::now();
            let bound = lp_relaxation_upper_bound(&data, &needs, k, None);
            eprintln!("full LP relaxation solved in {:.2?}", start.elapsed());
            Some(("full finite candidate model", bound))
        }
        LpMode::Top(limit) => {
            let candidates = top_capped_utility_candidates(&data, &needs, limit);
            let start = Instant::now();
            let bound = lp_relaxation_upper_bound(&data, &needs, k, Some(&candidates));
            eprintln!(
                "restricted top-{limit} LP relaxation solved in {:.2?}",
                start.elapsed()
            );
            Some(("restricted top-candidate model", bound))
        }
    };

    println!("finite candidate model bounds");
    println!("  buildings: {}", instance.buildings.len());
    println!("  candidates: {}", data.candidates.len());
    println!(
        "  total perimeter: {:.0}",
        data.perimeters.iter().sum::<f64>()
    );
    println!();
    println!("lower bounds");
    println!("  greedy exact: {greedy_exact}  (internal {greedy_internal})");
    if let Some(score) = solution_exact {
        println!("  supplied solution exact: {score}");
    }
    println!();
    println!("upper bounds");
    println!(
        "  candidate-serviceable: {}  ({} impossible in this candidate set)",
        serviceability.serviceable_count, serviceability.impossible_count
    );
    println!(
        "  capped-capacity: {}  (top-{k} capped visible length {:.0})",
        capacity.max_buildings, capacity.top_k_capacity
    );
    if let Some((label, lp)) = lp {
        println!(
            "  LP relaxation ({label}): {:.3}  (safe integer ceiling {})",
            lp.objective,
            lp.objective.ceil() as usize
        );
        println!(
            "    candidate variables: {}, fractional selected antennas: {:.3}, positive x: {}, positive y: {}",
            lp.candidate_variables, lp.selected_mass, lp.positive_x, lp.positive_y
        );
    }
    println!();
    println!("single-candidate diagnostics");
    println!(
        "  buildings serviceable by one candidate: {}",
        single.one_shot
    );
    println!(
        "  best single fraction buckets: <25%={} 25-50%={} 50-75%={} >=75%={}",
        single.lt25, single.f25_50, single.f50_75, single.ge75
    );
}

enum LpMode {
    None,
    Full,
    Top(usize),
}

impl LpMode {
    fn parse(s: &str) -> Self {
        if s == "no-lp" {
            Self::None
        } else if s == "lp" {
            Self::Full
        } else if let Some(limit) = s.strip_prefix("lp-top=") {
            Self::Top(limit.parse().expect("invalid lp-top limit"))
        } else {
            panic!("invalid LP mode: expected no-lp, lp, or lp-top=N");
        }
    }
}

struct LpBound {
    candidate_variables: usize,
    objective: f64,
    selected_mass: f64,
    positive_x: usize,
    positive_y: usize,
}

fn lp_relaxation_upper_bound(
    data: &CoverageData,
    needs: &[f64],
    k: usize,
    candidates: Option<&[usize]>,
) -> LpBound {
    let candidate_indices: Vec<usize> = match candidates {
        Some(candidates) => candidates.to_vec(),
        None => (0..data.candidates.len()).collect(),
    };
    let mut candidate_to_var = vec![None; data.candidates.len()];
    for (var, &candidate) in candidate_indices.iter().enumerate() {
        candidate_to_var[candidate] = Some(var);
    }

    let mut vars = variables!();
    let x: Vec<_> = (0..candidate_indices.len())
        .map(|_| vars.add(variable().min(0.0).max(1.0)))
        .collect();
    let y: Vec<_> = (0..data.perimeters.len())
        .map(|_| vars.add(variable().min(0.0).max(1.0)))
        .collect();

    let objective: Expression = y.iter().copied().map(Expression::from).sum();
    let mut model = vars.maximise(objective.clone()).using(default_solver);

    let budget: Expression = x.iter().copied().map(Expression::from).sum();
    model = model.with(constraint!(budget <= k as f64));

    for (b, seeing) in data.by_building.iter().enumerate() {
        if needs[b] <= 0.0 {
            continue;
        }
        let mut covered = Expression::from(0.0);
        for &(candidate, measure) in seeing {
            if let Some(var) = candidate_to_var[candidate] {
                covered += measure.min(needs[b]) * x[var];
            }
        }
        model = model.with(constraint!(covered >= needs[b] * y[b]));
    }

    let solution = model.solve().expect("LP relaxation solve failed");
    LpBound {
        candidate_variables: candidate_indices.len(),
        objective: solution.eval(&objective),
        selected_mass: x.iter().map(|&var| solution.value(var)).sum(),
        positive_x: x.iter().filter(|&&var| solution.value(var) > 1e-7).count(),
        positive_y: y.iter().filter(|&&var| solution.value(var) > 1e-7).count(),
    }
}

fn top_capped_utility_candidates(data: &CoverageData, needs: &[f64], limit: usize) -> Vec<usize> {
    let mut scored: Vec<(f64, usize)> = data
        .visibility
        .iter()
        .enumerate()
        .map(|(candidate, view)| {
            let score = view
                .iter()
                .map(|(b, set)| set.measure().min(needs[*b as usize]))
                .sum();
            (score, candidate)
        })
        .collect();
    if limit < scored.len() {
        scored.select_nth_unstable_by(limit, |a, b| b.0.total_cmp(&a.0));
        scored.truncate(limit);
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, candidate)| candidate).collect()
}

struct Serviceability {
    serviceable: Vec<bool>,
    serviceable_count: usize,
    impossible_count: usize,
}

fn serviceability_upper_bound(data: &CoverageData, needs: &[f64]) -> Serviceability {
    let mut reachable = vec![IntervalSet::new(); data.perimeters.len()];
    for view in &data.visibility {
        for (b, set) in view {
            reachable[*b as usize].union_with(set);
        }
    }
    let serviceable: Vec<bool> = reachable
        .iter()
        .enumerate()
        .map(|(b, set)| set.measure() + 1e-9 >= needs[b])
        .collect();
    let serviceable_count = serviceable.iter().filter(|&&x| x).count();
    Serviceability {
        serviceable,
        serviceable_count,
        impossible_count: data.perimeters.len() - serviceable_count,
    }
}

struct CapacityBound {
    top_k_capacity: f64,
    max_buildings: usize,
}

fn capacity_upper_bound(
    data: &CoverageData,
    needs: &[f64],
    k: usize,
    serviceable: &[bool],
) -> CapacityBound {
    let mut candidate_caps: Vec<f64> = data
        .visibility
        .iter()
        .map(|view| {
            view.iter()
                .map(|(b, set)| set.measure().min(needs[*b as usize]))
                .sum()
        })
        .collect();
    candidate_caps.sort_by(|a, b| b.total_cmp(a));
    let top_k_capacity: f64 = candidate_caps.into_iter().take(k).sum();

    let mut serviceable_needs: Vec<f64> = needs
        .iter()
        .enumerate()
        .filter_map(|(b, &need)| serviceable[b].then_some(need))
        .collect();
    serviceable_needs.sort_by(f64::total_cmp);

    let mut used = 0.0;
    let mut max_buildings = 0;
    for need in serviceable_needs {
        if used + need > top_k_capacity + 1e-9 {
            break;
        }
        used += need;
        max_buildings += 1;
    }
    CapacityBound {
        top_k_capacity,
        max_buildings,
    }
}

struct SingleStats {
    one_shot: usize,
    lt25: usize,
    f25_50: usize,
    f50_75: usize,
    ge75: usize,
}

fn best_single_stats(data: &CoverageData, needs: &[f64]) -> SingleStats {
    let mut best = vec![0.0; data.perimeters.len()];
    for view in &data.visibility {
        for (b, set) in view {
            let b = *b as usize;
            best[b] = f64::max(best[b], set.measure() / data.perimeters[b].max(1e-9));
        }
    }
    SingleStats {
        one_shot: best
            .iter()
            .enumerate()
            .filter(|(b, frac)| **frac + 1e-9 >= needs[*b] / data.perimeters[*b].max(1e-9))
            .count(),
        lt25: best.iter().filter(|&&x| x < 0.25).count(),
        f25_50: best.iter().filter(|&&x| (0.25..0.5).contains(&x)).count(),
        f50_75: best.iter().filter(|&&x| (0.5..0.75).contains(&x)).count(),
        ge75: best.iter().filter(|&&x| x >= 0.75).count(),
    }
}
