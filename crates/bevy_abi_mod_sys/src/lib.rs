#![no_std]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

//! # `bevy_abi_mod_sys`: Zero-Dependency C-ABI Contract for `bevy_abi_mod`
//!
//! This crate defines the foundational `#![no_std]`, zero-dependency C-ABI types, function
//! pointers, opaque handles, and handshake protocols connecting Bevy ECS host applications to
//! native dynamic guest mods (`.so`, `.dylib`, `.dll`).
//!
//! ## Core Architectural Principles
//!
//! ### 1. Zero Shared Rust ABI Assumptions
//! Rust does not have a stable ABI across compiler versions, build profiles (`debug` vs `release`),
//! or target flags. Passing complex Rust types (`bevy_ecs::world::World`, `bevy_ecs::system::Query`,
//! `Box<T>`, `Vec<T>`, `String`, or fat slice references `&[T]`) across a shared library boundary
//! leads to undefined behavior if the host and guest were compiled with differing compilers or flags.
//!
//! In `bevy_abi_mod`, the host and guest share **zero Rust types or allocator state**:
//! - All data exchanged across the boundary is `#[repr(C)]` or `#[repr(transparent)]`.
//! - Function pointers are strictly `unsafe extern "C" fn`.
//! - Arrays are passed as `FfiSlice<T>` (`{ ptr: *const T, len: usize }`) instead of Rust fat pointers.
//! - String views are passed as `StrView` (`FfiSlice<u8>`).
//!
//! ### 2. Memory & Allocator Isolation
//! - Memory allocated on the host must be freed by the host.
//! - Memory allocated on the guest must be freed by the guest.
//! - Passing owned heap collections (`Box`, `Vec`, `String`) across the boundary is strictly prohibited.
//! - When guest code inserts components via deferred commands, the host copies the raw bytes into
//!   host-managed memory before scheduling insertion, guaranteeing that unloading the guest `.cdylib`
//!   cannot cause use-after-free or memory corruption on the host.
//!
//! ### 3. Transparent Newtypes vs. Rust Enums
//! Rust enums have strict discriminant ranges. If foreign C, C++, or Zig code returns an unexpected
//! integer value into a Rust `enum`, it instantly creates undefined behavior.
//! To ensure sound FFI:
//! - Status codes ([`FfiResult`]) and access modes ([`ComponentAccessMode`]) are declared as
//!   `#[repr(transparent)] struct Type(pub u32)`.
//! - Any 32-bit bit pattern is sound in Rust, allowing graceful error detection without crashes.
//!
//! ### 4. Calling Conventions & Unwinding
//! Unwinding a panic across an `extern "C"` boundary is undefined behavior in Rust and aborts
//! the process. All function pointers across the ABI are `unsafe extern "C" fn`.
//! Guest libraries are expected to contain panics within their own runtime via `catch_unwind` and
//! return [`FfiResult::PANIC_CAUGHT`] by value.
//!
//! ## Subsystem Map
//! 1. **Versioning**: [`ABI_VERSION_MAJOR`], [`ABI_VERSION_MINOR`], [`ABI_VERSION_PATCH`],
//!    [`ABI_VERSION_PACKED`], [`pack_abi_version`], [`abi_version_is_compatible`].
//! 2. **Core Primitives**: [`FfiResult`], [`FfiBool`], [`FfiEntity`], [`FfiSlice`], [`StrView`], [`ComponentUuid`].
//! 3. **Query & Schedule Declarations**: [`ComponentAccessMode`], [`ComponentAccessDesc`], [`ScheduleLabelId`].
//! 4. **Archetype Traversal Handles**: [`RawChunkIter`], [`IterNextChunkFn`], [`IterColumnReadonlyFn`],
//!    [`IterColumnMutableFn`], [`IterMarkChangedFn`], [`IterEntitiesFn`], [`IterColumnStrideFn`],
//!    [`IterResetFn`], [`IterResourceReadonlyFn`], [`IterResourceMutableFn`].
//! 5. **Deferred Command Handles**: [`RawCommandQueue`], [`CmdSpawnEmptyFn`], [`CmdDespawnFn`],
//!    [`CmdInsertComponentFn`], [`CmdRemoveComponentFn`], [`CmdInsertDefaultFn`].
//! 6. **System Registration & Extensible Descriptors**: [`DynamicSystemCallback`], [`RunConditionCallback`],
//!    [`QuerySpec`], [`MultiQuerySystemCallback`], [`MultiQuerySystemDescriptor`],
//!    [`ScheduledMultiQuerySystemDescriptor`], [`ConditionalMultiQuerySystemDescriptor`],
//!    [`SystemDescriptor`], [`ScheduledSystemDescriptor`], [`ConditionalSystemDescriptor`],
//!    [`HostApi`], [`ExtensibleStruct`], [`read_extensible_struct`], [`read_descriptor`],
//!    [`RegisterSystemFn`], [`RegisterScheduledSystemFn`], [`RegisterConditionalSystemFn`],
//!    [`RegisterMultiQuerySystemFn`], [`RegisterConditionalMultiQuerySystemFn`], [`GetProcAddressFn`].
//! 7. **Dynamic Components & POD Traits**: [`ComponentDropFn`], [`ModComponentDescriptor`],
//!    [`DynamicComponentDescriptor`], [`RegisterComponentFn`], [`FfiLayout`], [`AbiPod`],
//!    [`SymbolEntry`], [`ExposableComponent`].

mod access;
mod commands;
mod component;
mod extensible;
mod host_api;
mod iter;
mod primitives;
mod system;
mod traits;
mod version;

#[cfg(test)]
mod tests;

pub use access::{ComponentAccessDesc, ComponentAccessMode, ScheduleLabelId};
pub use commands::{
    CmdDespawnFn, CmdInsertComponentFn, CmdInsertDefaultFn, CmdRemoveComponentFn, CmdSpawnEmptyFn,
    RawCommandQueue,
};
pub use component::{
    ComponentDropFn, DynamicComponentDescriptor, ModComponentDescriptor, RegisterComponentFn,
};
pub use extensible::{ExtensibleStruct, read_descriptor, read_extensible_struct};
pub use host_api::{GetProcAddressFn, HostApi, ModAbiVersionFn, ModInitFn, ModShutdownFn};
pub use iter::{
    IterColumnMutableFn, IterColumnReadonlyFn, IterColumnStrideFn, IterEntitiesFn,
    IterMarkChangedFn, IterNextChunkFn, IterResetFn, IterResourceMutableFn, IterResourceReadonlyFn,
    RawChunkIter,
};
pub use primitives::{
    ComponentUuid, FFI_BOOL_FALSE, FFI_BOOL_TRUE, FfiBool, FfiEntity, FfiResult, FfiSlice, StrView,
    ffi_bool_to_rust, rust_bool_to_ffi,
};
pub use system::{
    ConditionalMultiQuerySystemDescriptor, ConditionalSystemDescriptor, DynamicSystemCallback,
    MultiQuerySystemCallback, MultiQuerySystemDescriptor, MultiQuerySystemDescriptorV2,
    MultiQuerySystemDescriptorV3, QuerySpec, RegisterConditionalMultiQuerySystemFn,
    RegisterConditionalSystemFn, RegisterMultiQuerySystemFn, RegisterScheduledSystemFn,
    RegisterSystemFn, RunConditionCallback, ScheduledMultiQuerySystemDescriptor,
    ScheduledSystemDescriptor, SystemDescriptor, SystemDescriptorV2, SystemDescriptorV3,
};
pub use traits::{AbiPod, ExposableComponent, FfiLayout, SymbolEntry};
pub use version::{
    ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH,
    abi_version_is_compatible, abi_version_major, abi_version_minor, abi_version_patch,
    pack_abi_version,
};
