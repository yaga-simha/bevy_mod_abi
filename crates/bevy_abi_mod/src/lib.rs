#![forbid(unsafe_code)]

//! Zero-serialization, asymmetric C-ABI (`ABI 0.2.0`) native modding framework for Bevy ECS (`v0.20+`).
//!
//! `bevy_abi_mod` is the top-level host facade crate. It re-exports [`bevy_abi_mod_host`],
//! [`ModComponent`], [`AbiPod`], [`bevy_abi_mod_sys`], and [`prelude`], while enforcing
//! `#![forbid(unsafe_code)]` across all public host APIs.
//!
//! # Purpose & Design Philosophy
//!
//! Dynamically loading native shared libraries (`.so`, `.dylib`, `.dll`) into a Bevy application
//! at runtime requires navigating three systems constraints:
//!
//! 1. **Zero Shared Rust ABI Assumptions:** Rust provides no stable cross-compiler ABI.
//!    `bevy_abi_mod` communicates strictly over an `extern "C"` boundary (`ABI 0.2.0`), allowing
//!    guest mods to be compiled with different `rustc` versions or written in ISO C99, C++17/20,
//!    or Zig.
//! 2. **Zero Serialization Overhead:** Host ECS `Table` columns are exposed directly as
//!    contiguous Structure-of-Arrays (SoA) byte slices (and `SparseSet` columns as batched
//!    per-row pointer arrays) without JSON, MessagePack, or Bincode serialization.
//! 3. **Allocator & Lifecycle Isolation:** Host and guest dynamic libraries link independent
//!    heap allocators. Memory allocated on one side is never freed by the other, and each
//!    `World` owns isolated mod instances with deterministic unloading.
//!
//! ---
//!
//! # Host Integration Guide (Crate Consumers)
//!
//! ## 1. Add `AbiModPlugin` and Expose Components
//!
//! Derive [`ModComponent`] on `#[repr(C)]` Bevy components (or [`AbiPod`] on plain-old-data
//! types), register them on your `App` via [`ModAppExt`], and add [`AbiModPlugin`]:
//!
//! ```ignore
//! use bevy_abi_mod::prelude::*;
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//!
//! #[derive(Component, ModComponent, Clone, Copy)]
//! #[repr(C)]
//! struct Health {
//!     current: f32,
//!     max: f32,
//! }
//!
//! fn main() {
//!     App::new()
//!         .add_plugins(AbiModPlugin::default())
//!         .register_mod_component::<Health>()
//!         .run();
//! }
//! ```
//!
//! ## 2. Load and Unload Native Mods
//!
//! Mods can be loaded from disk using [`ModWorldExt::load_mod`] (or [`ModCommandsExt`]) with
//! [`LoadOptions`], and unloaded cleanly via [`ModWorldExt::unload_mod`]. Lifecycle events such
//! as [`ModLoaded`], [`ModUnloaded`], and [`ModSystemFaulted`] are emitted as Bevy messages.
//!
//! ```ignore
//! use bevy_abi_mod::prelude::*;
//! use bevy_ecs::prelude::*;
//! use std::path::Path;
//!
//! fn load_gameplay_mod(world: &mut World) {
//!     let mod_id = world
//!         .load_mod(Path::new("mods/gameplay_mod.dylib"), LoadOptions::default())
//!         .expect("failed to load native mod");
//!
//!     // Later, when hot-reloading or shutting down:
//!     world.unload_mod(mod_id).expect("failed to unload mod");
//! }
//! ```
//!
//! ## 3. Guest Mod Usage (Separate `cdylib` Crate)
//!
//! Guest mods depend on `bevy_abi_mod_guest` (which has **no** dependency on `bevy`) and
//! export their entry points with `export_mod!`:
//!
//! ```ignore
//! use bevy_abi_mod_guest::prelude::*;
//!
//! #[derive(GuestComponent, AbiPod, Clone, Copy)]
//! #[repr(C)]
//! struct Health {
//!     current: f32,
//!     max: f32,
//! }
//!
//! struct RegenerateMod;
//!
//! impl GuestPlugin for RegenerateMod {
//!     fn build(&self, app: &mut GuestApp) {
//!         let health = app.component::<Health>();
//!         app.system("regenerate", move |mut ctx| {
//!             for mut chunk in ctx.query().write(health).iter() {
//!                 for h in chunk.write_slice(health) {
//!                     h.current = (h.current + 0.1).min(h.max);
//!                 }
//!             }
//!         });
//!     }
//! }
//!
//! export_mod!(RegenerateMod);
//! ```
//!
//! ---
//!
//! # Architecture & Contributor Guide (Crate Developers)
//!
//! ## Seven-Crate `-sys` vs. Safe Rust Split
//!
//! The workspace strictly isolates `unsafe` FFI machinery inside `*-sys` crates while keeping
//! high-level host and guest crates under `#![forbid(unsafe_code)]`, and keeping `bevy*` out of
//! all wire, macro, and guest crates:
//!
//! - `bevy_abi_mod_sys`: `#![no_std]`, zero external dependencies. Defines the `#[repr(C)]`
//!   `ABI 0.2.0` wire contract (`BamHostApi`, `BamManifest`, `BamSystemDesc`, `ComponentDesc`,
//!   `FfiResult`, `StrView`, `ModComponent`, `AbiPod`).
//! - `bevy_abi_mod_macros`: Proc-macro crate (`#[derive(ModComponent, AbiPod)]`). No `bevy`
//!   dependency.
//! - `bevy_abi_mod_host_sys`: Isolates all host `unsafe` (`HOST_API`, `HostIter`, `HostCommands`,
//!   `GuestSystem`, `ModRegistry`, `libloading` loader).
//! - `bevy_abi_mod_host`: `#![forbid(unsafe_code)]` safe Bevy plugin, extension traits, and
//!   lifecycle messages.
//! - `bevy_abi_mod`: `#![forbid(unsafe_code)]` top-level host facade (this crate).
//! - `bevy_abi_mod_guest_sys`: Isolates all guest `unsafe` (`extern "C"` trampolines, panic
//!   guards, host API cache). No `bevy` dependency.
//! - `bevy_abi_mod_guest`: `#![forbid(unsafe_code)]` ergonomic Rust guest SDK. No `bevy`
//!   dependency.
//!
//! ## Core Invariants
//!
//! - **ABI Purity:** Every wire struct in `bevy_abi_mod_sys` is `#[repr(C)]` or
//!   `#[repr(transparent)]`, every callback is `extern "C" fn`, and strings are passed as
//!   [`bevy_abi_mod_sys::StrView`] (never Rust `&str`, `&[T]`, or `dyn Trait` fat pointers).
//! - **Memory & Allocator Isolation:** Host-allocated memory is freed by the host;
//!   guest-allocated memory is freed by the guest. Never pass `Box`, `Vec`, or `String` across
//!   the ABI boundary.
//! - **Dynamic System Concurrency Safety:** Registered guest systems declare all component
//!   read/write accesses in Bevy's `FilteredAccessSet` using pre-resolved `ComponentId`s,
//!   allowing Bevy's multi-threaded scheduler to enforce borrow rules and run non-conflicting
//!   mod and host systems in parallel.
//!
//! # Feature Flags
//!
//! - `strict-ffi-checks`: Enables additional runtime FFI layout and pointer validation in
//!   `bevy_abi_mod_host`.
//! - `diagnostics`: Enables optional diagnostic instrumentation in `bevy_abi_mod_host`.

pub use bevy_abi_mod_host;
pub use bevy_abi_mod_host::*;
pub use bevy_abi_mod_macros;
