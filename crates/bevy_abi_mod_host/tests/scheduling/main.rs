//! Consolidated Scheduling, Concurrency, Multi-Query & Differential Parity Suite (`scheduling`, `ABI 0.2.0`).
//!
//! Covers parallel disjoint writes, borrow/resource conflict detection, fault isolation &
//! `ModQuarantine::reenable`, single-path `Messages<ModSystemFaulted>`, unified `BamSystemDesc`
//! multi-query scoping, `run_if` condition callbacks, custom `ScheduleId >= 1024`,
//! `BAM_SYSTEM_FLAG_MAIN_THREAD`, `ModNamedSystemSet` ordering, runtime `Startup` execution,
//! and `proptest!` native-vs-dynamic differential Bevy ECS parity.

#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    unreachable_pub
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

#[path = "../support/helpers.rs"]
mod support;

mod conflicts_and_faults;
mod differential_parity;
mod multi_query_and_conditions;
mod ordering_and_validation;
mod sets_and_diagnostics;
