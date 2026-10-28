use crate::types::{Building, Instance, Point};
use geojson::{FeatureCollection, GeometryValue};
use std::fs;
use std::io::{self, Read};

pub fn parse_instance(path: &str, k: usize, tau: f64) -> Instance {
    let data = fs::read_to_string(path).expect("failed to read GeoJSON file");
    parse_geojson(&data, k, tau)
}

pub fn parse_instance_stdin(k: usize, tau: f64) -> Instance {
    let mut data = String::new();
    io::stdin()
        .read_to_string(&mut data)
        .expect("failed to read from stdin");
    parse_geojson(&data, k, tau)
}

fn parse_geojson(data: &str, k: usize, tau: f64) -> Instance {
    assert!(k > 0, "k must be positive");
    assert!(tau > 0.0 && tau <= 1.0, "tau must be in (0, 1]");
    let fc: FeatureCollection = data.parse().expect("failed to parse GeoJSON");

    let buildings = fc
        .features
        .into_iter()
        .map(|feature| {
            let id = feature
                .property("id")
                .and_then(|v| v.as_u64())
                .expect("missing or invalid 'id' property") as u32;

            let geometry = feature.geometry.expect("missing geometry");
            let coords = match geometry.value {
                GeometryValue::Polygon { coordinates } => {
                    coordinates.into_iter().next().expect("empty polygon")
                }
                _ => panic!("expected Polygon geometry"),
            };
            assert!(!coords.is_empty(), "empty polygon ring");

            let vertices: Vec<Point> = coords[..coords.len() - 1]
                .iter()
                .map(|c| Point::new(c[0], c[1]))
                .collect();

            Building { id, vertices }
        })
        .collect();

    Instance { buildings, k, tau }
}

pub fn parse_antennas(path: &str) -> Vec<Point> {
    let data = fs::read_to_string(path).expect("failed to read antennas file");
    data.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts = line.split(|c: char| c == ',' || c.is_whitespace());
            let mut next = || -> f64 {
                parts
                    .by_ref()
                    .find(|p| !p.is_empty())
                    .and_then(|p| p.parse().ok())
                    .unwrap_or_else(|| panic!("invalid antenna line: {line}"))
            };
            Point::new(next(), next())
        })
        .collect()
}
