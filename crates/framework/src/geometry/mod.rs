mod freemesh;
mod grid;
mod perimeter;
mod predicates;
mod visibility;

pub use freemesh::{FreeMesh, LitWall, Start, TargetSet, VisibleRegion};
pub use grid::{EdgeGrid, bbox};
pub use perimeter::{
    BOUNDARY_NUDGE, Boundary, SPATIAL_TOLERANCE, boundary_points, boundary_points_convex,
    outward_normal, perimeter_samples, signed_area2,
};
pub use predicates::{
    Orientation, PointLocation, is_reflex_vertex, line_segment_intersection, locate_point,
    on_segment, orient, project_onto_segment, ring_edges, segments_properly_cross,
};
pub use visibility::{is_visible, segment_crosses_interior};
