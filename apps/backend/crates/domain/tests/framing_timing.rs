//! Review item F3 of rust-domain-3, timing half: the two toothed floors the review named (60 m
//! with 404 vertices, 199 m with 2,004 vertices; the pre-F3 scan took 5.5 s and 317 s for them in a
//! release build) are framed well under a second, even in a debug build. Their results are
//! checked against Python in `tests/framing_work.rs`.
//!
//! This file is its own test binary, so no other test of this crate runs beside it. The shared
//! machine can be loaded by other builds, so each floor is timed three times and the fastest run
//! counts.

mod common;

use std::time::{Duration, Instant};

use ss_core::Contract;
use ss_domain::contracts::geometry::SiteGeometry;
use ss_domain::design::geometry::framing::{frame_for_solver, MIN_FRAME_AREA};

#[test]
fn toothed_floors_frame_well_under_a_second() {
    let doc = common::golden("domain/part4_framing.json");
    for name in ["toothed_bottom_60m_404v", "toothed_bottom_199m_2004v"] {
        let case = doc["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .expect(name);
        let site = SiteGeometry::from_json_value(case["site"].clone()).unwrap();
        let best = (0..3)
            .map(|_| {
                let t = Instant::now();
                assert!(frame_for_solver(&site, MIN_FRAME_AREA).is_some());
                t.elapsed()
            })
            .min()
            .unwrap();
        eprintln!("{name}: frame_for_solver in {best:?} (fastest of 3)");
        assert!(best < Duration::from_millis(1000), "{name} took {best:?}");
    }
}
