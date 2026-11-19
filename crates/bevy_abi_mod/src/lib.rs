#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Zero-serialization, asymmetric C-ABI (`ABI 0.3.0`) native modding framework for Bevy ECS (`v0.20+`).
//!
//! `bevy_abi_mod` is the top-level host facade crate. It re-exports [`bevy_abi_mod_host`],
//! [`ModComponent`], [`AbiPod`], [`bevy_abi_mod_sys`], optional companion crates
//! (`bevy_abi_mod_bridges`, `bevy_abi_mod_render`, `bevy_abi_mod_reflect`,
//! `bevy_abi_mod_watcher`), and [`prelude`], while enforcing `#![forbid(unsafe_code)]` across all
//! public host APIs.
//!
//! # Purpose & Design Philosophy
//!
//! Dynamically loading native shared libraries (`.so`, `.dylib`, `.dll`) into a Bevy application
//! at runtime requires navigating three systems constraints:
//!
//! 1. **Zero Shared Rust ABI Assumptions:** Rust provides no stable cross-compiler ABI.
//!    `bevy_abi_mod` communicates strictly over an `extern "C"` boundary (`ABI 0.3.0`), allowing
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
//! # Workspace Architecture & Feature Flags
//!
//! The workspace separates `unsafe` FFI machinery inside `*-sys` crates while keeping all public
//! host, bridge, render, reflection, watcher, guest, macro, trust, manifest, SDK-generator, and
//! CLI crates `#![forbid(unsafe_code)]` (and keeping `bevy*` out of all non-host crates).
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

#[cfg(any(
    feature = "bridge-std-types",
    feature = "bridge-asset",
    feature = "bridge-state",
    feature = "bridge-bsn"
))]
pub use bevy_abi_mod_bridges;
pub use bevy_abi_mod_host;
pub use bevy_abi_mod_host::*;
pub use bevy_abi_mod_macros;
#[cfg(feature = "bridge-reflect")]
pub use bevy_abi_mod_reflect;
#[cfg(feature = "sub-app-extract")]
pub use bevy_abi_mod_render;
#[cfg(feature = "hot-reload-watcher")]
pub use bevy_abi_mod_watcher;

/// Re-exported optional Bevy subsystem bridges (`std_types`, `asset`, `state`, `bsn`, and `reflect`).
pub mod bridges {
    #[cfg(all(
        feature = "bridge-std-types",
        feature = "bridge-asset",
        feature = "bridge-state"
    ))]
    pub use bevy_abi_mod_bridges::ModBridgesPlugin;
    #[cfg(feature = "bridge-bsn")]
    pub use bevy_abi_mod_bridges::{
        CANONICAL_READY_NAME, HostSceneRegistry, ModBsnAppExt, ModBsnPlugin, ModBsnWorldExt,
        SCENE_PATCH_ALIAS, bsn,
    };
    #[cfg(feature = "bridge-std-types")]
    pub use bevy_abi_mod_bridges::{
        FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput,
        FfiTimeFixed, FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility,
        StdTypesBridgePlugin, register_std_types_bridges, std_types,
    };
    #[cfg(feature = "bridge-asset")]
    pub use bevy_abi_mod_bridges::{
        ModAssetAppExt, ModAssetPlugin, ModAssetRegistry, ModAssetWorldExt, asset,
    };
    #[cfg(feature = "bridge-state")]
    pub use bevy_abi_mod_bridges::{ModNextState, ModStateError, ModStateExt, ModStateSpec, state};
    #[cfg(feature = "bridge-reflect")]
    pub use bevy_abi_mod_reflect::{
        BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
        BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
        ModReflectAppExt, ModReflectError, ModReflectPlugin, ModReflectRegistry,
        ModReflectRemoteExt, ModReflectWorldExt, ModSettingsError, ModSettingsRegistry,
        TypedReflectBridgeEntry, WitnessReflectSchema, handle_mod_brp_request,
        install_mod_brp_methods, reflect,
    };
}

#[cfg(feature = "sub-app-extract")]
pub use bevy_abi_mod_render::sub_app_materials;

#[cfg(feature = "hot-reload-watcher")]
pub use bevy_abi_mod_watcher::watcher;

#[cfg(all(
    feature = "bridge-std-types",
    feature = "bridge-asset",
    feature = "bridge-state"
))]
pub use bevy_abi_mod_bridges::ModBridgesPlugin;
#[cfg(feature = "bridge-bsn")]
pub use bevy_abi_mod_bridges::{
    CANONICAL_READY_NAME, HostSceneRegistry, ModBsnAppExt, ModBsnPlugin, ModBsnWorldExt,
    SCENE_PATCH_ALIAS,
};
#[cfg(feature = "bridge-std-types")]
pub use bevy_abi_mod_bridges::{
    FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput, FfiTimeFixed,
    FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility, StdTypesBridgePlugin,
    register_std_types_bridges,
};
#[cfg(feature = "bridge-asset")]
pub use bevy_abi_mod_bridges::{
    ModAssetAppExt, ModAssetPlugin, ModAssetRegistry, ModAssetWorldExt,
};
#[cfg(feature = "bridge-state")]
pub use bevy_abi_mod_bridges::{ModNextState, ModStateError, ModStateExt, ModStateSpec};

#[cfg(feature = "bridge-reflect")]
pub use bevy_abi_mod_reflect::{
    BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
    BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
    ModReflectAppExt, ModReflectError, ModReflectPlugin, ModReflectRegistry, ModReflectRemoteExt,
    ModReflectWorldExt, ModSettingsError, ModSettingsRegistry, TypedReflectBridgeEntry,
    WitnessReflectSchema, handle_mod_brp_request, install_mod_brp_methods,
    reflect_entity_component, reflect_entity_component_mut, reflect_resource_component,
    reflect_resource_component_mut,
};

#[cfg(feature = "sub-app-extract")]
pub use bevy_abi_mod_render::{
    ModMaterialAppExt, ModMaterialError, ModMaterialParams, ModMaterialWorldExt,
    ModShaderBufferInit, ModShaderBufferSpec, ModSpriteExtension2d, ModSpriteMaterialEntry,
    ModSpriteMaterialSpec, ModStandardMaterialEntry, ModStandardMaterialSpec, ModSubAppConfig,
    ModSubAppExt, ModSubAppPlugin, ModSubAppSchedule, ModSubAppSystems, ModSubWorldExt,
    ModWeslMaterialPlugin, ModWeslMaterialRegistry, WeslShaderAssetSpec, WeslShaderSource,
    cleanup_mod_wesl_materials_in_world, register_mod_custom_asset_in_world,
    register_mod_extended_sprite_material_in_world, register_mod_shader_buffer_in_world,
    register_mod_sprite_material_in_world, register_mod_standard_material_in_world,
    register_mod_wesl_shader_in_world, update_mod_shader_buffer_in_world,
};

#[cfg(feature = "hot-reload-watcher")]
pub use bevy_abi_mod_watcher::{
    ModHotReloadAction, ModHotReloadHistory, ModHotReloadOutcome, ModHotReloadPlugin,
    ModHotReloadWatcher,
};

/// Convenient unified prelude re-exporting core host APIs plus all enabled bridge, render,
/// reflection, and hot-reload watcher extensions.
pub mod prelude {
    #[cfg(any(
        feature = "bridge-std-types",
        feature = "bridge-asset",
        feature = "bridge-state",
        feature = "bridge-bsn"
    ))]
    pub use bevy_abi_mod_bridges::prelude::*;
    pub use bevy_abi_mod_host::prelude::*;
    #[cfg(feature = "bridge-reflect")]
    pub use bevy_abi_mod_reflect::prelude::*;
    #[cfg(feature = "sub-app-extract")]
    pub use bevy_abi_mod_render::prelude::*;
    #[cfg(feature = "hot-reload-watcher")]
    pub use bevy_abi_mod_watcher::prelude::*;
}
