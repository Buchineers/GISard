use std::collections::HashMap;

use rayon::prelude::*;

use crate::geometry::{
    BOUNDARY_NUDGE, Boundary, EdgeGrid, FreeMesh, SPATIAL_TOLERANCE, Start, TargetSet,
    VisibleRegion, outward_normal, project_onto_segment, ring_edges, signed_area2,
};
use crate::intervals::IntervalSet;
use crate::types::{Instance, Point, Solution};

const SNAP_DISTANCE_TIE_TOLERANCE: f64 = 1e-9;

pub fn evaluate(instance: &Instance, solution: &Solution) -> Evaluation {
    Evaluator::new(instance).evaluate(&solution.antennas)
}

pub struct Evaluator<'a> {
    instance: &'a Instance,
    mesh: FreeMesh,
    grid: EdgeGrid,
    targets: TargetSet,
    boundaries: Vec<Boundary<'a>>,
    vertex_probes: HashMap<(u64, u64), Point>,
}

pub struct CoverageGoals {
    pub needs: Vec<f64>,
}

impl CoverageGoals {
    pub fn new(perimeters: &[f64], tau: f64) -> Self {
        CoverageGoals {
            needs: perimeters.iter().map(|&p| tau * p).collect(),
        }
    }

    pub fn progress(&self, covered: &[IntervalSet]) -> f64 {
        covered
            .iter()
            .zip(&self.needs)
            .map(|(c, &n)| n.min(c.measure()))
            .sum()
    }

    pub fn gain(&self, view: &[(u32, IntervalSet)], acc: &[IntervalSet], covered: &[f64]) -> f64 {
        view.iter()
            .map(|(b, set)| {
                let b = *b as usize;
                self.needs[b].min(acc[b].union_measure(set)) - self.needs[b].min(covered[b])
            })
            .sum()
    }
}

pub struct AntennaView {
    pub query: Point,
    pub region: VisibleRegion,
    pub covered: Vec<(u32, IntervalSet)>,
}

pub struct Evaluation {
    pub views: Vec<AntennaView>,
    pub covered: Vec<IntervalSet>,
    pub fractions: Vec<f64>,
    pub serviced: Vec<bool>,
    pub contributors: Vec<Vec<(u32, f64)>>,
    pub score: usize,
}

impl Evaluation {
    pub fn serviced_ids(&self, instance: &Instance) -> Vec<u32> {
        instance
            .buildings
            .iter()
            .zip(&self.serviced)
            .filter(|&(_, &s)| s)
            .map(|(b, _)| b.id)
            .collect()
    }

    pub fn score_at(&self, tau: f64) -> usize {
        self.fractions.iter().filter(|&&f| f >= tau).count()
    }
}

impl<'a> Evaluator<'a> {
    pub fn new(instance: &'a Instance) -> Self {
        let mesh = FreeMesh::build(&instance.buildings);
        let grid = EdgeGrid::with_auto_cell_size(&instance.buildings);
        let targets = mesh.locate_targets(Vec::new());
        let boundaries = instance
            .buildings
            .iter()
            .map(|b| Boundary::new(&b.vertices))
            .collect();
        let mut vertex_probes = HashMap::new();
        for b in &instance.buildings {
            let ring = &b.vertices;
            let n = ring.len();
            if n < 2 {
                continue;
            }
            let area2 = signed_area2(ring);
            for i in 0..n {
                let v = ring[i];
                let mut dir = Point::new(0.0, 0.0);
                for (from, to) in [(ring[(i + n - 1) % n], v), (v, ring[(i + 1) % n])] {
                    let d = to - from;
                    let len = d.norm();
                    if len > 0.0 {
                        dir = dir + outward_normal(d, len, area2);
                    }
                }
                let norm = dir.norm();
                if norm > 0.0 {
                    vertex_probes
                        .entry((v.x.to_bits(), v.y.to_bits()))
                        .or_insert(v + dir * (BOUNDARY_NUDGE / norm));
                }
            }
        }
        Self {
            instance,
            mesh,
            grid,
            targets,
            boundaries,
            vertex_probes,
        }
    }

    pub fn instance(&self) -> &'a Instance {
        self.instance
    }

    pub fn perimeter(&self, b: usize) -> f64 {
        self.boundaries[b].length()
    }

    pub fn boundary(&self, b: usize) -> &Boundary<'a> {
        &self.boundaries[b]
    }

    pub fn covered_segments(&self, b: usize, set: &IntervalSet) -> Vec<(Point, Point)> {
        let ring = &self.instance.buildings[b].vertices;
        let cum = self.boundaries[b].edge_offsets();
        let mut out = Vec::new();
        for &(s0, s1) in set.spans() {
            for (i, (a, b)) in ring_edges(ring).enumerate() {
                let (e0, e1) = (cum[i], cum[i + 1]);
                let len = e1 - e0;
                if len == 0.0 || e1 <= s0 || e0 >= s1 {
                    continue;
                }
                let t0 = (s0.max(e0) - e0) / len;
                let t1 = (s1.min(e1) - e0) / len;
                let d = b - a;
                out.push((a + d * t0, a + d * t1));
            }
        }
        out
    }

    fn resolve(&self, p: Point) -> Option<(Point, Start)> {
        if let Some(&probe) = self.vertex_probes.get(&(p.x.to_bits(), p.y.to_bits())) {
            return Some((probe, self.mesh.locate_start(probe)?));
        }
        if let Some((_, _, probe)) = self.snap(p, SPATIAL_TOLERANCE) {
            return Some((probe, self.mesh.locate_start(probe)?));
        }
        Some((p, self.mesh.locate_start(p)?))
    }

    pub fn snap(&self, p: Point, max_dist: f64) -> Option<(usize, Point, Point)> {
        let mut best: Option<(f64, usize, Point, Point)> = None;
        for bi in self.grid.buildings_within(p, max_dist) {
            let ring = &self.instance.buildings[bi].vertices;
            let area2 = signed_area2(ring);
            for (a, c) in ring_edges(ring) {
                let d = c - a;
                let len_sq = d.norm_sq();
                if len_sq == 0.0 {
                    continue;
                }
                let on = project_onto_segment(p, a, c);
                let dist = (p - on).norm_sq();
                let replace = best.is_none_or(|(bd, best_b, ..)| {
                    let distance = dist.sqrt();
                    let best_distance = bd.sqrt();
                    distance < best_distance - SNAP_DISTANCE_TIE_TOLERANCE
                        || ((distance - best_distance).abs() <= SNAP_DISTANCE_TIE_TOLERANCE
                            && (self.instance.buildings[bi].id.to_string(), bi)
                                < (self.instance.buildings[best_b].id.to_string(), best_b))
                });
                if dist <= max_dist * max_dist && replace {
                    let probe = on + outward_normal(d, len_sq.sqrt(), area2) * BOUNDARY_NUDGE;
                    best = Some((dist, bi, on, probe));
                }
            }
        }
        best.map(|(_, b, on, probe)| {
            if let Some(&vp) = self.vertex_probes.get(&(on.x.to_bits(), on.y.to_bits())) {
                (b, on, vp)
            } else {
                (b, on, probe)
            }
        })
    }

    pub fn building_at(&self, p: Point) -> Option<usize> {
        self.mesh.building_at(p)
    }

    pub fn antenna_view(&self, p: Point) -> AntennaView {
        let Some((query, start)) = self.resolve(p) else {
            return AntennaView {
                query: p,
                region: VisibleRegion::default(),
                covered: Vec::new(),
            };
        };
        let region = self
            .mesh
            .visible_region(query, start, &self.targets, f64::INFINITY);
        let mut spans: HashMap<u32, Vec<(f64, f64)>> = HashMap::new();
        for w in &region.walls {
            let cum = self.boundaries[w.building as usize].edge_offsets();
            let e0 = cum[w.edge as usize];
            let len = cum[w.edge as usize + 1] - e0;
            spans
                .entry(w.building)
                .or_default()
                .push((e0 + w.span.0 * len, e0 + w.span.1 * len));
        }
        let mut covered: Vec<(u32, IntervalSet)> = spans
            .into_iter()
            .map(|(b, s)| (b, IntervalSet::from_spans(s)))
            .collect();
        covered.sort_unstable_by_key(|&(b, _)| b);
        AntennaView {
            query,
            region,
            covered,
        }
    }

    pub fn evaluate(&self, antennas: &[Point]) -> Evaluation {
        let views: Vec<AntennaView> = antennas.par_iter().map(|&p| self.antenna_view(p)).collect();
        let n = self.instance.buildings.len();
        let mut covered = vec![IntervalSet::new(); n];
        let mut contributors: Vec<Vec<(u32, f64)>> = vec![Vec::new(); n];
        for (i, view) in views.iter().enumerate() {
            for (b, set) in &view.covered {
                covered[*b as usize].union_with(set);
                contributors[*b as usize].push((i as u32, set.measure()));
            }
        }
        for c in &mut contributors {
            c.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        }
        let fractions: Vec<f64> = covered
            .iter()
            .enumerate()
            .map(|(b, set)| {
                let total = self.perimeter(b);
                if total == 0.0 {
                    0.0
                } else {
                    set.measure() / total
                }
            })
            .collect();
        let serviced: Vec<bool> = fractions.iter().map(|&f| f >= self.instance.tau).collect();
        let score = serviced.iter().filter(|&&s| s).count();
        Evaluation {
            views,
            covered,
            fractions,
            serviced,
            contributors,
            score,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{is_visible, perimeter_samples};
    use crate::test_utils::{instance, p, square};
    use crate::types::Building;

    fn sampled_fractions(instance: &Instance, antennas: &[Point], spacing: f64) -> Vec<f64> {
        let grid = EdgeGrid::with_auto_cell_size(&instance.buildings);
        instance
            .buildings
            .iter()
            .map(|b| {
                let samples = perimeter_samples(&b.vertices, spacing);
                if samples.is_empty() {
                    return 0.0;
                }
                let covered = samples
                    .iter()
                    .filter(|&&s| {
                        antennas
                            .iter()
                            .any(|&a| is_visible(a, s, &instance.buildings, &grid))
                    })
                    .count();
                covered as f64 / samples.len() as f64
            })
            .collect()
    }

    #[test]
    fn corner_antenna_covers_exactly_half() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let eval = Evaluator::new(&inst).evaluate(&[p(0.0, 0.0)]);
        assert_eq!(eval.fractions, vec![0.5]);
        assert_eq!(eval.serviced, vec![true]);
        assert_eq!(eval.score, 1);
        assert_eq!(eval.serviced_ids(&inst), vec![0]);
    }

    #[test]
    fn opposite_corners_cover_exactly_all() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let eval = Evaluator::new(&inst).evaluate(&[p(0.0, 0.0), p(10.0, 10.0)]);
        assert_eq!(eval.fractions, vec![1.0]);
        let contribs = &eval.contributors[0];
        assert_eq!(contribs.len(), 2);
        assert!(contribs.iter().all(|&(_, len)| (len - 20.0).abs() < 1e-9));
    }

    #[test]
    fn distant_antenna_covers_one_face() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let eval = Evaluator::new(&inst).evaluate(&[p(5.0, -100.0)]);
        assert_eq!(eval.fractions, vec![0.25]);
        assert_eq!(eval.score_at(0.25), 1);
        assert_eq!(eval.score_at(0.5), 0);
    }

    #[test]
    fn edge_midpoint_antenna_sees_own_wall() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.25);
        let eval = Evaluator::new(&inst).evaluate(&[p(5.0, 0.0)]);
        assert_eq!(eval.fractions, vec![0.25]);
    }

    #[test]
    fn near_boundary_antenna_snaps_to_wall() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.25);
        let evaluator = Evaluator::new(&inst);
        for p in [p(3.0, 0.000_999), p(3.0, -0.000_999)] {
            assert_eq!(evaluator.evaluate(&[p]).fractions, vec![0.25], "{p:?}");
        }
        assert_eq!(
            evaluator.evaluate(&[p(3.0, 0.001_001)]).fractions,
            vec![0.0]
        );
    }

    #[test]
    fn snap_finds_nearest_edge_point() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let evaluator = Evaluator::new(&inst);
        let (b, on, probe) = evaluator.snap(p(5.0, -3.0), 5.0).unwrap();
        assert_eq!(b, 0);
        assert_eq!(on, p(5.0, 0.0));
        assert!(probe.y < 0.0);
        assert!(evaluator.snap(p(50.0, 50.0), 5.0).is_none());
    }

    #[test]
    fn snap_breaks_distance_ties_like_the_official_evaluator() {
        let inst = instance(vec![square(2, 12.0, 0.0), square(10, 0.0, 0.0)], 0.5);
        let evaluator = Evaluator::new(&inst);
        let (building, on, _) = evaluator.snap(p(11.0, 5.0), 1.0).unwrap();
        assert_eq!(inst.buildings[building].id, 10);
        assert_eq!(on, p(10.0, 5.0));
    }

    #[test]
    fn neighbor_occludes() {
        let inst = instance(vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)], 0.5);
        let eval = Evaluator::new(&inst).evaluate(&[p(-5.0, 5.0)]);
        assert_eq!(eval.fractions[0], 0.25);
        assert_eq!(eval.fractions[1], 0.0);
    }

    #[test]
    fn interior_antenna_sees_nothing() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let eval = Evaluator::new(&inst).evaluate(&[p(5.0, 5.0)]);
        assert_eq!(eval.fractions, vec![0.0]);
        assert!(eval.views[0].covered.is_empty());
    }

    #[test]
    fn no_antennas_no_coverage() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let eval = Evaluator::new(&inst).evaluate(&[]);
        assert_eq!(eval.fractions, vec![0.0]);
        assert_eq!(eval.score, 0);
    }

    #[test]
    fn covered_segments_round_trip() {
        let inst = instance(vec![square(0, 0.0, 0.0)], 0.5);
        let evaluator = Evaluator::new(&inst);
        let eval = evaluator.evaluate(&[p(0.0, 0.0)]);
        let segments = evaluator.covered_segments(0, &eval.covered[0]);
        let length: f64 = segments.iter().map(|&(a, b)| (b - a).norm()).sum();
        assert!((length - 20.0).abs() < 1e-9, "length {length}");
        for (a, b) in segments {
            assert!((a.y == 0.0 && b.y == 0.0) || (a.x == 0.0 && b.x == 0.0));
        }
    }

    #[test]
    fn agrees_with_sampled_estimator() {
        let mut buildings = Vec::new();
        let mut seed = 12345u64;
        let mut rng = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as f64 / (1u64 << 31) as f64
        };
        for i in 0..20u32 {
            let (col, row) = (i % 5, i / 5);
            let x = col as f64 * 30.0 + rng() * 12.0;
            let y = row as f64 * 30.0 + rng() * 12.0;
            let (w, h) = (4.0 + rng() * 10.0, 4.0 + rng() * 10.0);
            buildings.push(Building {
                id: i,
                vertices: vec![p(x, y), p(x + w, y), p(x + w, y + h), p(x, y + h)],
            });
        }
        let antennas: Vec<Point> = [0, 7, 13, 16]
            .iter()
            .map(|&b| buildings[b].vertices[b % 4])
            .chain([p(-5.0, -5.0)])
            .collect();
        let inst = instance(buildings, 0.5);
        let exact = Evaluator::new(&inst).evaluate(&antennas);
        let sampled = sampled_fractions(&inst, &antennas, 0.05);
        for (b, (e, s)) in exact.fractions.iter().zip(&sampled).enumerate() {
            assert!((e - s).abs() < 0.03, "building {b}: exact {e}, sampled {s}");
        }
    }
}
