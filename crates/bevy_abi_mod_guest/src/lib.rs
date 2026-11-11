#![forbid(unsafe_code)]
#![deny(improper_ctypes_definitions)]

//! Safe, zero-Bevy Rust SDK for authoring `bevy_abi_mod` guest `.cdylib` modules.
//!
//! Mod dynamic libraries compile independently of the host game engine. Depending solely on
//! `bevy_abi_mod_guest_sys` and `#![no_std]` [`bevy_abi_mod_sys`] allows guest mods to communicate
//! with the host exclusively over `extern "C"` without shared Rust ABI or allocator assumptions.
//!
//! All `unsafe` FFI operations are isolated inside `bevy_abi_mod_guest_sys`; this crate enforces
//! `#![forbid(unsafe_code)]`.
//!
//! ## Example
//!
//! ```no_run
//! use bevy_abi_mod_guest::prelude::*;
//!
//! fn setup(app: &mut GuestApp<'_>) -> FfiResult {
//!     let queries = [
//!         ComponentAccessDesc::write(c"Position".as_ptr()),
//!         ComponentAccessDesc::read(c"Velocity".as_ptr()),
//!     ];
//!     app.add_system(c"movement_system", &queries, |ctx| {
//!         while let Some(mut chunk) = ctx.iter().next_chunk_view() {
//!             let Some((vel, pos)) = chunk.read_write::<[f32; 3], [f32; 3]>(1, 0) else {
//!                 return FfiResult::INTERNAL_ERROR;
//!             };
//!             for (p, v) in pos.iter_mut().zip(vel.iter()) {
//!                 p[0] += v[0];
//!                 p[1] += v[1];
//!                 p[2] += v[2];
//!             }
//!         }
//!         FfiResult::OK
//!     })
//! }
//!
//! export_mod!(setup);
//! ```

pub mod app;
pub mod commands;
pub mod component;
pub mod iter;
pub mod lifecycle;
mod macros;
pub mod prelude;
pub mod raw;

#[doc(hidden)]
pub use bevy_abi_mod_guest_sys as sys;
pub use bevy_abi_mod_sys;

#[cfg(feature = "derive")]
pub use bevy_abi_mod_macros::AbiPod;
pub use bevy_abi_mod_sys::AbiPod;

pub use self::app::{GuestApp, MultiQueryContext, SystemContext};
pub use self::commands::GuestCommandQueue;
pub use self::component::GuestComponent;
pub use self::iter::{Chunk, ChunkSlice, ChunkSliceMut, GuestChunkIter};
pub use self::lifecycle::{GuestPlugin, run_validated_guest_app};
pub use self::raw::{
    ChunkSliceMutRawExt, ChunkSliceRawExt, CommandBridgeVTable, CommandBridgeVTableExt,
    GuestAppInitExt, GuestAppRawExt, GuestChunkIterRawExt, GuestCommandQueueRawExt,
    MAX_GUEST_MULTI_QUERIES, ResourceBridgeVTable, catch_guest_panic,
    cleanup_registered_guest_state, register_runtime_component, resolve_fn, resolve_iter_reset,
    resolve_register_conditional_multi_query, resolve_register_conditional_system,
    resolve_register_multi_query, wrap_multi_query_callback, wrap_run_condition,
    wrap_system_callback,
};

pub use bevy_abi_mod_sys::{
    ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH, CmdDespawnFn,
    CmdInsertComponentFn, CmdInsertDefaultFn, CmdRemoveComponentFn, CmdSpawnEmptyFn,
    ComponentAccessDesc, ComponentAccessMode, ComponentDropFn, ComponentUuid,
    ConditionalMultiQuerySystemDescriptor, ConditionalSystemDescriptor, DynamicComponentDescriptor,
    DynamicSystemCallback, ExposableComponent, ExtensibleStruct, FFI_BOOL_FALSE, FFI_BOOL_TRUE,
    FfiBool, FfiEntity, FfiLayout, FfiResult, FfiSlice, GetProcAddressFn, HostApi,
    IterColumnMutableFn, IterColumnReadonlyFn, IterColumnStrideFn, IterEntitiesFn,
    IterMarkChangedFn, IterNextChunkFn, IterResetFn, IterResourceMutableFn, IterResourceReadonlyFn,
    ModAbiVersionFn, ModComponentDescriptor, ModInitFn, ModShutdownFn, MultiQuerySystemCallback,
    MultiQuerySystemDescriptor, MultiQuerySystemDescriptorV2, MultiQuerySystemDescriptorV3,
    QuerySpec, RawChunkIter, RawCommandQueue, RegisterComponentFn,
    RegisterConditionalMultiQuerySystemFn, RegisterConditionalSystemFn, RegisterMultiQuerySystemFn,
    RegisterSystemFn, RunConditionCallback, ScheduleLabelId, ScheduledMultiQuerySystemDescriptor,
    ScheduledSystemDescriptor, StrView, SymbolEntry, SystemDescriptor, SystemDescriptorV2,
    SystemDescriptorV3, ffi_bool_to_rust, rust_bool_to_ffi,
};
