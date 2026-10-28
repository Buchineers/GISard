use std::{env, time::Instant};

use framework::{Solver, parse_instance};
use gisard::coverage::CoverageConfig;
use gisard::solvers::{Cuts, ExactCoverSolver, LnsOptions, LnsSolver};

fn main() {
    let args: Vec<String> = env::args().collect();
    let [_, instance_path, k, tau, rest @ ..] = args.as_slice() else {
        panic!(
            "usage: exact_solve <instance.geojson> <k> <tau> [candidate_spacing] [time_limit_s] [lns|no-lns] [none|all|min,capped,nw,kc]"
        );
    };
    let k: usize = k.parse().expect("invalid k");
    let tau: f64 = tau.parse().expect("invalid tau");
    let mut coverage = CoverageConfig::default();
    if let Some(v) = rest.first() {
        coverage.candidate_spacing = v.parse().expect("invalid candidate spacing");
    }
    let time_limit: Option<f64> = rest
        .get(1)
        .map(|v| v.parse().expect("invalid time limit"))
        .filter(|&limit: &f64| limit > 0.0);
    let with_lns = rest.get(2).map(String::as_str) != Some("no-lns");
    let cuts = match rest.get(3).map(String::as_str).unwrap_or("all") {
        "no-cuts" | "none" => Cuts::default(),
        "all" | "cuts" => Cuts::all(),
        list => {
            let mut cuts = Cuts::default();
            for name in list.split(',') {
                match name {
                    "min" => cuts.min_antennas = true,
                    "capped" => cuts.capped_measure = true,
                    "nw" => cuts.submodular = true,
                    "kc" => cuts.knapsack_cover = true,
                    other => panic!("unknown cut family: {other}"),
                }
            }
            cuts
        }
    };

    let instance = parse_instance(instance_path, k, tau);
    eprintln!(
        "{} buildings, k={k}, tau={tau}, spacing {}",
        instance.buildings.len(),
        coverage.candidate_spacing,
    );

    let lns = with_lns.then(|| {
        let start = Instant::now();
        let solution = LnsSolver::new()
            .with_coverage_config(coverage.clone())
            .with_options(LnsOptions {
                seed: Some(42),
                ..LnsOptions::default()
            })
            .solve(instance.clone());
        let score = framework::evaluate(&instance, &solution).score;
        (solution.antennas, score, start.elapsed())
    });

    let mut solver = ExactCoverSolver::new()
        .with_coverage_config(coverage)
        .with_cuts(cuts)
        .verbose(true);
    if let Some(limit) = time_limit {
        solver = solver.with_time_limit(limit);
    }
    if let Some((antennas, _, _)) = &lns {
        solver = solver.with_warm_start(antennas.clone());
    }
    let start = Instant::now();
    let outcome = solver.solve_with_bound(&instance);
    let exact_time = start.elapsed();

    println!();
    println!("exact model ({:.2?}, cuts {cuts:?})", exact_time,);
    println!("  reachable buildings: {}", outcome.reachable);
    println!("  atoms: {}", outcome.atom_count);
    println!("  antennas used: {}", outcome.solution.antennas.len());
    println!("  model objective: {}", outcome.value);
    println!(
        "  certified bracket: {} <= optimum <= {}",
        outcome.evaluated,
        outcome.bound.floor() as usize
    );
    println!(
        "  {}",
        if outcome.is_proven_optimal() {
            "PROVEN OPTIMAL for this candidate set".to_string()
        } else {
            format!(
                "gap open: {} building(s) still unproven",
                outcome.bound.floor() as usize - outcome.evaluated
            )
        }
    );
    if let Some((_, score, elapsed)) = lns {
        let reference = outcome.evaluated;
        let label = if outcome.is_proven_optimal() {
            "proven optimum"
        } else {
            "exact model's incumbent, not yet proven optimal"
        };
        println!();
        println!("LNS ({elapsed:.2?})");
        println!(
            "  serviced: {score}  ({} of the {label} {reference}{})",
            if reference > 0 {
                format!("{:.1}%", 100.0 * score as f64 / reference as f64)
            } else {
                "n/a".into()
            },
            if score == reference && outcome.is_proven_optimal() {
                ", optimal"
            } else {
                ""
            },
        );
    }
    for antenna in &outcome.solution.antennas {
        println!("{},{}", antenna.x, antenna.y);
    }
}
