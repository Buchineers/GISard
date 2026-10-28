pub mod evaluation;
pub mod geometry;
pub mod intervals;
pub mod io;
pub mod pipeline;
pub mod test_utils;
pub mod types;

pub use evaluation::{AntennaView, CoverageGoals, Evaluation, Evaluator, evaluate};
pub use geometry::{
    BOUNDARY_NUDGE, Boundary, EdgeGrid, FreeMesh, LitWall, Orientation, PointLocation,
    SPATIAL_TOLERANCE, Start, TargetSet, VisibleRegion, bbox, boundary_points,
    boundary_points_convex, is_reflex_vertex, is_visible, line_segment_intersection, locate_point,
    on_segment, orient, outward_normal, perimeter_samples, project_onto_segment, ring_edges,
    segment_crosses_interior, segments_properly_cross, signed_area2,
};
pub use intervals::IntervalSet;
pub use io::{parse_antennas, parse_instance, parse_instance_stdin};
pub use pipeline::{
    Pipeline, PipelineEvent, PipelineObserver, PipelineStep, Reconstructor, Solver,
};
pub use types::{Building, Instance, Point, Solution};
