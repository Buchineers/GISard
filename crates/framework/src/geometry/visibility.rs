use crate::types::{Building, Point};

use super::grid::EdgeGrid;
use super::predicates::{
    PointLocation, locate_point, on_segment, ring_edges, segments_properly_cross,
};

pub fn segment_crosses_interior(p: Point, q: Point, polygon: &[Point]) -> bool {
    if polygon.is_empty() {
        return false;
    }
    for (a, b) in ring_edges(polygon) {
        if segments_properly_cross(p, q, a, b) {
            return true;
        }
    }
    let mut ts = vec![0.0, 1.0];
    for &v in polygon {
        if on_segment(p, q, v) {
            ts.push(segment_param(p, q, v));
        }
    }
    ts.sort_by(f64::total_cmp);
    ts.windows(2).any(|w| {
        w[1] > w[0] && {
            let mid = p + (q - p) * ((w[0] + w[1]) * 0.5);
            locate_point(mid, polygon) == PointLocation::Inside
        }
    })
}

fn segment_param(p: Point, q: Point, v: Point) -> f64 {
    let d = q - p;
    if d.x.abs() >= d.y.abs() {
        if d.x == 0.0 { 0.0 } else { (v.x - p.x) / d.x }
    } else {
        (v.y - p.y) / d.y
    }
}

pub fn is_visible(p: Point, q: Point, buildings: &[Building], grid: &EdgeGrid) -> bool {
    grid.buildings_near_segment(p, q)
        .into_iter()
        .all(|i| !segment_crosses_interior(p, q, &buildings[i].vertices))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{p, square_vertices};

    fn diamond() -> Vec<Point> {
        vec![p(0.0, 0.0), p(5.0, -5.0), p(10.0, 0.0), p(5.0, 5.0)]
    }

    #[test]
    fn through_interior_blocks() {
        assert!(segment_crosses_interior(
            p(-5.0, 5.0),
            p(15.0, 5.0),
            &square_vertices()
        ));
    }

    #[test]
    fn disjoint_segment_does_not_block() {
        assert!(!segment_crosses_interior(
            p(-5.0, 15.0),
            p(15.0, 15.0),
            &square_vertices()
        ));
    }

    #[test]
    fn grazing_vertex_does_not_block() {
        assert!(!segment_crosses_interior(
            p(-5.0, 5.0),
            p(5.0, 15.0),
            &square_vertices()
        ));
    }

    #[test]
    fn collinear_edge_does_not_block() {
        assert!(!segment_crosses_interior(
            p(-5.0, 0.0),
            p(15.0, 0.0),
            &square_vertices()
        ));
    }

    #[test]
    fn through_opposite_vertices_blocks() {
        assert!(segment_crosses_interior(
            p(-5.0, 0.0),
            p(15.0, 0.0),
            &diamond()
        ));
    }

    #[test]
    fn endpoint_on_boundary_going_outward() {
        assert!(!segment_crosses_interior(
            p(5.0, 0.0),
            p(5.0, -10.0),
            &square_vertices()
        ));
    }

    #[test]
    fn endpoint_on_boundary_going_inward() {
        assert!(segment_crosses_interior(
            p(5.0, 0.0),
            p(5.0, 5.0),
            &square_vertices()
        ));
    }

    #[test]
    fn segment_along_boundary_between_vertices() {
        assert!(!segment_crosses_interior(
            p(2.0, 0.0),
            p(8.0, 0.0),
            &square_vertices()
        ));
    }

    #[test]
    fn chord_between_boundary_points_blocks() {
        assert!(segment_crosses_interior(
            p(0.0, 5.0),
            p(10.0, 5.0),
            &square_vertices()
        ));
    }

    #[test]
    fn degenerate_point_segment() {
        assert!(segment_crosses_interior(
            p(5.0, 5.0),
            p(5.0, 5.0),
            &square_vertices()
        ));
        assert!(!segment_crosses_interior(
            p(15.0, 5.0),
            p(15.0, 5.0),
            &square_vertices()
        ));
    }

    fn building(id: u32, vertices: Vec<Point>) -> Building {
        Building { id, vertices }
    }

    #[test]
    fn is_visible_end_to_end() {
        let buildings = vec![
            building(0, square_vertices()),
            building(
                1,
                vec![p(20.0, 0.0), p(30.0, 0.0), p(30.0, 10.0), p(20.0, 10.0)],
            ),
        ];
        let grid = EdgeGrid::new(&buildings, 5.0);
        assert!(!is_visible(p(-5.0, 5.0), p(15.0, 5.0), &buildings, &grid));
        assert!(!is_visible(p(-5.0, 5.0), p(35.0, 5.0), &buildings, &grid));
        assert!(is_visible(p(15.0, -5.0), p(15.0, 15.0), &buildings, &grid));
        assert!(is_visible(p(-5.0, 15.0), p(35.0, 15.0), &buildings, &grid));
        assert!(is_visible(p(-5.0, 10.0), p(35.0, 10.0), &buildings, &grid));
    }

    #[test]
    fn is_visible_no_buildings() {
        let buildings: Vec<Building> = vec![];
        let grid = EdgeGrid::new(&buildings, 1.0);
        assert!(is_visible(p(0.0, 0.0), p(100.0, 100.0), &buildings, &grid));
    }
}
