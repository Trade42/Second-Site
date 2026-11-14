//! Port of `apps/api/app/domains/design/geometry/`: framing of measured floors and the layout
//! solver.

pub mod framing;
pub mod solver;

pub use framing::{
    dominant_angle, frame_for_solver, largest_inner_rectangle, normalize_plan, Frame, FrameMap,
    Side,
};
pub use solver::{generate_candidates, Candidate, SolveReport, SolveStatus, Topology};
