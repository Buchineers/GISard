use framework::{Evaluator, Point, parse_antennas, parse_instance};
use std::collections::BTreeSet;
use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let [_, instance_path, tau, a_path, b_path] = args.as_slice() else {
        panic!("usage: compare_solutions <instance.geojson> <tau> <a.txt> <b.txt>");
    };
    let tau: f64 = tau.parse().expect("invalid tau");
    let instance = parse_instance(instance_path, 1, tau);
    let evaluator = Evaluator::new(&instance);

    let a = parse_antennas(a_path);
    let b = parse_antennas(b_path);

    let serviced = |antennas: &[Point]| -> BTreeSet<u32> {
        let evaluation = evaluator.evaluate(antennas);
        evaluation.serviced_ids(&instance).into_iter().collect()
    };
    let sa = serviced(&a);
    let sb = serviced(&b);

    let both: Vec<_> = sa.intersection(&sb).copied().collect();
    let only_a: Vec<_> = sa.difference(&sb).copied().collect();
    let only_b: Vec<_> = sb.difference(&sa).copied().collect();
    let union = sa.union(&sb).count();

    println!("antennas       A {:>6}   B {:>6}", a.len(), b.len());
    println!("serviced       A {:>6}   B {:>6}", sa.len(), sb.len());
    println!("  both           {both:>6}", both = both.len());
    println!("  only A         {:>6}", only_a.len());
    println!("  only B         {:>6}   <- what B knows that A does not", only_b.len());
    println!("  union          {union:>6}");
    let jaccard = both.len() as f64 / union.max(1) as f64;
    println!("  jaccard        {jaccard:>6.3}");

    let mut nearest: Vec<f64> = b
        .iter()
        .map(|&p| {
            a.iter()
                .map(|&q| (p - q).norm())
                .fold(f64::INFINITY, f64::min)
        })
        .collect();
    nearest.sort_by(f64::total_cmp);
    if !nearest.is_empty() {
        let median = nearest[nearest.len() / 2];
        let coincident = nearest.iter().filter(|d| **d < 1.0).count();
        println!(
            "B->A distance  median {median:.1}   max {:.1}   within 1 unit {coincident}/{}",
            nearest.last().copied().unwrap_or(0.0),
            b.len(),
        );
    }
}
