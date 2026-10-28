use framework::parse_instance;
use gisard::inspector::Inspector;
use gisard::solvers::GreedyCoverageSolver;
use std::env;
use std::sync::Arc;
use visualizer::{Canvas, EguiCanvas};

fn main() {
    let args: Vec<String> = env::args().collect();
    let [_, instance_path, rest @ ..] = args.as_slice() else {
        panic!("usage: sandbox <instance.geojson> [tau] [antennas.txt]");
    };
    let tau: f64 = rest
        .first()
        .map_or(0.5, |v| v.parse().expect("invalid tau"));
    let preload = rest.get(1).map(|path| framework::parse_antennas(path));
    let instance = parse_instance(instance_path, 1, tau);
    println!("{} buildings, tau={tau}", instance.buildings.len());

    let (canvas, events, run_ui) = EguiCanvas::create_interactive();
    std::thread::spawn(move || {
        let mut state = Inspector::new(
            &instance,
            canvas as Arc<dyn Canvas>,
            Box::new(GreedyCoverageSolver::new()),
        );
        if let Some(antennas) = preload {
            state.preload(&antennas);
        }
        state.run(events);
    });
    run_ui();
}
