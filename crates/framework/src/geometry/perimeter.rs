use std::collections::HashMap;

use crate::types::Point;

use super::predicates::{is_reflex_vertex, project_onto_segment, ring_edges};

pub const BOUNDARY_NUDGE: f64 = 1e-6;

pub const SPATIAL_TOLERANCE: f64 = 0.001;

pub fn perimeter_samples(ring: &[Point], spacing: f64) -> Vec<Point> {
    assert!(spacing > 0.0, "spacing must be positive");
    let perimeter: f64 = ring_edges(ring).map(|(a, b)| (b - a).norm()).sum();
    if perimeter == 0.0 {
        return Vec::new();
    }
    let area2 = signed_area2(ring);
    let n = (perimeter / spacing).ceil() as usize;
    let step = perimeter / n as f64;
    let mut samples = Vec::with_capacity(n);
    let mut acc = 0.0;
    let mut targets = (0..n).map(|i| (i as f64 + 0.5) * step).peekable();
    for (a, b) in ring_edges(ring) {
        let d = b - a;
        let len = d.norm();
        let nudge = edge_nudge(d, len, area2);
        while let Some(&t) = targets.peek() {
            if t > acc + len {
                break;
            }
            let u = if len == 0.0 { 0.0 } else { (t - acc) / len };
            samples.push(a + d * u + nudge);
            targets.next();
        }
        acc += len;
    }
    samples.extend(targets.map(|_| ring[0]));
    samples
}

pub fn boundary_points(ring: &[Point], spacing: f64) -> Vec<(Point, Point)> {
    assert!(spacing > 0.0, "spacing must be positive");
    let area2 = signed_area2(ring);
    let mut out = Vec::new();
    for (a, b) in ring_edges(ring) {
        out.push((a, a));
        let d = b - a;
        let len = d.norm();
        if len == 0.0 {
            continue;
        }
        let nudge = edge_nudge(d, len, area2);
        let n = (len / spacing).floor() as usize;
        for j in 1..=n {
            let position = a + d * (j as f64 / (n + 1) as f64);
            out.push((position, position + nudge));
        }
    }
    out
}

pub fn boundary_points_convex(ring: &[Point], spacing: f64) -> Vec<(Point, Point)> {
    if ring.len() < 3 {
        return boundary_points(ring, spacing);
    }
    let area2 = signed_area2(ring);
    let n = ring.len();
    let reflex: HashMap<(u64, u64), bool> = ring
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let prev = ring[(i + n - 1) % n];
            let next = ring[(i + 1) % n];
            (
                (v.x.to_bits(), v.y.to_bits()),
                is_reflex_vertex(prev, v, next, area2),
            )
        })
        .collect();
    boundary_points(ring, spacing)
        .into_iter()
        .filter(|&(position, probe)| {
            position != probe
                || !reflex
                    .get(&(position.x.to_bits(), position.y.to_bits()))
                    .copied()
                    .unwrap_or(false)
        })
        .collect()
}

pub struct Boundary<'a> {
    ring: &'a [Point],
    cum: Vec<f64>,
}

impl<'a> Boundary<'a> {
    pub fn new(ring: &'a [Point]) -> Self {
        let mut cum = Vec::with_capacity(ring.len() + 1);
        let mut acc = 0.0;
        cum.push(acc);
        for (a, b) in ring_edges(ring) {
            acc += (b - a).norm();
            cum.push(acc);
        }
        Boundary { ring, cum }
    }

    pub fn length(&self) -> f64 {
        *self.cum.last().expect("cum always holds the leading 0")
    }

    pub fn edge_offsets(&self) -> &[f64] {
        &self.cum
    }

    pub fn point_at(&self, d: f64) -> Point {
        let ring = self.ring;
        assert!(!ring.is_empty(), "ring must not be empty");
        let total = self.length();
        if total <= 0.0 {
            return ring[0];
        }
        let s = d.rem_euclid(total);
        let i = self
            .cum
            .partition_point(|&c| c <= s)
            .saturating_sub(1)
            .min(ring.len() - 1);
        let len = self.cum[i + 1] - self.cum[i];
        if len <= 0.0 {
            return ring[i];
        }
        let t = (s - self.cum[i]) / len;
        ring[i] + (ring[(i + 1) % ring.len()] - ring[i]) * t
    }

    pub fn distance_of(&self, p: Point) -> f64 {
        let mut best = (f64::INFINITY, 0.0);
        for (i, (a, b)) in ring_edges(self.ring).enumerate() {
            let on = project_onto_segment(p, a, b);
            let dist = (p - on).norm_sq();
            if dist < best.0 {
                best = (dist, self.cum[i] + (on - a).norm());
            }
        }
        best.1
    }
}

fn edge_nudge(d: Point, len: f64, area2: f64) -> Point {
    if len == 0.0 {
        Point::new(0.0, 0.0)
    } else {
        outward_normal(d, len, area2) * BOUNDARY_NUDGE
    }
}

#[inline]
pub fn signed_area2(ring: &[Point]) -> f64 {
    ring_edges(ring).map(|(a, b)| a.cross(b)).sum()
}

#[inline]
pub fn outward_normal(d: Point, len: f64, area2: f64) -> Point {
    if area2 > 0.0 {
        Point::new(d.y, -d.x) * (1.0 / len)
    } else {
        Point::new(-d.y, d.x) * (1.0 / len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{PointLocation, locate_point};
    use crate::test_utils::{p, square_vertices};

    #[test]
    fn samples_are_evenly_spaced_and_nudged_out() {
        let ring = square_vertices();
        let samples = perimeter_samples(&ring, 0.5);
        assert_eq!(samples.len(), 80);
        for s in &samples {
            assert_eq!(locate_point(*s, &ring), PointLocation::Outside);
            let on_x =
                (s.x + BOUNDARY_NUDGE).abs() < 1e-12 || (s.x - 10.0 - BOUNDARY_NUDGE).abs() < 1e-12;
            let on_y =
                (s.y + BOUNDARY_NUDGE).abs() < 1e-12 || (s.y - 10.0 - BOUNDARY_NUDGE).abs() < 1e-12;
            assert!(on_x || on_y, "sample off boundary: {s:?}");
        }
    }

    #[test]
    fn nudge_respects_ring_orientation() {
        let mut ring = square_vertices();
        ring.reverse();
        for s in perimeter_samples(&ring, 0.5) {
            assert_eq!(locate_point(s, &ring), PointLocation::Outside);
        }
    }

    #[test]
    fn convex_variant_drops_only_the_reflex_vertex() {
        let l = [
            p(0.0, 0.0),
            p(10.0, 0.0),
            p(10.0, 10.0),
            p(5.0, 10.0),
            p(5.0, 5.0),
            p(0.0, 5.0),
        ];
        let all = boundary_points(&l, 100.0);
        let convex = boundary_points_convex(&l, 100.0);
        assert_eq!(all.len(), convex.len() + 1);
        assert!(all.iter().any(|&(pos, _)| pos == p(5.0, 5.0)));
        assert!(!convex.iter().any(|&(pos, _)| pos == p(5.0, 5.0)));
        for &v in &l {
            if v != p(5.0, 5.0) {
                assert!(convex.iter().any(|&(pos, _)| pos == v), "missing {v:?}");
            }
        }
    }

    #[test]
    fn convex_variant_keeps_interior_edge_points() {
        let ring = square_vertices();
        assert_eq!(
            boundary_points(&ring, 3.0).len(),
            boundary_points_convex(&ring, 3.0).len()
        );
    }

    #[test]
    fn boundary_points_cover_vertices_and_edges() {
        let ring = square_vertices();
        let points = boundary_points(&ring, 4.0);
        assert_eq!(points.len(), 12);
        for &(position, probe) in &points {
            assert_ne!(locate_point(position, &ring), PointLocation::Inside);
            assert_ne!(locate_point(probe, &ring), PointLocation::Inside);
        }
        let vertices = points.iter().filter(|(pos, _)| ring.contains(pos)).count();
        assert_eq!(vertices, 4);
    }

    #[test]
    fn vertex_points_are_not_nudged() {
        let ring = square_vertices();
        for (position, probe) in boundary_points(&ring, 100.0) {
            assert_eq!(position, probe);
        }
    }

    #[test]
    fn degenerate_rings() {
        assert!(perimeter_samples(&[], 1.0).is_empty());
        assert!(perimeter_samples(&[p(1.0, 1.0)], 1.0).is_empty());
        assert!(boundary_points(&[], 1.0).is_empty());
        assert_eq!(boundary_points(&[p(1.0, 1.0)], 1.0).len(), 1);
    }

    #[test]
    fn boundary_roundtrips_distance() {
        let ring = square_vertices();
        let wall = Boundary::new(&ring);
        let total = wall.length();
        assert_eq!(total, 40.0);
        for frac in [0.0, 0.1, 0.37, 0.62, 0.93] {
            let d = frac * total;
            let q = wall.point_at(d);
            let back = wall.distance_of(q);
            let diff = (back - d).abs().min(total - (back - d).abs());
            assert!(diff < 1e-9, "d={d} -> {back}");
        }
    }

    #[test]
    fn boundary_wraps_and_handles_degenerates() {
        let ring = square_vertices();
        let wall = Boundary::new(&ring);
        assert_eq!(wall.point_at(45.0), wall.point_at(5.0));
        assert_eq!(wall.point_at(-5.0), wall.point_at(35.0));

        let dot = [p(3.0, 4.0)];
        let wall = Boundary::new(&dot);
        assert_eq!(wall.length(), 0.0);
        assert_eq!(wall.point_at(7.0), p(3.0, 4.0));
    }
}
