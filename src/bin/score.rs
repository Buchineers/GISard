use framework::{Evaluator, parse_antennas, parse_instance};
use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let [_, instance_path, antennas_path, rest @ ..] = args.as_slice() else {
        panic!("usage: score <instance.geojson> <antennas.txt> [tau...]");
    };

    let instance = parse_instance(instance_path, 1, 0.5);
    let antennas = parse_antennas(antennas_path);

    let eval = Evaluator::new(&instance).evaluate(&antennas);
    let buildings = instance.buildings.len();
    println!("{} antennas, {buildings} buildings", antennas.len());
    let thresholds: Vec<f64> = if rest.is_empty() {
        vec![0.25, 0.5, 0.75]
    } else {
        rest.iter()
            .map(|tau| tau.parse().expect("invalid tau"))
            .collect()
    };
    for tau in thresholds {
        println!(
            "tau {tau:.2}: serviced {} / {buildings}",
            eval.score_at(tau)
        );
    }
}
