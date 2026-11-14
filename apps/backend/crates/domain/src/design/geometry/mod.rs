//! Port of `apps/api/app/domains/design/geometry/`: framing of measured floors and the layout
//! solver.

pub mod framing;
pub mod solver;

pub use framing::{
    dominant_angle, frame_for_solver, frame_for_solver_checked, largest_inner_rectangle,
    largest_inner_rectangle_checked, normalize_plan, Frame, FrameMap, Side, WorkBoundExceeded,
};
pub use solver::{generate_candidates, Candidate, SolveReport, SolveStatus, Topology};
