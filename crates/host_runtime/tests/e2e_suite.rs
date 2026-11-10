#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]
#![allow(clippy::all)]

//! # Master E2E Test Suite for `bevy_abi_mod`
//!
//! Enforces 4-tier opaque-box test coverage across all 26 features
//! defined in `PROJECT.md` and `TEST_INFRA.md`.
//!
//! - Tier 1: Feature Isolation (Happy Path, 130 tests)
//! - Tier 2: Boundary & Corner Cases (130 tests)
//! - Tier 3: Cross-Feature Pairwise Interactions (10 tests)
//! - Tier 4: Real-World Application Scenarios (5 comprehensive game scenarios)

#[path = "e2e_suite/harness.rs"]
pub mod harness;

#[path = "e2e_suite/tier1_features.rs"]
pub mod tier1_features;

#[path = "e2e_suite/tier2_boundaries.rs"]
pub mod tier2_boundaries;

#[path = "e2e_suite/tier3_pairwise.rs"]
pub mod tier3_pairwise;

#[path = "e2e_suite/tier4_scenarios.rs"]
pub mod tier4_scenarios;
