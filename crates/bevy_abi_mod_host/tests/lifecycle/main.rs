#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    unreachable_pub
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! Consolidated host integration test binary 3 of 4: **Mod Lifecycle & Safety**.
//!
//! Covers dynamic library loading/unloading, ABI SemVer handshake, `LoadTransaction` rollback,
//! `ModInitContext` active-scope guards, guest VTable/symbol eviction, drop-trampoline pool
//! conservation/isolation, schedule system-set excision, and hot-reload stress.

#[path = "../support/helpers.rs"]
mod support;

mod drop_trampolines;
mod handshake_and_strict_load;
mod hot_reload_and_excision;
mod regressions;
