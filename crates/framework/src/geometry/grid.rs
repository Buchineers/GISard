use crate::types::{Building, Point};

use super::predicates::ring_edges;

pub struct EdgeGrid {
    origin: Point,
    cell_size: f64,
    cols: usize,
    rows: usize,
    cells: Vec<Vec<u32>>,
}

impl EdgeGrid {
    pub fn new(buildings: &[Building], cell_size: f64) -> Self {
        assert!(cell_size > 0.0, "cell_size must be positive");
        let (min, max) = bbox(buildings);
        let cols = (((max.x - min.x) / cell_size).ceil() as usize).max(1);
        let rows = (((max.y - min.y) / cell_size).ceil() as usize).max(1);
        let mut grid = Self {
            origin: min,
            cell_size,
            cols,
            rows,
            cells: vec![Vec::new(); cols * rows],
        };
        for (i, b) in buildings.iter().enumerate() {
            for (a, c) in ring_edges(&b.vertices) {
                let (c0, c1) = (grid.col(a.x.min(c.x)), grid.col(a.x.max(c.x)));
                let (r0, r1) = (grid.row(a.y.min(c.y)), grid.row(a.y.max(c.y)));
                for r in r0..=r1 {
                    for col in c0..=c1 {
                        grid.cells[r * cols + col].push(i as u32);
                    }
                }
            }
        }
        for cell in &mut grid.cells {
            cell.sort_unstable();
            cell.dedup();
        }
        grid
    }

    pub fn with_auto_cell_size(buildings: &[Building]) -> Self {
        let edges: usize = buildings.iter().map(|b| b.vertices.len()).sum();
        let (min, max) = bbox(buildings);
        let area = (max.x - min.x) * (max.y - min.y);
        let cell_size = if edges == 0 || area <= 0.0 {
            1.0
        } else {
            (area / edges as f64).sqrt()
        };
        Self::new(buildings, cell_size)
    }

    pub fn buildings_near_segment(&self, p: Point, q: Point) -> Vec<usize> {
        let mut found: Vec<u32> = Vec::new();
        let d = q - p;
        let (r0, r1) = (self.row(p.y.min(q.y)), self.row(p.y.max(q.y)));
        for r in r0..=r1 {
            let (x0, x1) = if d.y == 0.0 {
                (p.x.min(q.x), p.x.max(q.x))
            } else {
                let slab_lo = self.origin.y + r as f64 * self.cell_size;
                let t0 = ((slab_lo - p.y) / d.y).clamp(0.0, 1.0);
                let t1 = ((slab_lo + self.cell_size - p.y) / d.y).clamp(0.0, 1.0);
                let (xa, xb) = (p.x + t0 * d.x, p.x + t1 * d.x);
                (xa.min(xb), xa.max(xb))
            };
            for c in self.col(x0)..=self.col(x1) {
                found.extend_from_slice(&self.cells[r * self.cols + c]);
            }
        }
        sorted_indices(found)
    }

    pub fn buildings_within(&self, p: Point, radius: f64) -> Vec<usize> {
        let mut found: Vec<u32> = Vec::new();
        for r in self.row(p.y - radius)..=self.row(p.y + radius) {
            for c in self.col(p.x - radius)..=self.col(p.x + radius) {
                found.extend_from_slice(&self.cells[r * self.cols + c]);
            }
        }
        sorted_indices(found)
    }

    fn col(&self, x: f64) -> usize {
        index(x - self.origin.x, self.cell_size, self.cols)
    }

    fn row(&self, y: f64) -> usize {
        index(y - self.origin.y, self.cell_size, self.rows)
    }
}

fn index(offset: f64, cell_size: f64, len: usize) -> usize {
    ((offset / cell_size).floor() as isize).clamp(0, len as isize - 1) as usize
}

fn sorted_indices(mut found: Vec<u32>) -> Vec<usize> {
    found.sort_unstable();
    found.dedup();
    found.into_iter().map(|i| i as usize).collect()
}

pub fn bbox(buildings: &[Building]) -> (Point, Point) {
    let mut min = Point::new(f64::INFINITY, f64::INFINITY);
    let mut max = Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY);
    for v in buildings.iter().flat_map(|b| &b.vertices) {
        min = Point::new(min.x.min(v.x), min.y.min(v.y));
        max = Point::new(max.x.max(v.x), max.y.max(v.y));
    }
    if min.x > max.x {
        (Point::new(0.0, 0.0), Point::new(0.0, 0.0))
    } else {
        (min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::p;

    fn unit_square(id: u32, x: f64, y: f64) -> Building {
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

    #[test]
    fn finds_blocking_building_and_skips_distant() {
        let buildings = vec![unit_square(0, 0.0, 0.0), unit_square(1, 100.0, 0.0)];
        let grid = EdgeGrid::new(&buildings, 10.0);
        let near = grid.buildings_near_segment(p(-5.0, 5.0), p(15.0, 5.0));
        assert!(near.contains(&0));
        assert!(!near.contains(&1));
    }

    #[test]
    fn finds_buildings_along_diagonal() {
        let buildings = vec![
            unit_square(0, 0.0, 0.0),
            unit_square(1, 50.0, 50.0),
            unit_square(2, 100.0, 0.0),
        ];
        let grid = EdgeGrid::new(&buildings, 10.0);
        let near = grid.buildings_near_segment(p(-5.0, -5.0), p(115.0, 115.0));
        assert!(near.contains(&0));
        assert!(near.contains(&1));
        assert!(!near.contains(&2));
    }

    #[test]
    fn vertical_segment() {
        let buildings = vec![unit_square(0, 0.0, 0.0), unit_square(1, 0.0, 100.0)];
        let grid = EdgeGrid::new(&buildings, 10.0);
        let near = grid.buildings_near_segment(p(5.0, -5.0), p(5.0, 115.0));
        assert_eq!(near, vec![0, 1]);
    }

    #[test]
    fn segment_on_cell_boundary() {
        let buildings = vec![unit_square(0, 0.0, 0.0)];
        let grid = EdgeGrid::new(&buildings, 10.0);
        let near = grid.buildings_near_segment(p(-5.0, 10.0), p(15.0, 10.0));
        assert!(near.contains(&0));
    }

    #[test]
    fn buildings_within_radius() {
        let buildings = vec![unit_square(0, 0.0, 0.0), unit_square(1, 100.0, 100.0)];
        let grid = EdgeGrid::new(&buildings, 10.0);
        let near = grid.buildings_within(p(5.0, 5.0), 20.0);
        assert!(near.contains(&0));
        assert!(!near.contains(&1));
        let all = grid.buildings_within(p(50.0, 50.0), 100.0);
        assert_eq!(all, vec![0, 1]);
    }

    #[test]
    fn query_outside_grid_does_not_panic() {
        let buildings = vec![unit_square(0, 0.0, 0.0)];
        let grid = EdgeGrid::new(&buildings, 10.0);
        grid.buildings_near_segment(p(200.0, 200.0), p(300.0, 300.0));
        grid.buildings_near_segment(p(-50.0, 5.0), p(50.0, 5.0));
    }

    #[test]
    fn empty_instance() {
        let buildings: Vec<Building> = vec![];
        let grid = EdgeGrid::with_auto_cell_size(&buildings);
        assert!(
            grid.buildings_near_segment(p(0.0, 0.0), p(10.0, 10.0))
                .is_empty()
        );
    }

    #[test]
    fn auto_cell_size_smoke() {
        let buildings = vec![unit_square(0, 0.0, 0.0), unit_square(1, 50.0, 50.0)];
        let grid = EdgeGrid::with_auto_cell_size(&buildings);
        let near = grid.buildings_near_segment(p(-5.0, 5.0), p(60.0, 5.0));
        assert!(near.contains(&0));
    }
}
