use std::collections::HashMap;

use super::grid::bbox;
use super::perimeter::signed_area2;
use super::predicates::{Orientation, dist_sq_point_segment, orient};
use crate::types::{Building, Point};
use spade::handles::FixedDirectedEdgeHandle;
use spade::{ConstrainedDelaunayTriangulation, Point2, PositionInTriangulation, Triangulation};

type Cdt = ConstrainedDelaunayTriangulation<Point2<f64>>;

const NONE: u32 = u32::MAX;

pub struct FreeMesh {
    cdt: Cdt,
    face_index: HashMap<usize, u32>,
    vertex_index: HashMap<(u64, u64), u32>,
    verts: Vec<Point>,
    tris: Vec<[u32; 3]>,
    neighbors: Vec<[u32; 3]>,
    constraint: Vec<[bool; 3]>,
    free: Vec<bool>,
    tri_building: Vec<u32>,
    wall_index: HashMap<(u32, u32), (u32, u32, u32)>,
}

#[derive(Clone, Copy, Debug)]
pub enum Start {
    Vertex(u32),
    Face(u32),
    Edge(u32, u32),
    Outside,
}

pub struct TargetSet {
    positions: Vec<Point>,
    tri_targets: Vec<Vec<u32>>,
}

struct Frame {
    tri: u32,
    entry: u8,
    r: Point,
    l: Point,
}

impl FreeMesh {
    pub fn build(buildings: &[Building]) -> Self {
        let mut vertex_index: HashMap<(u64, u64), u32> = HashMap::new();
        let mut points: Vec<Point2<f64>> = Vec::new();
        let mut rings: Vec<Vec<u32>> = Vec::with_capacity(buildings.len());
        for b in buildings {
            rings.push(
                b.vertices
                    .iter()
                    .map(|v| {
                        *vertex_index
                            .entry((v.x.to_bits(), v.y.to_bits()))
                            .or_insert_with(|| {
                                points.push(Point2::new(v.x, v.y));
                                (points.len() - 1) as u32
                            })
                    })
                    .collect(),
            );
        }
        let mut wall_index: HashMap<(u32, u32), (u32, u32, u32)> = HashMap::new();
        for (b, ring) in rings.iter().enumerate() {
            for i in 0..ring.len() {
                let (a, c) = (ring[i], ring[(i + 1) % ring.len()]);
                if a != c {
                    wall_index
                        .entry((a.min(c), a.max(c)))
                        .or_insert((b as u32, i as u32, a));
                }
            }
        }
        let edges: Vec<[usize; 2]> = wall_index
            .keys()
            .map(|&(a, b)| [a as usize, b as usize])
            .collect();
        if !points.is_empty() {
            let (min, max) = bbox(buildings);
            let pad = 0.01 * (max.x - min.x + max.y - min.y) + 1.0;
            points.push(Point2::new(min.x - pad, min.y - pad));
            points.push(Point2::new(max.x + pad, min.y - pad));
            points.push(Point2::new(max.x + pad, max.y + pad));
            points.push(Point2::new(min.x - pad, max.y + pad));
        }
        let cdt = Cdt::bulk_load_cdt(points, edges).expect("invalid coordinates for CDT");

        let verts: Vec<Point> = cdt
            .vertices()
            .map(|v| Point::new(v.position().x, v.position().y))
            .collect();
        for (key, &i) in &vertex_index {
            let v = verts[i as usize];
            assert_eq!(
                (v.x.to_bits(), v.y.to_bits()),
                *key,
                "bulk_load_cdt did not preserve vertex order"
            );
        }

        let mut face_index = HashMap::new();
        for (i, face) in cdt.inner_faces().enumerate() {
            face_index.insert(face.fix().index(), i as u32);
        }
        let ntris = face_index.len();
        let mut tris = vec![[0u32; 3]; ntris];
        let mut neighbors = vec![[NONE; 3]; ntris];
        let mut constraint = vec![[false; 3]; ntris];
        let mut edge_to_tri: HashMap<(u32, u32), u32> = HashMap::new();
        for face in cdt.inner_faces() {
            let t = face_index[&face.fix().index()];
            for (slot, edge) in face.adjacent_edges().into_iter().enumerate() {
                let from = edge.from().fix().index() as u32;
                tris[t as usize][slot] = from;
                constraint[t as usize][slot] = edge.is_constraint_edge();
                if let Some(nb) = edge.rev().face().as_inner() {
                    neighbors[t as usize][slot] = face_index[&nb.fix().index()];
                }
                edge_to_tri.insert((from, edge.to().fix().index() as u32), t);
            }
        }

        let mut tri_building = vec![NONE; ntris];
        let mut queue: Vec<(u32, u32)> = Vec::new();
        for (b, ring) in rings.iter().enumerate() {
            let area2 = signed_area2(&buildings[b].vertices);
            if area2 == 0.0 {
                continue;
            }
            for i in 0..ring.len() {
                let (a, c) = (ring[i], ring[(i + 1) % ring.len()]);
                if a == c {
                    continue;
                }
                let key = if area2 > 0.0 { (a, c) } else { (c, a) };
                let tri = *edge_to_tri
                    .get(&key)
                    .expect("building edge missing from mesh");
                queue.push((tri, b as u32));
            }
        }
        while let Some((t, b)) = queue.pop() {
            if tri_building[t as usize] != NONE {
                continue;
            }
            tri_building[t as usize] = b;
            for slot in 0..3 {
                let nb = neighbors[t as usize][slot];
                if !constraint[t as usize][slot] && nb != NONE && tri_building[nb as usize] == NONE
                {
                    queue.push((nb, b));
                }
            }
        }
        let free = tri_building.iter().map(|&b| b == NONE).collect();

        Self {
            cdt,
            face_index,
            vertex_index,
            verts,
            tris,
            neighbors,
            constraint,
            free,
            tri_building,
            wall_index,
        }
    }

    pub fn building_at(&self, p: Point) -> Option<usize> {
        let tri = match self.cdt.locate(Point2::new(p.x, p.y)) {
            PositionInTriangulation::OnFace(f) => Some(self.face_index[&f.index()]),
            PositionInTriangulation::OnEdge(e) => self.edge_tri(e),
            _ => None,
        }?;
        let b = self.tri_building[tri as usize];
        (b != NONE).then_some(b as usize)
    }

    fn edge_tri(&self, e: FixedDirectedEdgeHandle) -> Option<u32> {
        let edge = self.cdt.directed_edge(e);
        [edge.face(), edge.rev().face()]
            .into_iter()
            .find_map(|f| f.as_inner().map(|f| self.face_index[&f.fix().index()]))
    }

    pub fn vertex_start(&self, v: Point) -> Option<Start> {
        self.vertex_index
            .get(&(v.x.to_bits(), v.y.to_bits()))
            .map(|&i| Start::Vertex(i))
    }

    pub fn locate_start(&self, p: Point) -> Option<Start> {
        match self.cdt.locate(Point2::new(p.x, p.y)) {
            PositionInTriangulation::OnFace(f) => {
                let t = self.face_index[&f.index()];
                self.free[t as usize].then_some(Start::Face(t))
            }
            PositionInTriangulation::OnVertex(v) => Some(Start::Vertex(v.index() as u32)),
            PositionInTriangulation::OnEdge(e) => {
                let edge = self.cdt.directed_edge(e);
                let mut faces = [None, None];
                for (i, f) in [edge.face(), edge.rev().face()].into_iter().enumerate() {
                    faces[i] = f
                        .as_inner()
                        .map(|f| self.face_index[&f.fix().index()])
                        .filter(|&t| self.free[t as usize]);
                }
                match faces {
                    [Some(a), Some(b)] => Some(Start::Edge(a, b)),
                    [Some(a), None] | [None, Some(a)] => Some(Start::Face(a)),
                    [None, None] => None,
                }
            }
            PositionInTriangulation::OutsideOfConvexHull(_) => Some(Start::Outside),
            _ => None,
        }
    }

    pub fn locate_targets(&self, positions: Vec<Point>) -> TargetSet {
        let mut tri_targets = vec![Vec::new(); self.tris.len()];
        for (i, p) in positions.iter().enumerate() {
            let tri = match self.cdt.locate(Point2::new(p.x, p.y)) {
                PositionInTriangulation::OnFace(f) => Some(self.face_index[&f.index()]),
                PositionInTriangulation::OnEdge(e) => self.edge_tri(e),
                PositionInTriangulation::OnVertex(v) => {
                    self.cdt.vertex(v).out_edges().find_map(|e| {
                        e.face()
                            .as_inner()
                            .map(|f| self.face_index[&f.fix().index()])
                    })
                }
                _ => None,
            };
            if let Some(t) = tri {
                tri_targets[t as usize].push(i as u32);
            }
        }
        TargetSet {
            positions,
            tri_targets,
        }
    }

    pub fn visible_targets(&self, q: Point, start: Start, targets: &TargetSet, d: f64) -> Vec<u32> {
        self.traverse(q, start, targets, d, false).0
    }

    pub fn visible_region(
        &self,
        q: Point,
        start: Start,
        targets: &TargetSet,
        d: f64,
    ) -> VisibleRegion {
        let (targets, boundary) = self.traverse(q, start, targets, d, true);
        let mut region = boundary.unwrap();
        region.targets = targets;
        region
    }

    fn traverse(
        &self,
        q: Point,
        start: Start,
        targets: &TargetSet,
        d: f64,
        want_region: bool,
    ) -> (Vec<u32>, Option<VisibleRegion>) {
        let mut traversal = Traversal {
            mesh: self,
            q,
            d2: d * d,
            targets,
            stack: Vec::new(),
            out: Vec::new(),
            region: want_region.then(VisibleRegion::default),
        };
        match start {
            Start::Face(t) => traversal.seed_face(t),
            Start::Edge(t1, t2) => {
                traversal.seed_face(t1);
                traversal.seed_face(t2);
            }
            Start::Vertex(v) => {
                let handle = spade::handles::FixedVertexHandle::from_index(v as usize);
                for edge in self.cdt.vertex(handle).out_edges() {
                    if let Some(f) = edge.face().as_inner() {
                        let t = self.face_index[&f.fix().index()];
                        if self.free[t as usize] {
                            traversal.seed_face(t);
                        }
                    }
                }
            }
            Start::Outside => {
                for hull_edge in self.cdt.convex_hull() {
                    let edge = if hull_edge.face().as_inner().is_some() {
                        hull_edge
                    } else {
                        hull_edge.rev()
                    };
                    let Some(f) = edge.face().as_inner() else {
                        continue;
                    };
                    let t = self.face_index[&f.fix().index()];
                    let (vf, vt) = (
                        edge.from().fix().index() as u32,
                        edge.to().fix().index() as u32,
                    );
                    let slot = (0..3u8)
                        .find(|&s| {
                            self.tris[t as usize][s as usize] == vf
                                && self.tris[t as usize][(s as usize + 1) % 3] == vt
                        })
                        .expect("hull edge missing from its triangle");
                    traversal.seed_hull_edge(t, slot);
                }
            }
        }
        traversal.run();
        traversal.out.sort_unstable();
        traversal.out.dedup();
        (traversal.out, traversal.region)
    }

    fn edge(&self, tri: u32, slot: u8) -> (Point, Point) {
        let t = &self.tris[tri as usize];
        (
            self.verts[t[slot as usize] as usize],
            self.verts[t[(slot as usize + 1) % 3] as usize],
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct VisibleRegion {
    pub targets: Vec<u32>,
    pub walls: Vec<LitWall>,
    pub rim: Vec<(Point, Point)>,
}

#[derive(Clone, Copy, Debug)]
pub struct LitWall {
    pub a: Point,
    pub b: Point,
    pub building: u32,
    pub edge: u32,
    pub span: (f64, f64),
}

impl VisibleRegion {
    pub fn fan(&self, q: Point) -> (Vec<Point>, Vec<[usize; 3]>) {
        let pieces = self
            .walls
            .iter()
            .map(|w| (w.a, w.b))
            .chain(self.rim.iter().copied());
        let mut vertices = vec![q];
        let mut triangles = Vec::new();
        for (a, b) in pieces {
            let i = vertices.len();
            vertices.push(a);
            vertices.push(b);
            triangles.push([0, i, i + 1]);
        }
        (vertices, triangles)
    }
}

struct Traversal<'a> {
    mesh: &'a FreeMesh,
    q: Point,
    d2: f64,
    targets: &'a TargetSet,
    stack: Vec<Frame>,
    out: Vec<u32>,
    region: Option<VisibleRegion>,
}

impl Traversal<'_> {
    fn seed_face(&mut self, tri: u32) {
        self.collect(tri, None);
        for slot in 0..3u8 {
            let (a, b) = self.mesh.edge(tri, slot);
            if orient(self.q, a, b) == Orientation::Ccw {
                self.expand(tri, slot, a, b);
            }
        }
    }

    fn seed_hull_edge(&mut self, tri: u32, slot: u8) {
        let q = self.q;
        let (x, y) = self.mesh.edge(tri, slot);
        let inner = self.mesh.verts[self.mesh.tris[tri as usize][(slot as usize + 2) % 3] as usize];
        let side = orient(x, y, q);
        if side == Orientation::Collinear || side == orient(x, y, inner) {
            return;
        }
        let (er, el) = match orient(q, x, y) {
            Orientation::Ccw => (x, y),
            Orientation::Cw => (y, x),
            Orientation::Collinear => return,
        };
        if dist_sq_point_segment(q, x, y) > self.d2 {
            return;
        }
        self.stack.push(Frame {
            tri,
            entry: slot,
            r: er,
            l: el,
        });
    }

    fn run(&mut self) {
        while let Some(Frame { tri, entry, r, l }) = self.stack.pop() {
            self.collect(tri, Some((r, l)));
            for slot in 0..3u8 {
                if slot != entry {
                    self.expand(tri, slot, r, l);
                }
            }
        }
    }

    fn collect(&mut self, tri: u32, cone: Option<(Point, Point)>) {
        for &ti in &self.targets.tri_targets[tri as usize] {
            let s = self.targets.positions[ti as usize];
            if (s - self.q).norm_sq() <= self.d2
                && cone.is_none_or(|(r, l)| {
                    orient(self.q, r, s) != Orientation::Cw
                        && orient(self.q, l, s) != Orientation::Ccw
                })
            {
                self.out.push(ti);
            }
        }
    }

    fn expand(&mut self, tri: u32, slot: u8, r: Point, l: Point) {
        let q = self.q;
        let (x, y) = self.mesh.edge(tri, slot);
        let (er, el) = match orient(q, x, y) {
            Orientation::Ccw => (x, y),
            Orientation::Cw => (y, x),
            Orientation::Collinear => (x, y),
        };
        let new_r = if orient(q, r, er) == Orientation::Ccw {
            er
        } else {
            r
        };
        let new_l = if orient(q, l, el) == Orientation::Cw {
            el
        } else {
            l
        };
        if orient(q, new_r, new_l) == Orientation::Cw {
            return;
        }
        if dist_sq_point_segment(q, x, y) > self.d2 {
            return;
        }
        let is_wall = self.mesh.constraint[tri as usize][slot as usize];
        let nb = self.mesh.neighbors[tri as usize][slot as usize];
        let (vx, vy) = (
            self.mesh.tris[tri as usize][slot as usize],
            self.mesh.tris[tri as usize][(slot as usize + 1) % 3],
        );
        if is_wall || nb == NONE {
            if let Some(region) = &mut self.region
                && let (Some(sa), Some(sb)) = (ray_param(q, new_r, x, y), ray_param(q, new_l, x, y))
            {
                let (lo, hi) = (sa.min(sb).clamp(0.0, 1.0), sa.max(sb).clamp(0.0, 1.0));
                if hi > lo {
                    let (p_lo, p_hi) = (x + (y - x) * lo, x + (y - x) * hi);
                    if is_wall {
                        let &(building, edge, from) = self
                            .mesh
                            .wall_index
                            .get(&(vx.min(vy), vx.max(vy)))
                            .expect("wall edge missing provenance");
                        let (a, b, span) = if from == vx {
                            (p_lo, p_hi, (lo, hi))
                        } else {
                            (p_hi, p_lo, (1.0 - hi, 1.0 - lo))
                        };
                        region.walls.push(LitWall {
                            a,
                            b,
                            building,
                            edge,
                            span,
                        });
                    } else {
                        region.rim.push((p_lo, p_hi));
                    }
                }
            }
            return;
        }
        let entry = (0..3u8)
            .find(|&s| {
                self.mesh.tris[nb as usize][s as usize] == vy
                    && self.mesh.tris[nb as usize][(s as usize + 1) % 3] == vx
            })
            .expect("inconsistent mesh adjacency");
        self.stack.push(Frame {
            tri: nb,
            entry,
            r: new_r,
            l: new_l,
        });
    }
}

fn ray_param(q: Point, through: Point, a: Point, b: Point) -> Option<f64> {
    let dir = through - q;
    let e = b - a;
    let denom = e.cross(dir);
    if denom == 0.0 {
        None
    } else {
        Some((q - a).cross(dir) / denom)
    }
}

#[cfg(test)]
impl FreeMesh {
    fn is_free(&self, p: Point) -> Option<bool> {
        match self.cdt.locate(Point2::new(p.x, p.y)) {
            PositionInTriangulation::OnFace(f) => {
                Some(self.free[self.face_index[&f.index()] as usize])
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{EdgeGrid, boundary_points, is_visible, perimeter_samples};
    use crate::test_utils::{p, square};

    #[test]
    fn classifies_interior_and_free() {
        let buildings = vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)];
        let mesh = FreeMesh::build(&buildings);
        assert_eq!(mesh.is_free(p(5.0, 3.0)), Some(false));
        assert_eq!(mesh.is_free(p(25.0, 3.0)), Some(false));
        assert_eq!(mesh.is_free(p(14.0, 5.0)), Some(true));
        assert_eq!(mesh.is_free(p(-1.0, 5.0)), Some(true));
    }

    #[test]
    fn clockwise_ring_classified_too() {
        let mut b = square(0, 0.0, 0.0);
        b.vertices.reverse();
        let mesh = FreeMesh::build(&[b]);
        assert_eq!(mesh.is_free(p(5.0, 3.0)), Some(false));
    }

    #[test]
    fn agrees_with_segment_oracle() {
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
        let mesh = FreeMesh::build(&buildings);
        let grid = EdgeGrid::with_auto_cell_size(&buildings);

        let samples: Vec<Point> = buildings
            .iter()
            .flat_map(|b| perimeter_samples(&b.vertices, 2.0))
            .collect();
        let targets = mesh.locate_targets(samples.clone());

        let queries: Vec<(Point, Start)> = buildings
            .iter()
            .flat_map(|b| boundary_points(&b.vertices, 5.0))
            .map(|(position, probe)| {
                let start = if position == probe {
                    mesh.vertex_start(position).expect("vertex not in mesh")
                } else {
                    mesh.locate_start(probe).expect("probe not in free space")
                };
                (probe, start)
            })
            .chain(
                [p(-40.0, 75.0), p(75.0, -40.0), p(200.0, 200.0)]
                    .map(|q| (q, mesh.locate_start(q).expect("outside point has a start"))),
            )
            .collect();

        for d in [25.0, f64::INFINITY] {
            for &(q, start) in &queries {
                let tea: Vec<u32> = mesh.visible_targets(q, start, &targets, d);
                let oracle: Vec<u32> = (0..samples.len() as u32)
                    .filter(|&i| {
                        let s = samples[i as usize];
                        (s - q).norm_sq() <= d * d && is_visible(q, s, &buildings, &grid)
                    })
                    .collect();
                assert_eq!(tea, oracle, "query at {q:?}, d={d}");
            }
        }
    }

    #[test]
    fn lit_walls_from_probe() {
        let buildings = vec![square(0, 0.0, 0.0)];
        let mesh = FreeMesh::build(&buildings);
        let targets = mesh.locate_targets(Vec::new());
        let probe = p(5.0, -0.5);
        let start = mesh.locate_start(probe).unwrap();
        let region = mesh.visible_region(probe, start, &targets, f64::INFINITY);
        let length: f64 = region.walls.iter().map(|w| (w.b - w.a).norm()).sum();
        assert!((length - 10.0).abs() < 1e-9, "lit length {length}");
        let mut span_total = 0.0;
        for w in &region.walls {
            assert!(w.a.y == 0.0 && w.b.y == 0.0, "unexpected wall {w:?}");
            assert_eq!((w.building, w.edge), (0, 0), "wall {w:?}");
            assert!(w.span.0 < w.span.1 && w.span.0 >= 0.0 && w.span.1 <= 1.0);
            assert!((w.a.x - 10.0 * w.span.0).abs() < 1e-9, "wall {w:?}");
            assert!((w.b.x - 10.0 * w.span.1).abs() < 1e-9, "wall {w:?}");
            span_total += w.span.1 - w.span.0;
        }
        assert!((span_total - 1.0).abs() < 1e-9, "span total {span_total}");
        assert!(!region.rim.is_empty());
        let (verts, tris) = region.fan(probe);
        let area: f64 = tris
            .iter()
            .map(|t| {
                (verts[t[1]] - verts[t[0]])
                    .cross(verts[t[2]] - verts[t[0]])
                    .abs()
                    / 2.0
            })
            .sum();
        let frame_area = 12.4 * 12.4;
        assert!(area > 0.0 && area < frame_area, "fan area {area}");
    }

    #[test]
    fn lit_walls_between_buildings() {
        let buildings = vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)];
        let mesh = FreeMesh::build(&buildings);
        let targets = mesh.locate_targets(Vec::new());
        let probe = p(15.0, 5.0001);
        let start = mesh.locate_start(probe).unwrap();
        let region = mesh.visible_region(probe, start, &targets, f64::INFINITY);
        let right: f64 = region
            .walls
            .iter()
            .filter(|w| w.a.x == 10.0 && w.b.x == 10.0)
            .inspect(|w| assert_eq!((w.building, w.edge), (0, 1), "wall {w:?}"))
            .map(|w| (w.b - w.a).norm())
            .sum();
        let left: f64 = region
            .walls
            .iter()
            .filter(|w| w.a.x == 20.0 && w.b.x == 20.0)
            .inspect(|w| assert_eq!((w.building, w.edge), (1, 3), "wall {w:?}"))
            .map(|w| (w.b - w.a).norm())
            .sum();
        assert!((right - 10.0).abs() < 1e-9, "right wall lit {right}");
        assert!((left - 10.0).abs() < 1e-9, "left wall lit {left}");
    }

    #[test]
    fn building_at_locates_interiors() {
        let buildings = vec![square(0, 0.0, 0.0), square(1, 20.0, 0.0)];
        let mesh = FreeMesh::build(&buildings);
        assert_eq!(mesh.building_at(p(5.0, 3.0)), Some(0));
        assert_eq!(mesh.building_at(p(25.0, 3.0)), Some(1));
        assert_eq!(mesh.building_at(p(14.0, 5.0)), None);
    }

    #[test]
    fn vertex_query_sees_adjacent_walls() {
        let buildings = vec![square(0, 0.0, 0.0)];
        let mesh = FreeMesh::build(&buildings);
        let samples = perimeter_samples(&buildings[0].vertices, 0.5);
        let targets = mesh.locate_targets(samples);
        let start = mesh.vertex_start(p(0.0, 0.0)).unwrap();
        let visible = mesh.visible_targets(p(0.0, 0.0), start, &targets, f64::INFINITY);
        assert_eq!(visible.len(), 40);
    }
}
