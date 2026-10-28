use framework::{Evaluator, Instance, Point, bbox};

use crate::sampling::Candidate;

pub const ANTENNA_SNAP_DIST: f64 = 30.0;

pub fn snap_to_valid_antenna(evaluator: &Evaluator<'_>, point: Point) -> Option<Point> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return None;
    }
    evaluator
        .snap(point, ANTENNA_SNAP_DIST)
        .map(|(_, on_wall, _)| on_wall)
}

pub fn project_to_valid_antenna(
    evaluator: &Evaluator<'_>,
    point: Point,
    max_dist: f64,
) -> Option<Point> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return None;
    }
    snap_to_valid_antenna(evaluator, point).or_else(|| {
        evaluator
            .snap(point, max_dist.max(ANTENNA_SNAP_DIST))
            .map(|(_, on_wall, _)| on_wall)
    })
}

pub fn snap_candidate_to_valid(
    evaluator: &Evaluator<'_>,
    candidate: Candidate,
) -> Option<Candidate> {
    if !candidate.position.x.is_finite() || !candidate.position.y.is_finite() {
        return None;
    }
    evaluator
        .snap(candidate.position, ANTENNA_SNAP_DIST)
        .map(|(building, on_wall, _)| Candidate {
            position: on_wall,
            building: building as u32,
            kind: candidate.kind,
        })
}

pub fn project_candidate_to_valid(
    evaluator: &Evaluator<'_>,
    candidate: Candidate,
    max_dist: f64,
) -> Option<Candidate> {
    if !candidate.position.x.is_finite() || !candidate.position.y.is_finite() {
        return None;
    }
    snap_candidate_to_valid(evaluator, candidate).or_else(|| {
        evaluator
            .snap(candidate.position, max_dist.max(ANTENNA_SNAP_DIST))
            .map(|(building, on_wall, _)| Candidate {
                position: on_wall,
                building: building as u32,
                kind: candidate.kind,
            })
    })
}

pub fn require_valid_antennas(instance: &Instance, antennas: Vec<Point>) -> Vec<Point> {
    let evaluator = Evaluator::new(instance);
    let (min, max) = bbox(&instance.buildings);
    let search_radius = (max - min).norm().max(ANTENNA_SNAP_DIST) * 4.0;
    let expected = antennas.len();
    let valid: Vec<_> = antennas
        .into_iter()
        .filter_map(|point| project_to_valid_antenna(&evaluator, point, search_radius))
        .collect();
    assert_eq!(
        valid.len(),
        expected,
        "{} antenna(s) were not valid wall-mounted placements",
        expected - valid.len()
    );
    valid
}
