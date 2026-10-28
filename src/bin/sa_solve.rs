use std::{env, time::Instant};

use framework::{Solver, parse_instance};
use gisard::coverage::CoverageConfig;
#[cfg(feature = "visualizer")]
use gisard::inspector::Inspector;
use gisard::solvers::{SimulatedAnnealingConfig, SimulatedAnnealingSolver};
#[cfg(feature = "visualizer")]
use std::sync::Arc;

fn main() {
    #[cfg(feature = "visualizer")]
    let (canvas, events, run_ui) = visualizer::EguiCanvas::create_interactive();
    let args: Vec<String> = env::args().collect();
    let [_, instance_path, k, tau, rest @ ..] = args.as_slice() else {
        panic!(
            "usage: sa_solve <instance.geojson> <k> <tau> [candidate_spacing] [iterations] [proposal_samples] [seed] [local_spacing] [probe_rings] [refine_fraction] [event_radius] [event_partners] [threshold_breakpoints]"
        );
    };
    let k: usize = k.parse().expect("invalid k");
    let tau: f64 = tau.parse().expect("invalid tau");
    let mut coverage = CoverageConfig::default();
    let mut config = SimulatedAnnealingConfig::default();
    if let Some(value) = rest.first() {
        coverage.candidate_spacing = value.parse().expect("invalid candidate spacing");
    }
    if let Some(value) = rest.get(1) {
        config.iterations = value.parse().expect("invalid iterations");
    }
    if let Some(value) = rest.get(2) {
        config.proposal_samples = value.parse().expect("invalid proposal samples");
    }
    if let Some(value) = rest.get(3) {
        config.seed = Some(value.parse().expect("invalid seed"));
    }
    if let Some(value) = rest.get(4) {
        config.local_spacing = value.parse().expect("invalid local spacing");
    }
    if let Some(value) = rest.get(5) {
        config.probe_rings = value.parse().expect("invalid probe rings");
    }
    if let Some(value) = rest.get(6) {
        config.refine_fraction = value.parse().expect("invalid refine fraction");
    }
    if let Some(value) = rest.get(7) {
        coverage.event_candidate_radius = value.parse().expect("invalid event radius");
    }
    if let Some(value) = rest.get(8) {
        coverage.event_candidate_limit = value.parse().expect("invalid event partners");
    }
    if let Some(value) = rest.get(9) {
        config.threshold_breakpoints = value
            .parse()
            .expect("invalid threshold_breakpoints (true or false)");
    }

    let instance = parse_instance(instance_path, k, tau);
    let buildings = instance.buildings.len();
    eprintln!("{buildings} buildings, k={k}, tau={tau}, coverage {coverage:?}, config {config:?}");
    let solver = SimulatedAnnealingSolver::new()
        .with_coverage_config(coverage)
        .with_config(config);
    #[cfg(feature = "visualizer")]
    let solver = solver.with_canvas(canvas.clone());
    let start = Instant::now();
    let solve = move |instance: framework::Instance| {
        let solution = solver.solve(instance.clone());
        let score = framework::evaluate(&instance, &solution).score;
        eprintln!(
            "solved in {:.2?}: {} antennas, {score} / {buildings} serviced",
            start.elapsed(),
            solution.antennas.len(),
        );
        for antenna in &solution.antennas {
            println!("{},{}", antenna.x, antenna.y);
        }
        solution
    };

    #[cfg(feature = "visualizer")]
    {
        let inspect_instance = instance.clone();
        std::thread::spawn(move || {
            let mut inspector = Inspector::new(
                &inspect_instance,
                canvas as Arc<dyn visualizer::Canvas>,
                Box::new(SimulatedAnnealingSolver::new()),
            );
            let solution = solve(instance);
            inspector.preload(&solution.antennas);
            inspector.run(events);
        });
        run_ui();
    }

    #[cfg(not(feature = "visualizer"))]
    solve(instance);
}
