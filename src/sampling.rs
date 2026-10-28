use std::collections::HashMap;

use framework::{
    Building, EdgeGrid, Instance, Point, bbox, boundary_points, boundary_points_convex, is_visible,
    ring_edges,
};

const EVENT_EPS: f64 = 1e-9;
const EVENT_KEY_SCALE: f64 = 1e6;

#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub position: Point,
    pub building: u32,
    pub kind: CandidateKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateKind {
    Uniform,
    Event,
    Local,
    Probe,
    Incumbent,
    Breakpoint,
}

pub fn boundary_candidates(ring: &[Point], spacing: f64, building: u32) -> Vec<Candidate> {
    boundary_points(ring, spacing)
        .into_iter()
        .map(|(position, _)| Candidate {
            position,
            building,
            kind: CandidateKind::Uniform,
        })
        .collect()
}

pub fn boundary_candidates_convex_only(
    ring: &[Point],
    spacing: f64,
    building: u32,
) -> Vec<Candidate> {
    boundary_points_convex(ring, spacing)
        .into_iter()
        .map(|(position, _)| Candidate {
            position,
            building,
            kind: CandidateKind::Uniform,
        })
        .collect()
}

pub fn candidates_for_instance(
    instance: &Instance,
    spacing: f64,
    event_radius: f64,
    event_limit: usize,
) -> Vec<Candidate> {
    let mut candidates: Vec<_> = instance
        .buildings
        .iter()
        .enumerate()
        .flat_map(|(i, b)| boundary_candidates(&b.vertices, spacing, i as u32))
        .collect();
    if event_radius > 0.0 {
        candidates.extend(event_candidates(instance, event_radius, event_limit));
    }
    dedup_candidates(candidates)
}

pub fn event_candidates(instance: &Instance, radius: f64, limit: usize) -> Vec<Candidate> {
    assert!(radius > 0.0, "event radius must be positive");
    let vertices = boundary_vertices(&instance.buildings);
    if vertices.is_empty() {
        return Vec::new();
    }
    let vertex_grid = VertexGrid::new(&vertices, radius);
    let edge_grid = EdgeGrid::with_auto_cell_size(&instance.buildings);
    let mut ray_grid = EdgeRayGrid::with_auto_cell_size(&instance.buildings);
    let span = scene_span(&instance.buildings);
    let mut candidates: HashMap<(i64, i64), EventAccum> = HashMap::new();

    for (source_index, &source) in vertices.iter().enumerate() {
        for target_index in vertex_grid.neighbors(source_index) {
            if source_index == target_index {
                continue;
            }
            let target = vertices[target_index];
            if (target.point - source.point).norm_sq() <= EVENT_EPS * EVENT_EPS {
                continue;
            }
            if !is_visible(source.point, target.point, &instance.buildings, &edge_grid) {
                continue;
            }
            let Some((hit, hit_dist)) =
                first_boundary_hit_after(&mut ray_grid, source, target, span)
            else {
                continue;
            };
            let target_dist = (target.point - source.point).norm();
            candidates
                .entry(candidate_key(hit.position))
                .and_modify(|accum| accum.add(target_dist, hit_dist))
                .or_insert_with(|| EventAccum::new(hit, target_dist, hit_dist));
        }
    }

    let mut candidates: Vec<_> = candidates.into_values().collect();
    candidates.sort_by(|a, b| {
        b.support
            .cmp(&a.support)
            .then(a.min_target_dist.total_cmp(&b.min_target_dist))
            .then(a.min_hit_dist.total_cmp(&b.min_hit_dist))
            .then(a.candidate.building.cmp(&b.candidate.building))
            .then(a.candidate.position.x.total_cmp(&b.candidate.position.x))
            .then(a.candidate.position.y.total_cmp(&b.candidate.position.y))
    });
    if limit > 0 {
        candidates.truncate(limit);
    }
    let mut candidates: Vec<_> = candidates
        .into_iter()
        .map(|accum| accum.candidate)
        .collect();
    candidates.sort_by(|a, b| {
        a.building
            .cmp(&b.building)
            .then(a.position.x.total_cmp(&b.position.x))
            .then(a.position.y.total_cmp(&b.position.y))
    });
    candidates
}

struct EventAccum {
    candidate: Candidate,
    support: usize,
    min_target_dist: f64,
    min_hit_dist: f64,
}

impl EventAccum {
    fn new(candidate: Candidate, target_dist: f64, hit_dist: f64) -> Self {
        Self {
            candidate,
            support: 1,
            min_target_dist: target_dist,
            min_hit_dist: hit_dist,
        }
    }

    fn add(&mut self, target_dist: f64, hit_dist: f64) {
        self.support += 1;
        self.min_target_dist = self.min_target_dist.min(target_dist);
        self.min_hit_dist = self.min_hit_dist.min(hit_dist);
    }
}

#[derive(Clone, Copy)]
struct Vertex {
    point: Point,
    building: usize,
}

fn boundary_vertices(buildings: &[Building]) -> Vec<Vertex> {
    buildings
        .iter()
        .enumerate()
        .flat_map(|(building, b)| {
            b.vertices
                .iter()
                .map(move |&point| Vertex { point, building })
        })
        .collect()
}

struct VertexGrid {
    radius_sq: f64,
    cell_size: f64,
    cells: HashMap<(i64, i64), Vec<usize>>,
    points: Vec<Point>,
}

impl VertexGrid {
    fn new(vertices: &[Vertex], radius: f64) -> Self {
        let mut cells: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (i, vertex) in vertices.iter().enumerate() {
            cells
                .entry(vertex_cell(vertex.point, radius))
                .or_default()
                .push(i);
        }
        Self {
            radius_sq: radius * radius,
            cell_size: radius,
            cells,
            points: vertices.iter().map(|v| v.point).collect(),
        }
    }

    fn neighbors(&self, source: usize) -> Vec<usize> {
        let p = self.points[source];
        let (cx, cy) = vertex_cell(p, self.cell_size);
        let mut out = Vec::new();
        for y in cy - 1..=cy + 1 {
            for x in cx - 1..=cx + 1 {
                let Some(cell) = self.cells.get(&(x, y)) else {
                    continue;
                };
                out.extend(
                    cell.iter()
                        .copied()
                        .filter(|&i| (self.points[i] - p).norm_sq() <= self.radius_sq),
                );
            }
        }
        out
    }
}

fn vertex_cell(p: Point, cell_size: f64) -> (i64, i64) {
    (
        (p.x / cell_size).floor() as i64,
        (p.y / cell_size).floor() as i64,
    )
}

fn first_boundary_hit_after(
    ray_grid: &mut EdgeRayGrid,
    source: Vertex,
    target: Vertex,
    span: f64,
) -> Option<(Candidate, f64)> {
    let dir = target.point - source.point;
    let target_dist = dir.norm();
    if target_dist <= EVENT_EPS {
        return None;
    }
    let unit = dir * (1.0 / target_dist);
    ray_grid.first_hit_after(
        source.point,
        unit,
        target_dist,
        span,
        source.building,
        target.building,
    )
}

#[derive(Clone, Copy)]
struct IndexedEdge {
    a: Point,
    b: Point,
    building: usize,
}

struct EdgeRayGrid {
    origin: Point,
    cell_size: f64,
    cols: usize,
    rows: usize,
    edges: Vec<IndexedEdge>,
    cells: Vec<Vec<u32>>,
    seen: Vec<u32>,
    stamp: u32,
}

impl EdgeRayGrid {
    fn with_auto_cell_size(buildings: &[Building]) -> Self {
        let edges_count: usize = buildings.iter().map(|b| b.vertices.len()).sum();
        let (min, max) = bbox(buildings);
        let area = (max.x - min.x) * (max.y - min.y);
        let cell_size = if edges_count == 0 || area <= 0.0 {
            1.0
        } else {
            (area / edges_count as f64).sqrt()
        };
        Self::new(buildings, cell_size)
    }

    fn new(buildings: &[Building], cell_size: f64) -> Self {
        assert!(cell_size > 0.0);
        let (min, max) = bbox(buildings);
        let cols = (((max.x - min.x) / cell_size).ceil() as usize).max(1);
        let rows = (((max.y - min.y) / cell_size).ceil() as usize).max(1);
        let mut grid = Self {
            origin: min,
            cell_size,
            cols,
            rows,
            edges: Vec::new(),
            cells: vec![Vec::new(); cols * rows],
            seen: Vec::new(),
            stamp: 1,
        };
        for (building, b) in buildings.iter().enumerate() {
            for (a, c) in ring_edges(&b.vertices) {
                let edge_id = grid.edges.len() as u32;
                grid.edges.push(IndexedEdge { a, b: c, building });
                let (c0, c1) = (grid.col(a.x.min(c.x)), grid.col(a.x.max(c.x)));
                let (r0, r1) = (grid.row(a.y.min(c.y)), grid.row(a.y.max(c.y)));
                for r in r0..=r1 {
                    for col in c0..=c1 {
                        grid.cells[r * cols + col].push(edge_id);
                    }
                }
            }
        }
        grid.seen = vec![0; grid.edges.len()];
        grid
    }

    fn first_hit_after(
        &mut self,
        origin: Point,
        unit: Point,
        min_dist: f64,
        max_dist: f64,
        exclude_a: usize,
        exclude_b: usize,
    ) -> Option<(Candidate, f64)> {
        let (mut t, t_end) = self.ray_box_interval(origin, unit, max_dist)?;
        t = t.max(0.0);
        if t > t_end {
            return None;
        }

        self.advance_stamp();
        let p = origin + unit * t;
        let mut col = self.col(p.x);
        let mut row = self.row(p.y);
        let step_col: isize = if unit.x > 0.0 {
            1
        } else if unit.x < 0.0 {
            -1
        } else {
            0
        };
        let step_row: isize = if unit.y > 0.0 {
            1
        } else if unit.y < 0.0 {
            -1
        } else {
            0
        };
        let next_x = if step_col > 0 {
            self.origin.x + (col + 1) as f64 * self.cell_size
        } else {
            self.origin.x + col as f64 * self.cell_size
        };
        let next_y = if step_row > 0 {
            self.origin.y + (row + 1) as f64 * self.cell_size
        } else {
            self.origin.y + row as f64 * self.cell_size
        };
        let mut t_max_x = if step_col == 0 {
            f64::INFINITY
        } else {
            ((next_x - origin.x) / unit.x).max(t)
        };
        let mut t_max_y = if step_row == 0 {
            f64::INFINITY
        } else {
            ((next_y - origin.y) / unit.y).max(t)
        };
        let t_delta_x = if step_col == 0 {
            f64::INFINITY
        } else {
            self.cell_size / unit.x.abs()
        };
        let t_delta_y = if step_row == 0 {
            f64::INFINITY
        } else {
            self.cell_size / unit.y.abs()
        };

        let mut best_dist = f64::INFINITY;
        let mut best = None;
        loop {
            let cell_end = t_max_x.min(t_max_y).min(t_end);
            let cell_index = row * self.cols + col;
            for edge_id in self.cells[cell_index].clone() {
                let edge_id = edge_id as usize;
                if self.seen[edge_id] == self.stamp {
                    continue;
                }
                self.seen[edge_id] = self.stamp;
                let edge = self.edges[edge_id];
                if edge.building == exclude_a || edge.building == exclude_b {
                    continue;
                }
                let Some(dist) = ray_segment_distance(origin, unit, edge.a, edge.b) else {
                    continue;
                };
                if dist <= min_dist + EVENT_EPS || dist > max_dist + EVENT_EPS {
                    continue;
                }
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((
                        Candidate {
                            position: origin + unit * dist,
                            building: edge.building as u32,
                            kind: CandidateKind::Event,
                        },
                        dist,
                    ));
                }
            }
            if best_dist <= cell_end + EVENT_EPS || cell_end >= t_end {
                return best;
            }
            let advance_x = t_max_x <= t_max_y;
            let advance_y = t_max_y <= t_max_x;
            if advance_x {
                let next = col as isize + step_col;
                if !(0..self.cols as isize).contains(&next) {
                    return best;
                }
                col = next as usize;
                t_max_x += t_delta_x;
            }
            if advance_y {
                let next = row as isize + step_row;
                if !(0..self.rows as isize).contains(&next) {
                    return best;
                }
                row = next as usize;
                t_max_y += t_delta_y;
            }
        }
    }

    fn ray_box_interval(&self, origin: Point, unit: Point, max_dist: f64) -> Option<(f64, f64)> {
        let min = self.origin;
        let max = Point::new(
            self.origin.x + self.cols as f64 * self.cell_size,
            self.origin.y + self.rows as f64 * self.cell_size,
        );
        let mut t0: f64 = 0.0;
        let mut t1 = max_dist;
        for (o, d, lo, hi) in [
            (origin.x, unit.x, min.x, max.x),
            (origin.y, unit.y, min.y, max.y),
        ] {
            if d.abs() <= EVENT_EPS {
                if o < lo || o > hi {
                    return None;
                }
                continue;
            }
            let a = (lo - o) / d;
            let b = (hi - o) / d;
            t0 = t0.max(a.min(b));
            t1 = t1.min(a.max(b));
            if t0 > t1 {
                return None;
            }
        }
        Some((t0, t1))
    }

    fn advance_stamp(&mut self) {
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            self.seen.fill(0);
            self.stamp = 1;
        }
    }

    fn col(&self, x: f64) -> usize {
        grid_index(x - self.origin.x, self.cell_size, self.cols)
    }

    fn row(&self, y: f64) -> usize {
        grid_index(y - self.origin.y, self.cell_size, self.rows)
    }
}

fn grid_index(offset: f64, cell_size: f64, len: usize) -> usize {
    ((offset / cell_size).floor() as isize).clamp(0, len as isize - 1) as usize
}

fn ray_segment_distance(origin: Point, dir: Point, a: Point, b: Point) -> Option<f64> {
    let edge = b - a;
    let denom = dir.cross(edge);
    if denom.abs() <= EVENT_EPS {
        return None;
    }
    let offset = a - origin;
    let distance = offset.cross(edge) / denom;
    let edge_t = offset.cross(dir) / denom;
    if distance >= -EVENT_EPS && (-EVENT_EPS..=1.0 + EVENT_EPS).contains(&edge_t) {
        Some(distance.max(0.0))
    } else {
        None
    }
}

fn scene_span(buildings: &[Building]) -> f64 {
    let (min, max) = bbox(buildings);
    (max - min).norm() * 2.0 + 1.0
}

pub(crate) fn dedup_candidates(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut unique = HashMap::new();
    for candidate in candidates {
        unique
            .entry(candidate_key(candidate.position))
            .or_insert(candidate);
    }
    let mut candidates: Vec<_> = unique.into_values().collect();
    candidates.sort_by(|a, b| {
        a.building
            .cmp(&b.building)
            .then(a.position.x.total_cmp(&b.position.x))
            .then(a.position.y.total_cmp(&b.position.y))
    });
    candidates
}

pub(crate) fn candidate_key(point: Point) -> (i64, i64) {
    (
        (point.x * EVENT_KEY_SCALE).round() as i64,
        (point.y * EVENT_KEY_SCALE).round() as i64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use framework::test_utils::{p, square_vertices};

    #[test]
    fn candidates_carry_building_index() {
        let candidates = boundary_candidates(&square_vertices(), 4.0, 3);
        assert_eq!(candidates.len(), 12);
        assert!(candidates.iter().all(|c| c.building == 3));
    }

    #[test]
    fn visible_vertex_pair_adds_forward_boundary_event() {
        let instance = Instance {
            buildings: vec![
                Building {
                    id: 0,
                    vertices: vec![p(0.0, 0.0), p(2.0, 0.0), p(2.0, -2.0), p(0.0, -2.0)],
                },
                Building {
                    id: 1,
                    vertices: vec![p(10.0, 0.0), p(12.0, 0.0), p(12.0, -2.0), p(10.0, -2.0)],
                },
                Building {
                    id: 2,
                    vertices: vec![p(20.0, -1.0), p(22.0, -1.0), p(22.0, 1.0), p(20.0, 1.0)],
                },
            ],
            k: 1,
            tau: 0.5,
        };

        let candidates = event_candidates(&instance, 30.0, 0);

        assert!(candidates.iter().any(|c| {
            c.building == 2 && (c.position.x - 20.0).abs() < 1e-8 && c.position.y.abs() < 1e-8
        }));
    }
}
