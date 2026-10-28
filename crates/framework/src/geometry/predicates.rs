use crate::types::Point;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Ccw,
    Cw,
    Collinear,
}

#[inline]
pub fn orient(a: Point, b: Point, c: Point) -> Orientation {
    let det = robust::orient2d(coord(a), coord(b), coord(c));
    if det > 0.0 {
        Orientation::Ccw
    } else if det < 0.0 {
        Orientation::Cw
    } else {
        Orientation::Collinear
    }
}

#[inline]
fn coord(p: Point) -> robust::Coord<f64> {
    robust::Coord { x: p.x, y: p.y }
}

#[inline]
pub fn on_segment(a: Point, b: Point, p: Point) -> bool {
    orient(a, b, p) == Orientation::Collinear
        && p.x >= a.x.min(b.x)
        && p.x <= a.x.max(b.x)
        && p.y >= a.y.min(b.y)
        && p.y <= a.y.max(b.y)
}

#[inline]
pub fn segments_properly_cross(a: Point, b: Point, c: Point, d: Point) -> bool {
    let o1 = orient(a, b, c);
    let o2 = orient(a, b, d);
    let o3 = orient(c, d, a);
    let o4 = orient(c, d, b);
    o1 != Orientation::Collinear
        && o2 != Orientation::Collinear
        && o3 != Orientation::Collinear
        && o4 != Orientation::Collinear
        && o1 != o2
        && o3 != o4
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointLocation {
    Inside,
    Boundary,
    Outside,
}

pub fn locate_point(p: Point, polygon: &[Point]) -> PointLocation {
    let mut inside = false;
    for (a, b) in ring_edges(polygon) {
        if on_segment(a, b, p) {
            return PointLocation::Boundary;
        }
        if (a.y > p.y) != (b.y > p.y) && (orient(a, b, p) == Orientation::Ccw) == (b.y > a.y) {
            inside = !inside;
        }
    }
    if inside {
        PointLocation::Inside
    } else {
        PointLocation::Outside
    }
}

#[inline]
pub fn is_reflex_vertex(prev: Point, v: Point, next: Point, area2: f64) -> bool {
    let turn = orient(prev, v, next);
    if area2 >= 0.0 {
        turn == Orientation::Cw
    } else {
        turn == Orientation::Ccw
    }
}

#[inline]
pub fn line_segment_intersection(a1: Point, a2: Point, b1: Point, b2: Point) -> Option<f64> {
    let dir_a = a2 - a1;
    let edge_b = b2 - b1;
    let denom = dir_a.cross(edge_b);
    if denom == 0.0 {
        return None;
    }
    let t = (b1 - a1).cross(dir_a) / denom;
    (0.0..=1.0).contains(&t).then_some(t)
}

#[inline]
pub fn project_onto_segment(p: Point, a: Point, b: Point) -> Point {
    let d = b - a;
    let len_sq = d.norm_sq();
    if len_sq == 0.0 {
        return a;
    }
    let t = ((p - a).dot(d) / len_sq).clamp(0.0, 1.0);
    a + d * t
}

#[inline]
pub(crate) fn dist_sq_point_segment(p: Point, a: Point, b: Point) -> f64 {
    (p - project_onto_segment(p, a, b)).norm_sq()
}

#[inline]
pub fn ring_edges(ring: &[Point]) -> impl Iterator<Item = (Point, Point)> + '_ {
    ring.iter()
        .enumerate()
        .map(|(i, &a)| (a, ring[(i + 1) % ring.len()]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::signed_area2;
    use crate::test_utils::{p, square_vertices};

    #[test]
    fn orientation_signs() {
        assert_eq!(
            orient(p(0.0, 0.0), p(1.0, 0.0), p(0.0, 1.0)),
            Orientation::Ccw
        );
        assert_eq!(
            orient(p(0.0, 0.0), p(1.0, 0.0), p(0.0, -1.0)),
            Orientation::Cw
        );
        assert_eq!(
            orient(p(0.0, 0.0), p(1.0, 0.0), p(2.0, 0.0)),
            Orientation::Collinear
        );
    }

    #[test]
    fn on_segment_cases() {
        let (a, b) = (p(0.0, 0.0), p(10.0, 0.0));
        assert!(on_segment(a, b, p(5.0, 0.0)));
        assert!(on_segment(a, b, a));
        assert!(on_segment(a, b, b));
        assert!(!on_segment(a, b, p(11.0, 0.0)));
        assert!(!on_segment(a, b, p(5.0, 0.1)));
    }

    #[test]
    fn proper_crossing() {
        assert!(segments_properly_cross(
            p(0.0, 0.0),
            p(10.0, 10.0),
            p(0.0, 10.0),
            p(10.0, 0.0)
        ));
        assert!(!segments_properly_cross(
            p(0.0, 0.0),
            p(1.0, 1.0),
            p(5.0, 5.0),
            p(6.0, 4.0)
        ));
    }

    #[test]
    fn shared_endpoint_is_not_proper() {
        assert!(!segments_properly_cross(
            p(0.0, 0.0),
            p(10.0, 0.0),
            p(10.0, 0.0),
            p(10.0, 10.0)
        ));
    }

    #[test]
    fn vertex_touch_is_not_proper() {
        assert!(!segments_properly_cross(
            p(0.0, 0.0),
            p(10.0, 0.0),
            p(5.0, 0.0),
            p(5.0, 5.0)
        ));
    }

    #[test]
    fn collinear_overlap_is_not_proper() {
        assert!(!segments_properly_cross(
            p(0.0, 0.0),
            p(10.0, 0.0),
            p(5.0, 0.0),
            p(15.0, 0.0)
        ));
    }

    #[test]
    fn locate_inside_outside() {
        assert_eq!(
            locate_point(p(5.0, 5.0), &square_vertices()),
            PointLocation::Inside
        );
        assert_eq!(
            locate_point(p(15.0, 5.0), &square_vertices()),
            PointLocation::Outside
        );
    }

    #[test]
    fn locate_boundary() {
        assert_eq!(
            locate_point(p(0.0, 0.0), &square_vertices()),
            PointLocation::Boundary
        );
        assert_eq!(
            locate_point(p(5.0, 0.0), &square_vertices()),
            PointLocation::Boundary
        );
    }

    #[test]
    fn locate_ray_through_vertex() {
        assert_eq!(
            locate_point(p(-5.0, 0.0), &square_vertices()),
            PointLocation::Outside
        );
        assert_eq!(
            locate_point(p(-5.0, 10.0), &square_vertices()),
            PointLocation::Outside
        );
    }

    #[test]
    fn locate_empty_ring() {
        assert_eq!(locate_point(p(0.0, 0.0), &[]), PointLocation::Outside);
    }

    #[test]
    fn reflex_vertex_of_a_ccw_square() {
        let l = [
            p(0.0, 0.0),
            p(10.0, 0.0),
            p(10.0, 10.0),
            p(5.0, 10.0),
            p(5.0, 5.0),
            p(0.0, 5.0),
        ];
        let area2 = signed_area2(&l);
        assert!(area2 > 0.0);
        for i in 0..l.len() {
            let prev = l[(i + l.len() - 1) % l.len()];
            let next = l[(i + 1) % l.len()];
            let reflex = is_reflex_vertex(prev, l[i], next, area2);
            assert_eq!(reflex, i == 4, "vertex {i} ({:?})", l[i]);
        }
    }

    #[test]
    fn reflex_test_respects_cw_winding() {
        let mut l = vec![
            p(0.0, 0.0),
            p(10.0, 0.0),
            p(10.0, 10.0),
            p(5.0, 10.0),
            p(5.0, 5.0),
            p(0.0, 5.0),
        ];
        l.reverse();
        let area2 = signed_area2(&l);
        assert!(area2 < 0.0);
        let reflex_index = l.iter().position(|&v| v == p(5.0, 5.0)).unwrap();
        for i in 0..l.len() {
            let prev = l[(i + l.len() - 1) % l.len()];
            let next = l[(i + 1) % l.len()];
            let reflex = is_reflex_vertex(prev, l[i], next, area2);
            assert_eq!(reflex, i == reflex_index, "vertex {i} ({:?})", l[i]);
        }
    }

    #[test]
    fn collinear_vertex_is_not_reflex() {
        assert!(!is_reflex_vertex(
            p(0.0, 0.0),
            p(5.0, 0.0),
            p(10.0, 0.0),
            10.0 * 10.0,
        ));
    }

    #[test]
    fn line_extension_crosses_a_perpendicular_edge() {
        let t = line_segment_intersection(p(0.0, 0.0), p(0.0, 1.0), p(-5.0, 5.0), p(5.0, 5.0));
        assert!((t.unwrap() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn line_extension_only_needs_the_infinite_side_unbounded() {
        let t = line_segment_intersection(p(0.0, 0.0), p(0.0, 1.0), p(-5.0, 5.0), p(5.0, 5.0));
        assert!(t.is_some());
    }

    #[test]
    fn line_extension_misses_outside_the_edge_bounds() {
        let t = line_segment_intersection(p(0.0, 0.0), p(0.0, 1.0), p(1.0, 5.0), p(5.0, 5.0));
        assert!(t.is_none());
    }

    #[test]
    fn line_extension_parallel_lines_do_not_cross() {
        let t = line_segment_intersection(p(0.0, 0.0), p(0.0, 1.0), p(1.0, 0.0), p(1.0, 1.0));
        assert!(t.is_none());
    }
}
