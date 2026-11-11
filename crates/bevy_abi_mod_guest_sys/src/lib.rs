#![deny(
    improper_ctypes_definitions,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]

//! Low-level guest FFI internals for `bevy_abi_mod`.
//!
//! `bevy_abi_mod_guest_sys` isolates all guest-side `unsafe` C-ABI trampolines,
//! `GetProcAddressFn` symbol resolution, guest-heap allocation registries, raw pointer/stride
//! validation, and raw command/resource dispatchers.
//!
//! This crate has zero Bevy dependencies and depends exclusively on `bevy_abi_mod_sys`.

pub mod alloc;
pub mod commands;
pub mod component;
pub mod iter;
pub mod resolve;
pub mod trampoline;

pub use bevy_abi_mod_sys;
pub use bevy_abi_mod_sys::{
    ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH, AbiPod,
    CmdDespawnFn, CmdInsertComponentFn, CmdInsertDefaultFn, CmdRemoveComponentFn, CmdSpawnEmptyFn,
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

pub use self::alloc::{
    ErasedAllocation, abort_allocations, catch_guest_panic, cleanup_registered_guest_state,
    commit_allocations,
};
pub use self::commands::{
    GuestCommandQueueRawExt, ValidatedCommandQueue, ValidatedCommandQueueAdapter, raw_cmd_despawn,
    raw_cmd_insert_bytes, raw_cmd_insert_default, raw_cmd_insert_default_cstr,
    raw_cmd_insert_slice, raw_cmd_remove_component, raw_cmd_remove_component_cstr,
    raw_cmd_spawn_empty,
};
pub use self::component::register_runtime_component;
pub use self::iter::{
    ChunkSliceMutRawExt, ChunkSliceRawExt, GuestChunkIterRawExt, ValidatedIter,
    ValidatedIterAdapter, ValidatedSlice, ValidatedSliceAdapter, ValidatedSliceMut,
    ValidatedSliceMutAdapter, byte_ranges_overlap, raw_deref_resource, raw_deref_resource_mut,
    raw_iter_column_mutable, raw_iter_column_readonly, raw_iter_column_stride, raw_iter_entities,
    raw_iter_mark_changed, raw_iter_next_chunk, raw_iter_reset, raw_iter_resource_mutable,
    raw_iter_resource_readonly, raw_slice_as_typed, raw_slice_as_typed_mut, raw_slice_get_ptr,
    raw_slice_get_ptr_mut, validated_entities_slice,
};
pub use self::resolve::{
    CommandBridgeVTable, CommandBridgeVTableExt, MIN_GUEST_APP_HOST_API_SIZE,
    MIN_REGISTER_COMPONENT_HOST_API_SIZE, ResourceBridgeVTable, resolve_fn, resolve_iter_reset,
    resolve_register_conditional_multi_query, resolve_register_conditional_system,
    resolve_register_multi_query, resolve_symbol_cstr, resolve_vtable_raw, validate_host_api,
};
pub use self::trampoline::{
    GuestAppInitExt, GuestAppLifecycle, GuestAppRawExt, GuestInitHandles, MAX_GUEST_MULTI_QUERIES,
    ValidatedAppAdapter, ValidatedInit, ValidatedMultiQueryInvocation, ValidatedSystemInvocation,
    register_conditional_multi_query_closure, register_conditional_system_closure,
    register_multi_query_closure, register_system_closure, run_lifecycle_init,
    run_with_validated_init,
};

#[cfg(test)]
#[allow(clippy::undocumented_unsafe_blocks)]
mod tests {
    use super::*;
    use core::ffi::{CStr, c_char, c_void};

    unsafe extern "C" fn stub_register_system(
        _ctx: *mut c_void,
        _desc: *const SystemDescriptor,
    ) -> FfiResult {
        FfiResult::OK
    }

    unsafe extern "C" fn stub_iter_next_chunk(_iter: *mut RawChunkIter) -> usize {
        0
    }

    unsafe extern "C" fn stub_iter_column_readonly(
        _iter: *const RawChunkIter,
        _index: usize,
    ) -> *const u8 {
        core::ptr::null()
    }

    unsafe extern "C" fn stub_iter_column_mutable(
        _iter: *mut RawChunkIter,
        _index: usize,
    ) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe extern "C" fn stub_iter_mark_changed(
        _iter: *mut RawChunkIter,
        _index: usize,
        _row: usize,
    ) -> FfiResult {
        FfiResult::OK
    }

    unsafe extern "C" fn stub_iter_entities(_iter: *const RawChunkIter) -> *const FfiEntity {
        core::ptr::null()
    }

    unsafe extern "C" fn stub_iter_column_stride(
        _iter: *const RawChunkIter,
        _index: usize,
    ) -> usize {
        0
    }

    #[repr(C)]
    struct CoreOnlyHostApiPrefix {
        struct_size: u32,
        abi_version: u32,
        register_system: RegisterSystemFn,
        iter_next_chunk: IterNextChunkFn,
        iter_column_readonly: IterColumnReadonlyFn,
        iter_column_mutable: IterColumnMutableFn,
        iter_mark_changed: IterMarkChangedFn,
        iter_entities: IterEntitiesFn,
        iter_column_stride: IterColumnStrideFn,
    }

    #[test]
    fn regression_c02_guest_host_api_accepts_core_prefix_offset() {
        assert_eq!(
            core::mem::size_of::<CoreOnlyHostApiPrefix>(),
            MIN_GUEST_APP_HOST_API_SIZE
        );
        assert!(MIN_GUEST_APP_HOST_API_SIZE < core::mem::size_of::<HostApi>());

        let prefix = CoreOnlyHostApiPrefix {
            struct_size: core::mem::size_of::<CoreOnlyHostApiPrefix>() as u32,
            abi_version: ABI_VERSION_PACKED,
            register_system: stub_register_system,
            iter_next_chunk: stub_iter_next_chunk,
            iter_column_readonly: stub_iter_column_readonly,
            iter_column_mutable: stub_iter_column_mutable,
            iter_mark_changed: stub_iter_mark_changed,
            iter_entities: stub_iter_entities,
            iter_column_stride: stub_iter_column_stride,
        };

        unsafe extern "C" fn empty_gpa(_name: *const c_char) -> *const c_void {
            core::ptr::null()
        }

        let dummy_ctx = core::ptr::without_provenance_mut(0x1000);
        let prefix_ptr = (&raw const prefix).cast::<HostApi>();

        let init = unsafe { ValidatedInit::from_raw(empty_gpa, prefix_ptr, dummy_ctx) }
            .expect("core prefix must satisfy GuestApp offset check");
        assert_eq!(init.host_api().abi_version, ABI_VERSION_PACKED);
        assert_eq!(
            init.register_component::<u32>(c"MyComp"),
            FfiResult::INVALID_VERSION
        );

        let mut truncated = CoreOnlyHostApiPrefix {
            struct_size: (MIN_GUEST_APP_HOST_API_SIZE - 1) as u32,
            ..prefix
        };
        let truncated_ptr = (&raw mut truncated).cast::<HostApi>();
        let err = unsafe { ValidatedInit::from_raw(empty_gpa, truncated_ptr, dummy_ctx) };
        assert_eq!(err.err(), Some(FfiResult::INVALID_VERSION));
    }

    #[test]
    fn regression_g06_resolve_fn_resolves_and_handles_missing_symbols() {
        unsafe extern "C" fn mock_reset(_iter: *mut RawChunkIter) -> FfiResult {
            FfiResult::OK
        }

        unsafe extern "C" fn mock_gpa(symbol_name: *const c_char) -> *const c_void {
            let name = unsafe { CStr::from_ptr(symbol_name) };
            if name == c"iter_reset" {
                mock_reset as *const c_void
            } else {
                core::ptr::null()
            }
        }

        let found = unsafe { resolve_fn::<IterResetFn>(mock_gpa, c"iter_reset") };
        assert!(found.is_some());
        assert_eq!(
            unsafe { (found.unwrap())(core::ptr::without_provenance_mut(0x1)) },
            FfiResult::OK
        );

        let missing = unsafe { resolve_fn::<IterResetFn>(mock_gpa, c"unknown_symbol") };
        assert!(missing.is_none());
    }
}
