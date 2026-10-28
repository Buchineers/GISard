use crate::types::{Building, Instance, Point, Solution};

pub fn p(x: f64, y: f64) -> Point {
    Point::new(x, y)
}

pub fn serviced_ids(instance: &Instance, solution: &Solution) -> Vec<u32> {
    let mut ids = crate::evaluation::evaluate(instance, solution).serviced_ids(instance);
    ids.sort_unstable();
    ids
}

pub fn square_vertices() -> Vec<Point> {
    vec![p(0.0, 0.0), p(10.0, 0.0), p(10.0, 10.0), p(0.0, 10.0)]
}

pub fn square(id: u32, x: f64, y: f64) -> Building {
    Building {
        id,
        vertices: vec![
            p(x, y),
            p(x + 10.0, y),
            p(x + 10.0, y + 10.0),
            p(x, y + 10.0),
        ],
    }
}

pub fn instance(buildings: Vec<Building>, tau: f64) -> Instance {
    Instance {
        buildings,
        k: 1,
        tau,
    }
}
