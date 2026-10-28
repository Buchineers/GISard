use std::{env, fs};

use framework::{Point, bbox, parse_instance};

fn main() {
    let args: Vec<String> = env::args().collect();
    let [_, instance_path, count, out_path, rest @ ..] = args.as_slice() else {
        panic!("usage: subinstance <instance.geojson> <count> <out.geojson> [center_x center_y]");
    };
    let count: usize = count.parse().expect("invalid count");

    let instance = parse_instance(instance_path, 1, 0.5);
    let (min, max) = bbox(&instance.buildings);
    let center = match rest {
        [x, y] => Point::new(
            x.parse().expect("invalid center_x"),
            y.parse().expect("invalid center_y"),
        ),
        [] => (min + max) * 0.5,
        _ => panic!("expected both center_x and center_y"),
    };

    let centroid = |vertices: &[Point]| {
        let sum = vertices
            .iter()
            .fold(Point::new(0.0, 0.0), |acc, &v| acc + v);
        sum * (1.0 / vertices.len().max(1) as f64)
    };
    let mut ranked: Vec<_> = instance
        .buildings
        .iter()
        .map(|b| ((centroid(&b.vertices) - center).norm_sq(), b))
        .collect();
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    ranked.truncate(count);

    let features: Vec<String> = ranked
        .iter()
        .map(|(_, b)| {
            let ring: Vec<String> = b
                .vertices
                .iter()
                .chain(b.vertices.first())
                .map(|v| format!("[{},{}]", v.x, v.y))
                .collect();
            format!(
                "{{\"type\":\"Feature\",\"properties\":{{\"id\":{}}},\
                 \"geometry\":{{\"type\":\"Polygon\",\"coordinates\":[[{}]]}}}}",
                b.id,
                ring.join(",")
            )
        })
        .collect();
    fs::write(
        out_path,
        format!(
            "{{\"type\":\"FeatureCollection\",\"features\":[{}]}}",
            features.join(",")
        ),
    )
    .expect("failed to write sub-instance");

    eprintln!(
        "wrote {} buildings around ({:.1}, {:.1}) to {out_path}",
        ranked.len(),
        center.x,
        center.y
    );
}
