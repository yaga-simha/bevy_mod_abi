#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    clippy::default_constructed_unit_structs,
    unreachable_pub
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]
#![allow(clippy::all)]

//! End-to-End Integration Test Binary (`e2e`):
//! - 4-tier feature, boundary, pairwise, and game-scenario suites
//! - Asymmetric Rust & ISO C99 shared-library integration and hot-reload suites
//! - Progression example guest-mod pipelines (Examples 01..04)

#[path = "../support/helpers.rs"]
mod support;

pub mod asymmetric_mods;
pub mod example_pipelines;
pub mod harness;
pub mod tier1_features;
pub mod tier2_boundaries;
pub mod tier3_pairwise;
pub mod tier4_scenarios;
