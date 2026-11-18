#![allow(
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    unreachable_pub
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! Consolidated host integration test binary 3 of 4: **Mod Lifecycle & Safety** (`ABI 0.2.0`).
//!
//! Covers dynamic library loading/unloading, ABI SemVer handshake, `LoadTransaction` rollback,
//! `LoadContext` / `HandleHeader` active-scope guards, guest VTable/symbol eviction,
//! `WorldDropGuard` drop-trampoline reclamation, `panic = "abort"` subprocess termination,
//! per-World `BamInstance` isolation, schedule system-set excision, and hot-reload stress.

#[path = "../support/helpers.rs"]
mod support;

use std::sync::RwLock;

/// Coordinates read-shared vs. full-1024-slot-saturation drop-trampoline tests under parallel
/// `cargo test` execution without any `reset_drop_trampolines_for_testing` hook.
pub(crate) static DROP_SLOT_POOL_RWLOCK: RwLock<()> = RwLock::new(());

mod drop_trampolines;
mod handshake_and_strict_load;
mod hot_reload_and_excision;
mod isolation_and_rollback;
mod mod_dag_and_cascade;
mod scoped_cleanup_and_slab;
#[cfg(feature = "signature-verification")]
mod signature_verification;
mod state_hooks_and_observers;
