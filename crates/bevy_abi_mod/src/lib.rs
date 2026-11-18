#![forbid(unsafe_code)]
#![warn(missing_docs)]

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
//!    contiguous Structure-of-Arrays (SoA) byte slices, `SparseSet` columns as batched per-row
//!    pointer arrays, and singleton resources with change-detection ticks without JSON, MessagePack,
//!    or Bincode serialization.
//! 3. **Allocator & Relational Integrity:** Host and guest dynamic libraries link independent heap
//!    allocators. Memory allocated on one side is never freed by the other, and all deferred guest
//!    spawns, inserts, and removals execute through Bevy `EntityWorldMut` paths so
//!    `ComponentHooks`, `#[require(...)]` Required Components, and `ChildOf` / `Children`
//!    relationships fire accurately.
//!
//! ---
//!
//! # Host Integration Guide (Crate Consumers)
//!
//! ## 1. Add `AbiModPlugin` and Expose Components, Resources & Sets
//!
//! Derive [`ModComponent`] on `#[repr(C)]` Bevy components (or [`AbiPod`] on plain-old-data
//! types), register them on your `App` via [`ModAppExt`], expose host `SystemSet`s via
//! [`ModAppExt::register_mod_visible_set`], and add [`AbiModPlugin`]:
//!
//! ```no_run
//! use bevy_abi_mod::prelude::*;
//! use bevy_app::App;
//! use bevy_ecs::component::Component;
//!
//! #[repr(C)]
//! #[derive(Component, ModComponent, Clone, Copy, Default)]
//! #[mod_component(name = "my_game::Health")]
//! struct Health {
//!     current: f32,
//!     max: f32,
//! }
//!
//! fn main() {
//!     let mut app = App::new();
//!     app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
//!     app.register_mod_component::<Health>(
//!         ComponentOptions::new().with_raw_insert().with_default(),
//!     )
//!     .expect("register Health");
//!     app.load_mod_with("mods/libcombat_mod.dylib", LoadOptions::default())
//!         .expect("load mod");
//!     app.run();
//! }
//! ```
//!
//! ## 2. Load, Hot-Reload, and Unload Native Mods
//!
//! Mods can be loaded from disk using [`ModWorldExt::load_mod_with`] (or [`ModCommandsExt`]) with
//! [`LoadOptions`], hot-reloaded with state preservation via [`ModWorldExt::reload_mod`], and
//! unloaded cleanly via [`ModWorldExt::unload_mod`]. Lifecycle events such as [`ModLoaded`],
//! [`ModUnloaded`], and [`ModSystemFaulted`] are emitted as Bevy messages.
//!
//! ```no_run
//! use bevy_abi_mod::prelude::*;
//! use bevy_ecs::world::World;
//!
//! fn load_gameplay_mod(world: &mut World) {
//!     let mod_id = world
//!         .load_mod_with("mods/libgameplay_mod.dylib", LoadOptions::default())
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
//! export their entry points with `export_mod!(plugin: MyMod)`:
//!
//! ```ignore
//! use bevy_abi_mod_guest::prelude::*;
//! use core::ffi::CStr;
//!
//! #[repr(C)]
//! #[derive(Clone, Copy, AbiPod)]
//! struct Health {
//!     current: f32,
//!     max: f32,
//! }
//!
//! impl GuestComponent for Health {
//!     const NAME: &'static CStr = c"my_game::Health";
//! }
//!
//! #[derive(Default)]
//! struct RegenerateMod;
//!
//! impl GuestPlugin for RegenerateMod {
//!     const NAME: &'static CStr = c"regenerate_mod";
//!
//!     fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
//!         app.system(c"regenerate")
//!             .query(|q| q.write::<Health>())
//!             .schedule(ScheduleId::UPDATE)
//!             .build(|ctx| {
//!                 while let Some(mut chunk) = ctx.iter().next_chunk_view() {
//!                     if let Some(healths) = chunk.component_mut::<Health>(0) {
//!                         for h in healths {
//!                             h.current = (h.current + 0.1).min(h.max);
//!                         }
//!                     }
//!                 }
//!             })
//!     }
//! }
//!
//! export_mod!(plugin: RegenerateMod);
//! ```
//!
//! ---
//!
//! # Architecture & Contributor Guide (Crate Developers)
//!
//! ## Eight-Crate `-sys` vs. Safe Rust Split
//!
//! The workspace strictly isolates `unsafe` FFI machinery inside `*-sys` crates while keeping
//! high-level host, guest, macro, and CLI crates under `#![forbid(unsafe_code)]`, and keeping
//! `bevy*` out of all wire, macro, guest, and CLI crates:
//!
//! - `bevy_abi_mod_sys`: `#![no_std]`, zero external dependencies. Defines the `#[repr(C)]`
//!   `ABI 0.2.0` wire contract (`BamHostApi`, `BamManifest`, `BamSystemDesc`, `ComponentDesc`,
//!   `FfiResult`, `StrView`, `ModComponent`, `AbiPod`).
//! - `bevy_abi_mod_macros`: `#![forbid(unsafe_code)]` proc-macro crate (`#[derive(ModComponent, AbiPod)]`). No `bevy`
//!   dependency.
//! - `bevy_abi_mod_host_sys`: Isolates all host `unsafe` (`HOST_API`, `HostIter`, `HostCommands`,
//!   `GuestSystem`, `ModRegistry`, `libloading` loader).
//! - `bevy_abi_mod_host`: `#![forbid(unsafe_code)]` safe Bevy plugin, optional Bevy `0.20` bridges,
//!   `SubApp` extraction, extension traits, and lifecycle messages.
//! - `bevy_abi_mod`: `#![forbid(unsafe_code)]` top-level host facade (this crate).
//! - `bevy_abi_mod_guest_sys`: Isolates all guest `unsafe` (`extern "C"` trampolines, panic
//!   guards, host API cache). No `bevy` dependency.
//! - `bevy_abi_mod_guest`: `#![forbid(unsafe_code)]` ergonomic Rust guest SDK. No `bevy`
//!   dependency.
//! - `cargo-bam`: `#![forbid(unsafe_code)]` standalone modder CLI (`new`, `build`, `header`,
//!   `check-mods`, `doctor`, `sign`). No `bevy` dependency.
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
//! - `signature-verification`: Enables optional pure-Rust OpenSSH `SSHSIG` and Ed25519 CA
//!   certificate verification (`ModTrustPolicy`) prior to mapping native shared libraries.
//! - `bridge-std-types`: Enables Bevy standard component/resource/input bridges (`StdTypesBridgePlugin`).
//! - `bridge-asset`: Enables isolated `mod://<namespace>/...` Bevy asset sourcing (`ModAssetPlugin`).
//! - `bridge-state`: Enables Bevy `States` exposure (`ModStateExt`).
//! - `bridge-reflect`: Enables dynamic `bevy_reflect`, `bevy_remote` BRP, and `bevy_settings` bridges.
//! - `bridge-bsn`: Enables Bevy Scene Notation (`bevy_scene`) scene spawning (`cmd_spawn_scene`).
//! - `sub-app-extract`: Enables `SubApp` (`RenderApp`) extraction and custom shader/material support.
//! - `hot-reload-watcher`: Enables debounced filesystem hot-reload watching (`ModHotReloadPlugin`).

pub use bevy_abi_mod_host;
pub use bevy_abi_mod_host::*;
pub use bevy_abi_mod_macros;
