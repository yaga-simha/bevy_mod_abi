#![deny(improper_ctypes_definitions)]

//! # `bevy_abi_mod_guest`: Zero-Bevy Ergonomic Rust SDK for Mod Authors
//!
//! This crate provides safe, idiomatic Rust abstractions for creating dynamic mods
//! (`.cdylib`) that integrate seamlessly into Bevy ECS via the `bevy_abi_mod` framework.
//!
//! ## Architecture: Why Zero Bevy Dependency?
//! Mod dynamic libraries must compile independently of the host game engine. If guest mods
//! linked `bevy`, any discrepancy in `rustc` compiler version, Cargo feature flags, or standard
//! library implementations would trigger ABI incompatibility and undefined behavior.
//! By depending solely on `bevy_abi_mod_guest_sys` and `#![no_std]` [`bevy_abi_mod_sys`],
//! `bevy_abi_mod_guest` enables mods to interact with the host exclusively over `extern "C"`.
//!
//! ## Idiomatic Mod Definition
//!
//! ```no_run
//! use bevy_abi_mod_guest::prelude::*;
//!
//! #[repr(C)]
//! #[derive(Clone, Copy)]
//! struct Position { x: f32, y: f32, z: f32 }
//!
//! #[repr(C)]
//! #[derive(Clone, Copy)]
//! struct Velocity { x: f32, y: f32, z: f32 }
//!
//! fn setup(app: &mut GuestApp<'_>) -> FfiResult {
//!     let queries = [
//!         ComponentAccessDesc::write(c"Position".as_ptr()),
//!         ComponentAccessDesc::read(c"Velocity".as_ptr()),
//!     ];
//!     app.add_system(c"movement_system", &queries, |ctx| {
//!         while let Some(mut chunk) = ctx.iter().next_chunk_view() {
//!             let Some((vel, pos)) = chunk.read_write::<Velocity, Position>(1, 0) else {
//!                 return FfiResult::INTERNAL_ERROR;
//!             };
//!             for (p, v) in pos.iter_mut().zip(vel.iter()) {
//!                 p.x += v.x;
//!                 p.y += v.y;
//!                 p.z += v.z;
//!             }
//!         }
//!         FfiResult::OK
//!     })
//! }
//!
//! export_mod!(setup);
//! ```

use core::ffi::{CStr, c_char, c_void};

#[doc(hidden)]
pub use bevy_abi_mod_guest_sys as sys;
pub use bevy_abi_mod_sys;

#[cfg(feature = "derive")]
pub use bevy_abi_mod_macros::AbiPod;
#[cfg(not(feature = "derive"))]
pub use bevy_abi_mod_sys::AbiPod;

pub use bevy_abi_mod_guest_sys::{
    CommandBridgeVTable, CommandBridgeVTableExt, MAX_GUEST_MULTI_QUERIES, ResourceBridgeVTable,
    catch_guest_panic, cleanup_registered_guest_state, register_runtime_component,
    resolve_iter_reset, resolve_register_conditional_multi_query,
    resolve_register_conditional_system, resolve_register_multi_query, wrap_multi_query_callback,
    wrap_run_condition, wrap_system_callback,
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

use bevy_abi_mod_guest_sys::{
    ErasedAllocation, GuestInitHandles, RawMultiQueryInvocation, RawSystemInvocation,
    abort_allocations, byte_ranges_overlap, commit_allocations, raw_cmd_despawn,
    raw_cmd_insert_bytes, raw_cmd_insert_default, raw_cmd_insert_default_cstr,
    raw_cmd_insert_slice, raw_cmd_remove_component, raw_cmd_remove_component_cstr,
    raw_cmd_spawn_empty, raw_deref_resource, raw_deref_resource_mut, raw_iter_column_mutable,
    raw_iter_column_readonly, raw_iter_column_stride, raw_iter_entities, raw_iter_mark_changed,
    raw_iter_next_chunk, raw_iter_reset, raw_iter_resource_mutable, raw_iter_resource_readonly,
    raw_slice_as_typed, raw_slice_as_typed_mut, raw_slice_get_ptr, raw_slice_get_ptr_mut,
    register_conditional_multi_query_closure, register_conditional_system_closure,
    register_multi_query_closure, register_system_closure, resolve_symbol_cstr, resolve_vtable_raw,
    validate_host_api, validated_entities_slice,
};

/// Macro to export a mod's lifecycle entry points (`bevy_abi_mod_abi_version`,
/// `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`) with guest-side panic containment
/// and automatic [`GuestApp`] state teardown.
///
/// Supports three forms:
/// 1. `export_mod!(setup_fn)` — takes `fn(&mut GuestApp<'_>) -> FfiResult`.
/// 2. `export_mod!(plugin: MyModType)` — takes a type implementing [`GuestPlugin`].
/// 3. `export_mod!(init_fn, shutdown_fn)` — low-level C-ABI init/shutdown functions.
///
/// # Example
///
/// ```no_run
/// use bevy_abi_mod_guest::{export_mod, FfiResult, GuestApp};
///
/// fn setup(_app: &mut GuestApp<'_>) -> FfiResult {
///     FfiResult::OK
/// }
/// export_mod!(setup);
/// ```
#[macro_export]
macro_rules! export_mod {
    ($build_fn:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_abi_version() -> u32 {
            $crate::ABI_VERSION_PACKED
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            get_proc_address: $crate::GetProcAddressFn,
            host_api: *const $crate::HostApi,
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            // SAFETY: Entry point invoked by host during `load_mod` with valid handshake pointers.
            unsafe {
                $crate::run_guest_app_init(get_proc_address, host_api, host_context, $build_fn)
            }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            _host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| {
                $crate::cleanup_registered_guest_state();
                $crate::FfiResult::OK
            })
        }
    };
    (plugin: $plugin_ty:ty) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_abi_version() -> u32 {
            $crate::ABI_VERSION_PACKED
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            get_proc_address: $crate::GetProcAddressFn,
            host_api: *const $crate::HostApi,
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            // SAFETY: Entry point invoked by host during `load_mod` with valid handshake pointers.
            unsafe {
                $crate::run_guest_app_init(
                    get_proc_address,
                    host_api,
                    host_context,
                    <$plugin_ty as $crate::GuestPlugin>::build,
                )
            }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            _host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            let status =
                $crate::catch_guest_panic(|| <$plugin_ty as $crate::GuestPlugin>::shutdown());
            $crate::cleanup_registered_guest_state();
            status
        }
    };
    ($init_fn:path, $shutdown_fn:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_abi_version() -> u32 {
            $crate::ABI_VERSION_PACKED
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            get_proc_address: $crate::GetProcAddressFn,
            host_api: *const $crate::HostApi,
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| $init_fn(get_proc_address, host_api, host_context))
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            let status = $crate::catch_guest_panic(|| $shutdown_fn(host_context));
            $crate::cleanup_registered_guest_state();
            status
        }
    };
}

// ============================================================================
// Deferred Command Queue
// ============================================================================

/// Wrapper around the host's deferred command queue handle ([`RawCommandQueue`]).
///
/// Prefer obtaining a `GuestCommandQueue` safely through [`SystemContext::commands`] or
/// [`SystemContext::iter_and_commands`] when using [`GuestApp`].
///
/// # Example
///
/// ```
/// use bevy_abi_mod_guest::{GuestCommandQueue, CommandBridgeVTable, FfiEntity, FfiResult};
///
/// let mut queue = GuestCommandQueue::new(core::ptr::null_mut(), CommandBridgeVTable::default());
/// assert_eq!(queue.spawn_empty(), FfiEntity(0));
/// assert_eq!(queue.despawn(FfiEntity(1)), FfiResult::NULL_POINTER);
/// assert_eq!(queue.insert_value(FfiEntity(1), c"Position", &42.0f32), FfiResult::NULL_POINTER);
/// ```
pub struct GuestCommandQueue<'a> {
    raw: *mut RawCommandQueue,
    vtable: CommandBridgeVTable,
    remove_component: Option<CmdRemoveComponentFn>,
    insert_default: Option<CmdInsertDefaultFn>,
    _marker: core::marker::PhantomData<&'a mut ()>,
}

impl<'a> GuestCommandQueue<'a> {
    /// Creates a new guest command queue wrapper from an opaque handle and resolved VTable.
    #[inline]
    pub fn new(raw: *mut RawCommandQueue, vtable: CommandBridgeVTable) -> Self {
        Self {
            raw,
            vtable,
            remove_component: None,
            insert_default: None,
            _marker: core::marker::PhantomData,
        }
    }

    /// Creates a new guest command queue wrapper from an opaque handle and extended command VTable.
    #[inline]
    pub fn with_ext(raw: *mut RawCommandQueue, ext: CommandBridgeVTableExt) -> Self {
        Self {
            raw,
            vtable: ext.base,
            remove_component: ext.remove_component,
            insert_default: ext.insert_default,
            _marker: core::marker::PhantomData,
        }
    }

    /// Attaches an optional `cmd_remove_component` function pointer to this queue wrapper.
    #[inline]
    pub fn with_remove_component(mut self, remove_fn: Option<CmdRemoveComponentFn>) -> Self {
        self.remove_component = remove_fn;
        self
    }

    /// Attaches an optional `cmd_insert_default` function pointer to this queue wrapper.
    #[inline]
    pub fn with_insert_default(mut self, insert_default_fn: Option<CmdInsertDefaultFn>) -> Self {
        self.insert_default = insert_default_fn;
        self
    }

    /// Returns `true` if the underlying `RawCommandQueue` pointer is null.
    #[inline]
    pub fn is_null(&self) -> bool {
        self.raw.is_null()
    }

    /// Enqueues the creation of a new empty entity and returns its allocated [`FfiEntity`].
    #[inline]
    pub fn spawn_empty(&mut self) -> FfiEntity {
        // SAFETY: `self.raw` is either null or a valid host `RawCommandQueue` handle for `&mut self`.
        unsafe { raw_cmd_spawn_empty(self.raw, &self.vtable) }
    }

    /// Enqueues the despawning of the specified entity.
    #[inline]
    pub fn despawn(&mut self, entity: FfiEntity) -> FfiResult {
        // SAFETY: `self.raw` is either null or a valid host `RawCommandQueue` handle for `&mut self`.
        unsafe { raw_cmd_despawn(self.raw, &self.vtable, entity) }
    }

    /// Enqueues the insertion of raw component bytes for the target entity.
    ///
    /// # Safety
    /// - `component_name` must point to a valid, nul-terminated UTF-8 C string.
    /// - `data` must point to at least `len` contiguous initialized bytes matching the component layout.
    /// - If `len > 0`, `data` must not be null.
    /// - The memory pointed to by `data` must not be concurrently mutated during this call.
    #[inline]
    pub unsafe fn insert_bytes(
        &mut self,
        entity: FfiEntity,
        component_name: *const c_char,
        data: *const u8,
        len: usize,
    ) -> FfiResult {
        // SAFETY: Forwarding caller's raw pointer invariants to `bevy_abi_mod_guest_sys`.
        unsafe { raw_cmd_insert_bytes(self.raw, &self.vtable, entity, component_name, data, len) }
    }

    /// Safely enqueues the insertion of a typed slice of POD elements for the target entity.
    #[inline]
    pub fn insert_slice<T: Copy>(
        &mut self,
        entity: FfiEntity,
        component_name: &CStr,
        data: &[T],
    ) -> FfiResult {
        // SAFETY: `self.raw` is either null or a valid host `RawCommandQueue` handle for `&mut self`.
        unsafe { raw_cmd_insert_slice(self.raw, &self.vtable, entity, component_name, data) }
    }

    /// Safely enqueues the insertion of a single typed POD component value for the target entity.
    #[inline]
    pub fn insert_value<T: Copy>(
        &mut self,
        entity: FfiEntity,
        component_name: &CStr,
        value: &T,
    ) -> FfiResult {
        self.insert_slice(entity, component_name, core::slice::from_ref(value))
    }

    /// Safely enqueues the deferred insertion of a typed [`GuestComponent`] value onto `entity`.
    #[inline]
    pub fn insert_component<T: GuestComponent>(
        &mut self,
        entity: FfiEntity,
        value: &T,
    ) -> FfiResult {
        self.insert_value(entity, T::NAME, value)
    }

    /// Safely enqueues the deferred insertion of a typed runtime dynamic component value onto the target entity.
    #[inline]
    pub fn insert_runtime_component<T: Copy>(
        &mut self,
        entity: FfiEntity,
        name: &CStr,
        value: &T,
    ) -> FfiResult {
        self.insert_value(entity, name, value)
    }

    /// Enqueues the deferred removal of a component by raw C string name from `entity`.
    ///
    /// # Safety
    /// `component_name` must point to a valid, nul-terminated UTF-8 C string.
    #[inline]
    pub unsafe fn remove_component_raw(
        &mut self,
        entity: FfiEntity,
        component_name: *const c_char,
    ) -> FfiResult {
        // SAFETY: Forwarding caller's raw C string invariant to `bevy_abi_mod_guest_sys`.
        unsafe { raw_cmd_remove_component(self.raw, self.remove_component, entity, component_name) }
    }

    /// Safely enqueues the deferred removal of `component_name` from `entity`.
    #[inline]
    pub fn remove_component(&mut self, entity: FfiEntity, component_name: &CStr) -> FfiResult {
        // SAFETY: `self.raw` is either null or a valid host `RawCommandQueue` handle for `&mut self`.
        unsafe {
            raw_cmd_remove_component_cstr(self.raw, self.remove_component, entity, component_name)
        }
    }

    /// Safely enqueues the deferred removal of a typed [`GuestComponent`] from `entity`.
    #[inline]
    pub fn remove_guest_component<T: GuestComponent>(&mut self, entity: FfiEntity) -> FfiResult {
        self.remove_component(entity, T::NAME)
    }

    /// Enqueues the deferred insertion of a host-constructed `T::default()` component onto `entity`
    /// by raw C string name.
    ///
    /// # Safety
    /// `component_name` must point to a valid, nul-terminated UTF-8 C string.
    #[inline]
    pub unsafe fn insert_default_raw(
        &mut self,
        entity: FfiEntity,
        component_name: *const c_char,
    ) -> FfiResult {
        // SAFETY: Forwarding caller's raw C string invariant to `bevy_abi_mod_guest_sys`.
        unsafe { raw_cmd_insert_default(self.raw, self.insert_default, entity, component_name) }
    }

    /// Safely enqueues the deferred insertion of a host-constructed `T::default()` component onto `entity`.
    #[inline]
    pub fn insert_default(&mut self, entity: FfiEntity, component_name: &CStr) -> FfiResult {
        // SAFETY: `self.raw` is either null or a valid host `RawCommandQueue` handle for `&mut self`.
        unsafe {
            raw_cmd_insert_default_cstr(self.raw, self.insert_default, entity, component_name)
        }
    }

    /// Safely enqueues the deferred default insertion of a typed [`GuestComponent`] onto `entity`.
    #[inline]
    pub fn insert_guest_default<T: GuestComponent>(&mut self, entity: FfiEntity) -> FfiResult {
        self.insert_default(entity, T::NAME)
    }
}

// ============================================================================
// Chunk Iteration & Disjoint Multi-Column Split Borrows
// ============================================================================

/// Wrapper around the host's chunk iterator handle ([`RawChunkIter`]).
pub struct GuestChunkIter<'a> {
    raw: *mut RawChunkIter,
    api: &'a HostApi,
    reset_fn: Option<IterResetFn>,
    res_vtable: ResourceBridgeVTable,
    allow_resource_access: bool,
}

impl<'a> GuestChunkIter<'a> {
    /// Creates a new guest chunk iterator wrapper from an opaque handle and host API table.
    #[inline]
    pub fn new(raw: *mut RawChunkIter, api: &'a HostApi) -> Self {
        Self {
            raw,
            api,
            reset_fn: None,
            res_vtable: ResourceBridgeVTable::default(),
            allow_resource_access: true,
        }
    }

    /// Creates a new guest chunk iterator wrapper with a pre-resolved [`IterResetFn`] for nested loops.
    #[inline]
    pub fn with_reset(
        raw: *mut RawChunkIter,
        api: &'a HostApi,
        reset_fn: Option<IterResetFn>,
    ) -> Self {
        Self {
            raw,
            api,
            reset_fn,
            res_vtable: ResourceBridgeVTable::default(),
            allow_resource_access: true,
        }
    }

    /// Creates a new guest chunk iterator wrapper with both [`IterResetFn`] and [`ResourceBridgeVTable`].
    #[inline]
    pub fn with_bridges(
        raw: *mut RawChunkIter,
        api: &'a HostApi,
        reset_fn: Option<IterResetFn>,
        res_vtable: ResourceBridgeVTable,
    ) -> Self {
        Self {
            raw,
            api,
            reset_fn,
            res_vtable,
            allow_resource_access: true,
        }
    }

    #[inline]
    fn for_multi_query(
        raw: *mut RawChunkIter,
        api: &'a HostApi,
        reset_fn: Option<IterResetFn>,
    ) -> Self {
        Self {
            raw,
            api,
            reset_fn,
            res_vtable: ResourceBridgeVTable::default(),
            allow_resource_access: false,
        }
    }

    /// Attaches a resolved [`IterResetFn`] to enable [`GuestChunkIter::reset`].
    #[inline]
    pub fn set_reset_fn(&mut self, reset_fn: Option<IterResetFn>) {
        self.reset_fn = reset_fn;
    }

    /// Attaches a resolved [`ResourceBridgeVTable`] to enable [`GuestChunkIter::resource`] / [`GuestChunkIter::resource_mut`].
    #[inline]
    pub fn set_resource_vtable(&mut self, res_vtable: ResourceBridgeVTable) {
        self.res_vtable = res_vtable;
    }

    /// Rewinds the chunk iterator cursor back to the first matched chunk.
    #[inline]
    pub fn reset(&mut self) -> FfiResult {
        // SAFETY: `self.raw` is either null or exclusively borrowed via `&mut self`.
        unsafe { raw_iter_reset(self.raw, self.reset_fn) }
    }

    /// Rewinds the chunk iterator cursor using an explicitly supplied [`IterResetFn`].
    #[inline]
    pub fn reset_with(&mut self, reset_fn: IterResetFn) -> FfiResult {
        // SAFETY: `self.raw` is either null or exclusively borrowed via `&mut self`.
        unsafe { raw_iter_reset(self.raw, Some(reset_fn)) }
    }

    /// Advances to the next archetype table chunk and returns the entity count (`0` when exhausted).
    #[inline]
    pub fn next_chunk(&mut self) -> usize {
        // SAFETY: `self.raw` is either null or exclusively borrowed via `&mut self`.
        unsafe { raw_iter_next_chunk(self.raw, self.api) }
    }

    /// Advances to the next chunk and returns a scoped [`Chunk`] view if non-empty (`None` when exhausted).
    #[inline]
    pub fn next_chunk_view(&mut self) -> Option<Chunk<'_, 'a>> {
        let count = self.next_chunk();
        if count == 0 {
            None
        } else {
            Some(Chunk { iter: self, count })
        }
    }

    /// Obtains a read-only pointer to the start of the specified column in the current chunk.
    #[inline]
    pub fn column_readonly(&self, index: usize) -> *const u8 {
        // SAFETY: `self.raw` is either null or borrowed via `&self`.
        unsafe { raw_iter_column_readonly(self.raw, self.api, index) }
    }

    /// Obtains a mutable pointer to the start of the specified column in the current chunk.
    #[inline]
    pub fn column_mutable(&mut self, index: usize) -> *mut u8 {
        // SAFETY: `self.raw` is either null or exclusively borrowed via `&mut self`.
        unsafe { raw_iter_column_mutable(self.raw, self.api, index) }
    }

    /// Explicitly marks the given row within the column as changed.
    #[inline]
    pub fn mark_changed(&mut self, index: usize, row: usize) -> FfiResult {
        // SAFETY: `self.raw` is either null or exclusively borrowed via `&mut self`.
        unsafe { raw_iter_mark_changed(self.raw, self.api, index, row) }
    }

    /// Obtains a raw pointer to the contiguous array of entity IDs in the current chunk.
    #[inline]
    pub fn entities(&self) -> *const FfiEntity {
        // SAFETY: `self.raw` is either null or borrowed via `&self`.
        unsafe { raw_iter_entities(self.raw, self.api) }
    }

    /// Returns a validated slice of [`FfiEntity`] identifiers for the current chunk.
    #[inline]
    pub fn entities_slice(&self, count: usize) -> &[FfiEntity] {
        if self.raw.is_null() || count == 0 {
            return &[];
        }
        // SAFETY: `self.entities()` yields the active chunk's entity column pointer for `&self`.
        unsafe { validated_entities_slice(self.entities(), count) }
    }

    /// Returns the byte stride for elements in the specified column.
    #[inline]
    pub fn column_stride(&self, index: usize) -> usize {
        // SAFETY: `self.raw` is either null or borrowed via `&self`.
        unsafe { raw_iter_column_stride(self.raw, self.api, index) }
    }

    /// Returns a structured, bounds-checked read-only slice for the given column.
    #[inline]
    pub fn readonly_slice(&self, index: usize, count: usize) -> ChunkSlice<'_> {
        let ptr = self.column_readonly(index);
        let stride = self.column_stride(index);
        ChunkSlice::new(ptr, stride, count)
    }

    /// Returns a structured, bounds-checked mutable slice for the given column.
    #[inline]
    pub fn mutable_slice(&mut self, index: usize, count: usize) -> ChunkSliceMut<'_> {
        let ptr = self.column_mutable(index);
        let stride = self.column_stride(index);
        ChunkSliceMut::new(ptr, stride, count)
    }

    /// Simultaneously borrows `entities`, `R` read-only columns, and `W` disjoint mutable columns
    /// from the active chunk.
    pub fn split_slices<const R: usize, const W: usize>(
        &mut self,
        read_cols: [usize; R],
        write_cols: [usize; W],
        count: usize,
    ) -> Option<(&[FfiEntity], [ChunkSlice<'_>; R], [ChunkSliceMut<'_>; W])> {
        for i in 0..W {
            for j in (i + 1)..W {
                if write_cols[i] == write_cols[j] {
                    return None;
                }
            }
            for &r_col in &read_cols {
                if r_col == write_cols[i] {
                    return None;
                }
            }
        }

        if self.raw.is_null() && count > 0 {
            return None;
        }

        let ent_ptr = self.entities();
        // SAFETY: `ent_ptr` is the active chunk's entity column pointer for `&mut self`.
        let ent_slice = unsafe { validated_entities_slice(ent_ptr, count) };
        let ent_addr = ent_slice.as_ptr() as usize;
        let ent_bytes = core::mem::size_of_val(ent_slice);

        let mut ro_ptrs = [core::ptr::null::<u8>(); R];
        let mut ro_strides = [0usize; R];
        let mut ro_bytes = [0usize; R];

        for i in 0..R {
            let ptr = self.column_readonly(read_cols[i]);
            let stride = self.column_stride(read_cols[i]);
            if count > 0 && ptr.is_null() {
                return None;
            }
            let bytes = count.checked_mul(stride)?;
            if bytes > (isize::MAX as usize) {
                return None;
            }
            if count > 0 && (ptr as usize).checked_add(bytes).is_none() {
                return None;
            }
            ro_ptrs[i] = ptr;
            ro_strides[i] = stride;
            ro_bytes[i] = bytes;
        }

        let mut rw_ptrs = [core::ptr::null_mut::<u8>(); W];
        let mut rw_strides = [0usize; W];
        let mut rw_bytes = [0usize; W];

        for i in 0..W {
            let ptr = self.column_mutable(write_cols[i]);
            let stride = self.column_stride(write_cols[i]);
            if count > 0 && ptr.is_null() {
                return None;
            }
            let bytes = count.checked_mul(stride)?;
            if bytes > (isize::MAX as usize) {
                return None;
            }
            if count > 0 && (ptr as usize).checked_add(bytes).is_none() {
                return None;
            }
            rw_ptrs[i] = ptr;
            rw_strides[i] = stride;
            rw_bytes[i] = bytes;
        }

        if count > 0 {
            for i in 0..W {
                let addr_w = rw_ptrs[i] as usize;
                let len_w = rw_bytes[i];

                if byte_ranges_overlap(addr_w, len_w, ent_addr, ent_bytes) {
                    return None;
                }
                for j in (i + 1)..W {
                    if byte_ranges_overlap(addr_w, len_w, rw_ptrs[j] as usize, rw_bytes[j]) {
                        return None;
                    }
                }
                for r in 0..R {
                    if byte_ranges_overlap(addr_w, len_w, ro_ptrs[r] as usize, ro_bytes[r]) {
                        return None;
                    }
                }
            }
        }

        let ro_slices = core::array::from_fn(|i| ChunkSlice::new(ro_ptrs[i], ro_strides[i], count));
        let rw_slices =
            core::array::from_fn(|i| ChunkSliceMut::new(rw_ptrs[i], rw_strides[i], count));

        Some((ent_slice, ro_slices, rw_slices))
    }

    /// Borrows `N` disjoint mutable columns simultaneously from the active chunk.
    #[inline]
    pub fn columns_mut<const N: usize>(
        &mut self,
        indices: [usize; N],
        count: usize,
    ) -> Option<[ChunkSliceMut<'_>; N]> {
        let (_, _, rw) = self.split_slices([], indices, count)?;
        Some(rw)
    }

    /// Simultaneously borrows one typed read-only column (`&[R]`) and one typed mutable column (`&mut [W]`).
    #[inline]
    pub fn read_write<R: Copy, W: Copy>(
        &mut self,
        read_col: usize,
        write_col: usize,
        count: usize,
    ) -> Option<(&[R], &mut [W])> {
        let (_, [ro], [rw]) = self.split_slices([read_col], [write_col], count)?;
        let r_typed = ro.as_typed_slice::<R>()?;
        let w_typed = rw.into_typed_slice::<W>()?;
        Some((r_typed, w_typed))
    }

    /// Simultaneously borrows two disjoint typed mutable columns (`&mut [W1]`, `&mut [W2]`).
    #[inline]
    pub fn write_2<W1: Copy, W2: Copy>(
        &mut self,
        col_a: usize,
        col_b: usize,
        count: usize,
    ) -> Option<(&mut [W1], &mut [W2])> {
        let [rw_a, rw_b] = self.columns_mut([col_a, col_b], count)?;
        let a_typed = rw_a.into_typed_slice::<W1>()?;
        let b_typed = rw_b.into_typed_slice::<W2>()?;
        Some((a_typed, b_typed))
    }

    /// Obtains an optional read-only column pointer (`Option<&T>`) for the current chunk.
    #[inline]
    pub fn optional_column_ptr(&self, index: usize) -> Option<*const u8> {
        let ptr = self.column_readonly(index);
        if ptr.is_null() { None } else { Some(ptr) }
    }

    /// Obtains an optional mutable column pointer (`Option<&mut T>`) for the current chunk.
    #[inline]
    pub fn optional_column_mut_ptr(&mut self, index: usize) -> Option<*mut u8> {
        let ptr = self.column_mutable(index);
        if ptr.is_null() { None } else { Some(ptr) }
    }

    /// Returns `Some(ChunkSlice)` when an `OPTIONAL_READ` column is present on the current chunk.
    #[inline]
    pub fn optional_readonly_slice(&self, index: usize, count: usize) -> Option<ChunkSlice<'_>> {
        let ptr = self.optional_column_ptr(index)?;
        let stride = self.column_stride(index);
        Some(ChunkSlice::new(ptr, stride, count))
    }

    /// Returns `Some(ChunkSliceMut)` when an `OPTIONAL_WRITE` column is present on the current chunk.
    #[inline]
    pub fn optional_mutable_slice(
        &mut self,
        index: usize,
        count: usize,
    ) -> Option<ChunkSliceMut<'_>> {
        let ptr = self.optional_column_mut_ptr(index)?;
        let stride = self.column_stride(index);
        Some(ChunkSliceMut::new(ptr, stride, count))
    }

    /// Simultaneously borrows an optional read-only column (`Option<&[OptR]>`) and a required
    /// disjoint mutable column (`&mut [W]`).
    pub fn optional_read_write<OptR: Copy, W: Copy>(
        &mut self,
        opt_read_col: usize,
        write_col: usize,
        count: usize,
    ) -> Option<(Option<&[OptR]>, &mut [W])> {
        if opt_read_col == write_col {
            return None;
        }
        if self.raw.is_null() && count > 0 {
            return None;
        }

        let opt_ptr = self.column_readonly(opt_read_col);
        let opt_stride = self.column_stride(opt_read_col);

        let w_ptr = self.column_mutable(write_col);
        let w_stride = self.column_stride(write_col);
        if count > 0 && w_ptr.is_null() {
            return None;
        }
        let w_bytes = count.checked_mul(w_stride)?;
        if w_bytes > (isize::MAX as usize)
            || (count > 0 && (w_ptr as usize).checked_add(w_bytes).is_none())
        {
            return None;
        }

        let opt_slice = if opt_ptr.is_null() {
            None
        } else {
            let opt_bytes = count.checked_mul(opt_stride)?;
            if opt_bytes > (isize::MAX as usize)
                || (count > 0 && (opt_ptr as usize).checked_add(opt_bytes).is_none())
            {
                return None;
            }
            if count > 0
                && byte_ranges_overlap(w_ptr as usize, w_bytes, opt_ptr as usize, opt_bytes)
            {
                return None;
            }
            let raw_ro = ChunkSlice::new(opt_ptr, opt_stride, count);
            Some(raw_ro.as_typed_slice::<OptR>()?)
        };

        let raw_rw = ChunkSliceMut::new(w_ptr, w_stride, count);
        let w_typed = raw_rw.into_typed_slice::<W>()?;
        Some((opt_slice, w_typed))
    }

    /// Obtains a read-only byte pointer to an exposed Bevy `Resource` (`Res<R>`).
    #[inline]
    pub fn resource_ptr(&self, resource_name: &CStr) -> *const u8 {
        if !self.allow_resource_access {
            return core::ptr::null();
        }
        // SAFETY: `self.raw` is either null or borrowed via `&self`.
        unsafe { raw_iter_resource_readonly(self.raw, &self.res_vtable, resource_name) }
    }

    /// Obtains a mutable byte pointer to an exposed Bevy `Resource` (`ResMut<R>`).
    #[inline]
    pub fn resource_mut_ptr(&mut self, resource_name: &CStr) -> *mut u8 {
        if !self.allow_resource_access {
            return core::ptr::null_mut();
        }
        // SAFETY: `self.raw` is either null or exclusively borrowed via `&mut self`.
        unsafe { raw_iter_resource_mutable(self.raw, &self.res_vtable, resource_name) }
    }

    /// Safely borrows a typed `#[repr(C)]` POD Bevy `Resource` (`Res<R>`) for reading.
    #[inline]
    pub fn resource<R: Copy>(&self, resource_name: &CStr) -> Option<&R> {
        // SAFETY: `resource_ptr` returns null or a valid host resource pointer for `&self`.
        unsafe { raw_deref_resource::<R>(self.resource_ptr(resource_name)) }
    }

    /// Safely borrows a typed `#[repr(C)]` POD Bevy `Resource` (`ResMut<R>`) mutably.
    #[inline]
    pub fn resource_mut<R: Copy>(&mut self, resource_name: &CStr) -> Option<&mut R> {
        // SAFETY: `resource_mut_ptr` returns null or an exclusively borrowed host resource pointer for `&mut self`.
        unsafe { raw_deref_resource_mut::<R>(self.resource_mut_ptr(resource_name)) }
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`Res<R>`) using `R::NAME`.
    #[inline]
    pub fn guest_resource<R: GuestComponent>(&self) -> Option<&R> {
        self.resource::<R>(R::NAME)
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`ResMut<R>`) mutably using `R::NAME`.
    #[inline]
    pub fn guest_resource_mut<R: GuestComponent>(&mut self) -> Option<&mut R> {
        self.resource_mut::<R>(R::NAME)
    }
}

/// Scoped handle to a single non-empty chunk yielded by [`GuestChunkIter::next_chunk_view`].
pub struct Chunk<'iter, 'api> {
    iter: &'iter mut GuestChunkIter<'api>,
    count: usize,
}

impl<'iter, 'api> Chunk<'iter, 'api> {
    /// Number of entities in this chunk (guaranteed `>= 1` when yielded by `next_chunk_view`).
    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Alias for [`Chunk::len`].
    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Returns `true` if this chunk has zero entities.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the validated slice of [`FfiEntity`] handles for all rows in this chunk.
    #[inline]
    pub fn entities(&self) -> &[FfiEntity] {
        self.iter.entities_slice(self.count)
    }

    /// Returns an untyped bounds-checked read-only [`ChunkSlice`] for column `index`.
    #[inline]
    pub fn readonly_slice(&self, index: usize) -> ChunkSlice<'_> {
        self.iter.readonly_slice(index, self.count)
    }

    /// Returns an untyped bounds-checked mutable [`ChunkSliceMut`] for column `index`.
    #[inline]
    pub fn mutable_slice(&mut self, index: usize) -> ChunkSliceMut<'_> {
        self.iter.mutable_slice(index, self.count)
    }

    /// Borrows a typed read-only slice `&[T]` for column `index`.
    #[inline]
    pub fn column<T: Copy>(&self, index: usize) -> Option<&[T]> {
        self.iter
            .readonly_slice(index, self.count)
            .as_typed_slice::<T>()
    }

    /// Borrows a typed mutable slice `&mut [T]` for column `index`.
    #[inline]
    pub fn column_mut<T: Copy>(&mut self, index: usize) -> Option<&mut [T]> {
        self.iter
            .mutable_slice(index, self.count)
            .into_typed_slice::<T>()
    }

    /// Simultaneously borrows the chunk's [`FfiEntity`] slice and one typed mutable column `&mut [T]`.
    #[inline]
    pub fn entities_and_column_mut<T: Copy>(
        &mut self,
        index: usize,
    ) -> Option<(&[FfiEntity], &mut [T])> {
        let count = self.count;
        let (entities, _, [rw]) = self.iter.split_slices([], [index], count)?;
        let typed = rw.into_typed_slice::<T>()?;
        Some((entities, typed))
    }

    /// Simultaneously borrows `entities`, `R` read-only [`ChunkSlice`]s, and `W` disjoint [`ChunkSliceMut`]s.
    #[inline]
    pub fn split_slices<const R: usize, const W: usize>(
        &mut self,
        read_cols: [usize; R],
        write_cols: [usize; W],
    ) -> Option<(&[FfiEntity], [ChunkSlice<'_>; R], [ChunkSliceMut<'_>; W])> {
        let count = self.count;
        self.iter.split_slices(read_cols, write_cols, count)
    }

    /// Simultaneously borrows `N` disjoint mutable [`ChunkSliceMut`] columns.
    #[inline]
    pub fn columns_mut<const N: usize>(
        &mut self,
        indices: [usize; N],
    ) -> Option<[ChunkSliceMut<'_>; N]> {
        let count = self.count;
        self.iter.columns_mut(indices, count)
    }

    /// Simultaneously borrows one typed read-only column `&[R]` and one disjoint typed mutable column `&mut [W]`.
    #[inline]
    pub fn read_write<R: Copy, W: Copy>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[R], &mut [W])> {
        let count = self.count;
        self.iter.read_write(read_col, write_col, count)
    }

    /// Simultaneously borrows `entities`, one typed read-only column `&[R]`, and one disjoint typed mutable column `&mut [W]`.
    #[inline]
    pub fn entities_and_read_write<R: Copy, W: Copy>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[FfiEntity], &[R], &mut [W])> {
        let count = self.count;
        let (entities, [ro], [rw]) = self.iter.split_slices([read_col], [write_col], count)?;
        let r_typed = ro.as_typed_slice::<R>()?;
        let w_typed = rw.into_typed_slice::<W>()?;
        Some((entities, r_typed, w_typed))
    }

    /// Simultaneously borrows two disjoint typed mutable columns (`&mut [W1]`, `&mut [W2]`).
    #[inline]
    pub fn write_2<W1: Copy, W2: Copy>(
        &mut self,
        col_a: usize,
        col_b: usize,
    ) -> Option<(&mut [W1], &mut [W2])> {
        let count = self.count;
        self.iter.write_2(col_a, col_b, count)
    }

    /// Explicitly marks `row` in column `index` as changed.
    #[inline]
    pub fn mark_changed(&mut self, index: usize, row: usize) -> FfiResult {
        self.iter.mark_changed(index, row)
    }

    /// Returns `Some(ptr)` when an `OPTIONAL_READ` column is present on this chunk's archetype.
    #[inline]
    pub fn optional_column_ptr(&self, index: usize) -> Option<*const u8> {
        self.iter.optional_column_ptr(index)
    }

    /// Returns `Some(ptr)` and marks ticks when an `OPTIONAL_WRITE` column is present on this chunk.
    #[inline]
    pub fn optional_column_mut_ptr(&mut self, index: usize) -> Option<*mut u8> {
        self.iter.optional_column_mut_ptr(index)
    }

    /// Returns `Some(&[T])` when an `OPTIONAL_READ` POD column is present on this chunk.
    #[inline]
    pub fn optional_column<T: Copy>(&self, index: usize) -> Option<&[T]> {
        self.iter
            .optional_readonly_slice(index, self.count)?
            .as_typed_slice::<T>()
    }

    /// Returns `Some(&mut [T])` and marks ticks when an `OPTIONAL_WRITE` POD column is present on this chunk.
    #[inline]
    pub fn optional_column_mut<T: Copy>(&mut self, index: usize) -> Option<&mut [T]> {
        self.iter
            .optional_mutable_slice(index, self.count)?
            .into_typed_slice::<T>()
    }

    /// Simultaneously borrows an optional read-only column (`Option<&[OptR]>`) and a required
    /// disjoint mutable column (`&mut [W]`) for this chunk.
    #[inline]
    pub fn optional_read_write<OptR: Copy, W: Copy>(
        &mut self,
        opt_read_col: usize,
        write_col: usize,
    ) -> Option<(Option<&[OptR]>, &mut [W])> {
        let count = self.count;
        self.iter
            .optional_read_write::<OptR, W>(opt_read_col, write_col, count)
    }

    /// Reads an exposed Bevy `Resource` (`Res<R>`) while inspecting this chunk.
    #[inline]
    pub fn resource<R: Copy>(&self, resource_name: &CStr) -> Option<&R> {
        self.iter.resource::<R>(resource_name)
    }

    /// Obtains a read-only byte pointer to an exposed Bevy `Resource` while inspecting this chunk.
    #[inline]
    pub fn resource_ptr(&self, resource_name: &CStr) -> *const u8 {
        self.iter.resource_ptr(resource_name)
    }
}

// ============================================================================
// ChunkSlice & ChunkSliceMut
// ============================================================================

/// Read-only chunk slice view for iterating components in an archetype table chunk.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_guest::ChunkSlice;
///
/// let numbers = [10u32, 20, 30];
/// let slice = ChunkSlice::new(numbers.as_ptr() as *const u8, core::mem::size_of::<u32>(), numbers.len());
/// assert_eq!(slice.count(), 3);
/// assert_eq!(slice.stride(), 4);
/// assert!(!slice.is_empty());
/// let ptr = slice.get_ptr(1).unwrap();
/// assert_eq!(unsafe { *(ptr as *const u32) }, 20);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct ChunkSlice<'a> {
    ptr: *const u8,
    stride: usize,
    count: usize,
    _marker: core::marker::PhantomData<&'a ()>,
}

impl<'a> ChunkSlice<'a> {
    /// Creates a new read-only chunk slice view.
    #[inline]
    pub fn new(ptr: *const u8, stride: usize, count: usize) -> Self {
        Self {
            ptr,
            stride,
            count,
            _marker: core::marker::PhantomData,
        }
    }

    /// Returns the number of entity elements in the current chunk.
    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Returns the byte stride between consecutive elements in the column.
    #[inline]
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Returns `true` if the chunk slice contains zero elements or points to null.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0 || self.ptr.is_null()
    }

    /// Returns a pointer to the element at the specified index within the chunk.
    #[inline]
    pub fn get_ptr(&self, index: usize) -> Option<*const u8> {
        // SAFETY: `self.ptr` and `(stride, count)` describe this `ChunkSlice`'s backing buffer.
        unsafe { raw_slice_get_ptr(self.ptr, self.stride, self.count, index) }
    }

    /// Converts this raw chunk slice into a typed slice of `#[repr(C)]` POD `T: Copy` elements.
    #[inline]
    pub fn as_typed_slice<T: Copy>(&self) -> Option<&'a [T]> {
        // SAFETY: `self.ptr` and `(stride, count)` describe this `ChunkSlice`'s backing buffer for `'a`.
        unsafe { raw_slice_as_typed::<T>(self.ptr, self.stride, self.count) }
    }
}

/// Mutable chunk slice view for iterating components in an archetype table chunk.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_guest::ChunkSliceMut;
///
/// let mut numbers = [10u32, 20, 30];
/// let mut slice = ChunkSliceMut::new(numbers.as_mut_ptr() as *mut u8, core::mem::size_of::<u32>(), numbers.len());
/// let ptr = slice.get_ptr_mut(1).unwrap();
/// unsafe { *(ptr as *mut u32) = 99 };
/// assert_eq!(numbers[1], 99);
/// ```
#[derive(Debug)]
pub struct ChunkSliceMut<'a> {
    ptr: *mut u8,
    stride: usize,
    count: usize,
    _marker: core::marker::PhantomData<&'a mut ()>,
}

impl<'a> ChunkSliceMut<'a> {
    /// Creates a new mutable chunk slice view.
    #[inline]
    pub fn new(ptr: *mut u8, stride: usize, count: usize) -> Self {
        Self {
            ptr,
            stride,
            count,
            _marker: core::marker::PhantomData,
        }
    }

    /// Returns the number of entity elements in the current chunk.
    #[inline]
    pub fn count(&self) -> usize {
        self.count
    }

    /// Returns the byte stride between consecutive elements in the column.
    #[inline]
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Returns `true` if the chunk slice contains zero elements or points to null.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0 || self.ptr.is_null()
    }

    /// Returns a mutable pointer to the element at the specified index within the chunk.
    #[inline]
    pub fn get_ptr_mut(&mut self, index: usize) -> Option<*mut u8> {
        // SAFETY: `self.ptr` and `(stride, count)` describe this `ChunkSliceMut`'s backing buffer.
        unsafe { raw_slice_get_ptr_mut(self.ptr, self.stride, self.count, index) }
    }

    /// Reborrows this mutable chunk slice as a typed `&mut [T]` slice of `#[repr(C)]` POD elements.
    #[inline]
    pub fn as_typed_slice<T: Copy>(&mut self) -> Option<&mut [T]> {
        // SAFETY: `self.ptr` is exclusively borrowed through `&mut self`.
        unsafe { raw_slice_as_typed_mut::<T>(self.ptr, self.stride, self.count) }
    }

    /// Consumes this `ChunkSliceMut<'a>` and converts it into `&'a mut [T]`.
    #[inline]
    pub fn into_typed_slice<T: Copy>(self) -> Option<&'a mut [T]> {
        // SAFETY: `self.ptr` is exclusively borrowed for `'a`.
        unsafe { raw_slice_as_typed_mut::<T>(self.ptr, self.stride, self.count) }
    }
}

// ============================================================================
// GuestComponent Trait
// ============================================================================

/// Marker trait for `#[repr(C)]` plain-old-data component structs defined or mirrored by guest mods.
///
/// # Safety
/// Implementors must ensure:
/// 1. `Self` is `#[repr(C)]` (or `#[repr(transparent)]`) with deterministic cross-compiler layout.
/// 2. Every bit pattern stored in the host's component column is valid for `Self` (no `bool`,
///    Rust `enum`, references, or heap pointers).
pub unsafe trait GuestComponent: Copy + 'static {
    /// Registered C-ABI component name (e.g. `c"Position"`).
    const NAME: &'static CStr;

    /// Builds a read-only (`&T`) query access descriptor for this component.
    #[inline]
    fn read_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::read(Self::NAME.as_ptr())
    }

    /// Builds a mutable (`&mut T`) query access descriptor for this component.
    #[inline]
    fn write_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::write(Self::NAME.as_ptr())
    }

    /// Builds a presence filter (`With<T>`) query access descriptor for this component.
    #[inline]
    fn with_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::filter_only(Self::NAME.as_ptr())
    }

    /// Builds an absence filter (`Without<T>`) query access descriptor for this component.
    #[inline]
    fn without_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::without(Self::NAME.as_ptr())
    }

    /// Builds an optional read-only (`Option<&T>`) query access descriptor for this component.
    #[inline]
    fn optional_read_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::optional_read(Self::NAME.as_ptr())
    }

    /// Builds an optional mutable (`Option<&mut T>`) query access descriptor for this component.
    #[inline]
    fn optional_write_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::optional_write(Self::NAME.as_ptr())
    }

    /// Builds a row-level change filter (`Changed<T>`) query access descriptor for this component.
    #[inline]
    fn changed_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::changed(Self::NAME.as_ptr())
    }

    /// Builds a row-level addition filter (`Added<T>`) query access descriptor for this component.
    #[inline]
    fn added_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::added(Self::NAME.as_ptr())
    }

    /// Builds a read-only resource (`Res<T>`) access descriptor for this type.
    #[inline]
    fn resource_read_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::resource_read(Self::NAME.as_ptr())
    }

    /// Builds a mutable resource (`ResMut<T>`) access descriptor for this type.
    #[inline]
    fn resource_write_desc() -> ComponentAccessDesc {
        ComponentAccessDesc::resource_write(Self::NAME.as_ptr())
    }
}

// ============================================================================
// High-Level Ergonomic Builder: GuestPlugin, GuestApp, SystemContext
// ============================================================================

/// Trait for modular Bevy-like guest plugin definitions exported via `export_mod!(plugin: MyMod)`.
pub trait GuestPlugin {
    /// Registers components, systems, and optional shutdown hooks on `app`.
    fn build(app: &mut GuestApp<'_>) -> FfiResult;

    /// Optional custom teardown callback invoked during `bevy_abi_mod_shutdown` prior to
    /// automatic [`GuestApp`] allocation cleanup.
    fn shutdown() -> FfiResult {
        FfiResult::OK
    }
}

/// Execution context passed to systems registered via [`GuestApp::add_system`].
pub struct SystemContext<'a> {
    iter: GuestChunkIter<'a>,
    commands: GuestCommandQueue<'a>,
    host_api: &'a HostApi,
}

impl<'a> SystemContext<'a> {
    /// Borrows the system's [`GuestChunkIter`] mutably.
    #[inline]
    pub fn iter(&mut self) -> &mut GuestChunkIter<'a> {
        &mut self.iter
    }

    /// Borrows the system's deferred [`GuestCommandQueue`] mutably.
    #[inline]
    pub fn commands(&mut self) -> &mut GuestCommandQueue<'a> {
        &mut self.commands
    }

    /// Simultaneously borrows both the [`GuestChunkIter`] and the deferred [`GuestCommandQueue`].
    #[inline]
    pub fn iter_and_commands(&mut self) -> (&mut GuestChunkIter<'a>, &mut GuestCommandQueue<'a>) {
        (&mut self.iter, &mut self.commands)
    }

    /// Returns a shared reference to the host's [`HostApi`] VTable.
    #[inline]
    pub fn host_api(&self) -> &'a HostApi {
        self.host_api
    }

    /// Obtains a read-only byte pointer to an exposed Bevy `Resource` (`Res<R>`).
    #[inline]
    pub fn resource_ptr(&self, resource_name: &CStr) -> *const u8 {
        self.iter.resource_ptr(resource_name)
    }

    /// Obtains a mutable byte pointer to an exposed Bevy `Resource` (`ResMut<R>`), marking its change tick.
    #[inline]
    pub fn resource_mut_ptr(&mut self, resource_name: &CStr) -> *mut u8 {
        self.iter.resource_mut_ptr(resource_name)
    }

    /// Safely borrows a typed `#[repr(C)]` POD Bevy `Resource` (`Res<R>`).
    #[inline]
    pub fn resource<R: Copy>(&self, resource_name: &CStr) -> Option<&R> {
        self.iter.resource::<R>(resource_name)
    }

    /// Safely borrows a typed `#[repr(C)]` POD Bevy `Resource` (`ResMut<R>`) mutably, marking its change tick.
    #[inline]
    pub fn resource_mut<R: Copy>(&mut self, resource_name: &CStr) -> Option<&mut R> {
        self.iter.resource_mut::<R>(resource_name)
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`Res<R>`).
    #[inline]
    pub fn guest_resource<R: GuestComponent>(&self) -> Option<&R> {
        self.iter.guest_resource::<R>()
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`ResMut<R>`) mutably.
    #[inline]
    pub fn guest_resource_mut<R: GuestComponent>(&mut self) -> Option<&mut R> {
        self.iter.guest_resource_mut::<R>()
    }
}

/// Execution context passed to multi-query systems registered via [`GuestApp::add_multi_query_system`].
pub struct MultiQueryContext<'a> {
    iters: [Option<GuestChunkIter<'a>>; MAX_GUEST_MULTI_QUERIES],
    iter_count: usize,
    commands: GuestCommandQueue<'a>,
    host_api: &'a HostApi,
    res_vtable: ResourceBridgeVTable,
}

impl<'a> MultiQueryContext<'a> {
    /// Number of query iterators passed to this multi-query system.
    #[inline]
    pub fn iter_count(&self) -> usize {
        self.iter_count
    }

    /// Borrows query iterator `index` mutably.
    #[inline]
    pub fn iter_mut(&mut self, index: usize) -> Option<&mut GuestChunkIter<'a>> {
        if index < self.iter_count {
            self.iters[index].as_mut()
        } else {
            None
        }
    }

    /// Simultaneously borrows two disjoint query iterators `(index_a, index_b)` for nested query loops.
    #[inline]
    pub fn iters_2_mut(
        &mut self,
        index_a: usize,
        index_b: usize,
    ) -> Option<(&mut GuestChunkIter<'a>, &mut GuestChunkIter<'a>)> {
        if index_a == index_b || index_a >= self.iter_count || index_b >= self.iter_count {
            return None;
        }
        if index_a < index_b {
            let (left, right) = self.iters.split_at_mut(index_b);
            Some((left[index_a].as_mut()?, right[0].as_mut()?))
        } else {
            let (left, right) = self.iters.split_at_mut(index_a);
            Some((right[0].as_mut()?, left[index_b].as_mut()?))
        }
    }

    /// Borrows the deferred [`GuestCommandQueue`] mutably.
    #[inline]
    pub fn commands(&mut self) -> &mut GuestCommandQueue<'a> {
        &mut self.commands
    }

    /// Simultaneously borrows two disjoint query iterators AND the deferred [`GuestCommandQueue`].
    #[inline]
    pub fn iters_2_and_commands(
        &mut self,
        index_a: usize,
        index_b: usize,
    ) -> Option<(
        &mut GuestChunkIter<'a>,
        &mut GuestChunkIter<'a>,
        &mut GuestCommandQueue<'a>,
    )> {
        if index_a == index_b || index_a >= self.iter_count || index_b >= self.iter_count {
            return None;
        }
        let commands = &mut self.commands;
        if index_a < index_b {
            let (left, right) = self.iters.split_at_mut(index_b);
            Some((left[index_a].as_mut()?, right[0].as_mut()?, commands))
        } else {
            let (left, right) = self.iters.split_at_mut(index_a);
            Some((right[0].as_mut()?, left[index_b].as_mut()?, commands))
        }
    }

    /// Returns a shared reference to the host's [`HostApi`] VTable.
    #[inline]
    pub fn host_api(&self) -> &'a HostApi {
        self.host_api
    }

    /// Obtains a read-only byte pointer to an exposed Bevy `Resource` (`Res<R>`) declared in any
    /// query spec of this multi-query system.
    #[inline]
    pub fn resource_ptr(&self, resource_name: &CStr) -> *const u8 {
        for slot in self.iters.iter().take(self.iter_count).flatten() {
            // SAFETY: `slot.raw` is either null or borrowed via `&self`.
            let ptr =
                unsafe { raw_iter_resource_readonly(slot.raw, &self.res_vtable, resource_name) };
            if !ptr.is_null() {
                return ptr;
            }
        }
        core::ptr::null()
    }

    /// Obtains a mutable byte pointer to an exposed Bevy `Resource` (`ResMut<R>`) declared with
    /// `RESOURCE_WRITE` in any query spec of this multi-query system, marking its change tick.
    #[inline]
    pub fn resource_mut_ptr(&mut self, resource_name: &CStr) -> *mut u8 {
        for slot in self.iters.iter_mut().take(self.iter_count).flatten() {
            // SAFETY: `slot.raw` is either null or exclusively borrowed via `&mut self`.
            let ptr =
                unsafe { raw_iter_resource_mutable(slot.raw, &self.res_vtable, resource_name) };
            if !ptr.is_null() {
                return ptr;
            }
        }
        core::ptr::null_mut()
    }

    /// Safely borrows a typed `#[repr(C)]` POD Bevy `Resource` (`Res<R>`) across any query spec.
    #[inline]
    pub fn resource<R: Copy>(&self, resource_name: &CStr) -> Option<&R> {
        // SAFETY: `resource_ptr` returns null or a valid host resource pointer for `&self`.
        unsafe { raw_deref_resource::<R>(self.resource_ptr(resource_name)) }
    }

    /// Safely borrows a typed `#[repr(C)]` POD Bevy `Resource` (`ResMut<R>`) mutably across any query spec.
    #[inline]
    pub fn resource_mut<R: Copy>(&mut self, resource_name: &CStr) -> Option<&mut R> {
        // SAFETY: `resource_mut_ptr` returns null or an exclusively borrowed host resource pointer for `&mut self`.
        unsafe { raw_deref_resource_mut::<R>(self.resource_mut_ptr(resource_name)) }
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`Res<R>`) using `R::NAME`.
    #[inline]
    pub fn guest_resource<R: GuestComponent>(&self) -> Option<&R> {
        self.resource::<R>(R::NAME)
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`ResMut<R>`) mutably using `R::NAME`.
    #[inline]
    pub fn guest_resource_mut<R: GuestComponent>(&mut self) -> Option<&mut R> {
        self.resource_mut::<R>(R::NAME)
    }
}

/// High-level mod initialization builder eliminating raw FFI globals and `user_data` memory leaks.
pub struct GuestApp<'a> {
    get_proc_address: GetProcAddressFn,
    handles: GuestInitHandles<'a>,
    staged_allocations: Vec<ErasedAllocation>,
}

impl<'a> GuestApp<'a> {
    /// Constructs a [`GuestApp`] from the raw C-ABI arguments passed to `bevy_abi_mod_init`.
    ///
    /// # Safety
    /// - `get_proc_address` must be a valid host symbol resolver function pointer.
    /// - If non-null, `host_api` must point to an initialized `HostApi` struct that outlives all
    ///   registered system executions for this mod load session.
    /// - If non-null, `host_context` must point to the host's initialization context for `bevy_abi_mod_init`.
    pub unsafe fn from_raw(
        get_proc_address: GetProcAddressFn,
        host_api: *const HostApi,
        host_context: *mut c_void,
    ) -> Result<Self, FfiResult> {
        // SAFETY: Forwarding C-ABI `bevy_abi_mod_init` handshake pointers to `bevy_abi_mod_guest_sys`.
        let api_ref = unsafe { validate_host_api(host_api, host_context)? };

        let cmd_vtable_ext = CommandBridgeVTableExt::resolve(get_proc_address);
        let res_vtable = ResourceBridgeVTable::resolve(get_proc_address);
        let iter_reset = resolve_iter_reset(get_proc_address);
        let register_multi_query = resolve_register_multi_query(get_proc_address);
        let register_conditional_system = resolve_register_conditional_system(get_proc_address);
        let register_conditional_multi_query =
            resolve_register_conditional_multi_query(get_proc_address);

        Ok(Self {
            get_proc_address,
            handles: GuestInitHandles {
                host_api: api_ref,
                host_context,
                cmd_vtable_ext,
                res_vtable,
                iter_reset,
                register_multi_query,
                register_conditional_system,
                register_conditional_multi_query,
            },
            staged_allocations: Vec::new(),
        })
    }

    /// Returns the host's [`HostApi`] table.
    #[inline]
    pub fn host_api(&self) -> &'a HostApi {
        self.handles.host_api
    }

    /// Returns the raw `host_context` pointer for interoperability with low-level registration helpers.
    #[inline]
    pub fn host_context(&self) -> *mut c_void {
        self.handles.host_context
    }

    /// Returns the host's `GetProcAddressFn` symbol resolver.
    #[inline]
    pub fn get_proc_address(&self) -> GetProcAddressFn {
        self.get_proc_address
    }

    /// Returns the pre-resolved [`CommandBridgeVTable`] for this world.
    #[inline]
    pub fn command_vtable(&self) -> CommandBridgeVTable {
        self.handles.cmd_vtable_ext.base
    }

    /// Returns the pre-resolved [`CommandBridgeVTableExt`] (including `remove_component` & `insert_default`).
    #[inline]
    pub fn command_vtable_ext(&self) -> CommandBridgeVTableExt {
        self.handles.cmd_vtable_ext
    }

    /// Returns the pre-resolved [`ResourceBridgeVTable`] (`res_readonly`, `res_mutable`) for this world.
    #[inline]
    pub fn resource_vtable(&self) -> ResourceBridgeVTable {
        self.handles.res_vtable
    }

    /// Resolves a raw symbol pointer by nul-terminated name via `get_proc_address`.
    #[inline]
    pub fn resolve_symbol(&self, symbol_name: &CStr) -> *const c_void {
        resolve_symbol_cstr(self.get_proc_address, symbol_name)
    }

    /// Resolves a typed static component VTable reference (e.g. `c"Position_vtable"`).
    ///
    /// # Safety
    /// Caller must ensure `V` matches the exact `#[repr(C)]` VTable layout exported by the host
    /// under `symbol_name`.
    #[inline]
    pub unsafe fn resolve_vtable<V: Copy + 'static>(&self, symbol_name: &CStr) -> Option<&'a V> {
        // SAFETY: Forwarding caller's VTable layout guarantee to `bevy_abi_mod_guest_sys`.
        unsafe { resolve_vtable_raw::<V>(self.get_proc_address, symbol_name) }
    }

    /// Registers a runtime dynamic component `T` under `name`.
    #[inline]
    pub fn register_component<T: 'static>(&mut self, name: &CStr) -> FfiResult {
        // SAFETY: `self.handles.host_api` and `self.handles.host_context` were validated in `GuestApp::from_raw`.
        unsafe {
            register_runtime_component::<T>(self.handles.host_api, self.handles.host_context, name)
        }
    }

    /// Registers a runtime dynamic component implementing [`GuestComponent`] using `T::NAME`.
    #[inline]
    pub fn register_guest_component<T: GuestComponent>(&mut self) -> FfiResult {
        self.register_component::<T>(T::NAME)
    }

    /// Allocates a guest-owned value on the heap and returns a stable `*mut T` suitable for
    /// `SystemDescriptor::user_data`, automatically freeing it on `bevy_abi_mod_shutdown`
    /// (or immediately if `bevy_abi_mod_init` fails).
    pub fn alloc_user_data<T: Send + 'static>(&mut self, value: T) -> *mut T {
        let (alloc, raw) = ErasedAllocation::new_boxed(value);
        self.staged_allocations.push(alloc);
        raw
    }

    /// Registers a custom cleanup closure to execute automatically during `bevy_abi_mod_shutdown`
    /// (or on rollback if `bevy_abi_mod_init` fails).
    pub fn on_shutdown<F>(&mut self, cleanup: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.staged_allocations
            .push(ErasedAllocation::new_shutdown_hook(cleanup));
    }

    /// Registers a panic-safe, leak-free Rust system closure with the host ECS schedule.
    pub fn add_system<F>(
        &mut self,
        name: &CStr,
        queries: &[ComponentAccessDesc],
        mut system: F,
    ) -> FfiResult
    where
        F: FnMut(&mut SystemContext<'_>) -> FfiResult + Send + 'static,
    {
        register_system_closure(
            self.handles,
            &mut self.staged_allocations,
            name,
            queries,
            move |inv: RawSystemInvocation<'_>| {
                let mut ctx = SystemContext {
                    iter: GuestChunkIter::with_bridges(
                        inv.iter,
                        inv.host_api,
                        inv.iter_reset,
                        inv.res_vtable,
                    ),
                    commands: GuestCommandQueue::with_ext(inv.commands, inv.cmd_vtable_ext),
                    host_api: inv.host_api,
                };
                system(&mut ctx)
            },
        )
    }

    /// Registers a panic-safe, leak-free Rust system closure gated by a `run_if` predicate closure.
    pub fn add_system_if<F, C>(
        &mut self,
        name: &CStr,
        queries: &[ComponentAccessDesc],
        run_if: C,
        mut system: F,
    ) -> FfiResult
    where
        C: FnMut() -> bool + Send + 'static,
        F: FnMut(&mut SystemContext<'_>) -> FfiResult + Send + 'static,
    {
        register_conditional_system_closure(
            self.handles,
            &mut self.staged_allocations,
            name,
            queries,
            run_if,
            move |inv: RawSystemInvocation<'_>| {
                let mut ctx = SystemContext {
                    iter: GuestChunkIter::with_bridges(
                        inv.iter,
                        inv.host_api,
                        inv.iter_reset,
                        inv.res_vtable,
                    ),
                    commands: GuestCommandQueue::with_ext(inv.commands, inv.cmd_vtable_ext),
                    host_api: inv.host_api,
                };
                system(&mut ctx)
            },
        )
    }

    /// Registers a multi-query Rust system closure (up to [`MAX_GUEST_MULTI_QUERIES`] query groups).
    pub fn add_multi_query_system<F>(
        &mut self,
        name: &CStr,
        query_groups: &[&[ComponentAccessDesc]],
        mut system: F,
    ) -> FfiResult
    where
        F: FnMut(&mut MultiQueryContext<'_>) -> FfiResult + Send + 'static,
    {
        register_multi_query_closure(
            self.handles,
            &mut self.staged_allocations,
            name,
            query_groups,
            move |inv: RawMultiQueryInvocation<'_>| {
                let mut iter_array: [Option<GuestChunkIter<'_>>; MAX_GUEST_MULTI_QUERIES] =
                    core::array::from_fn(|_| None);
                for (slot, item) in iter_array.iter_mut().enumerate().take(inv.iter_count) {
                    *item = Some(GuestChunkIter::for_multi_query(
                        inv.raw_iters[slot],
                        inv.host_api,
                        inv.iter_reset,
                    ));
                }
                let mut ctx = MultiQueryContext {
                    iters: iter_array,
                    iter_count: inv.iter_count,
                    commands: GuestCommandQueue::with_ext(inv.commands, inv.cmd_vtable_ext),
                    host_api: inv.host_api,
                    res_vtable: inv.res_vtable,
                };
                system(&mut ctx)
            },
        )
    }

    /// Registers a multi-query Rust system closure gated by a `run_if` predicate closure.
    pub fn add_multi_query_system_if<F, C>(
        &mut self,
        name: &CStr,
        query_groups: &[&[ComponentAccessDesc]],
        run_if: C,
        mut system: F,
    ) -> FfiResult
    where
        C: FnMut() -> bool + Send + 'static,
        F: FnMut(&mut MultiQueryContext<'_>) -> FfiResult + Send + 'static,
    {
        register_conditional_multi_query_closure(
            self.handles,
            &mut self.staged_allocations,
            name,
            query_groups,
            run_if,
            move |inv: RawMultiQueryInvocation<'_>| {
                let mut iter_array: [Option<GuestChunkIter<'_>>; MAX_GUEST_MULTI_QUERIES] =
                    core::array::from_fn(|_| None);
                for (slot, item) in iter_array.iter_mut().enumerate().take(inv.iter_count) {
                    *item = Some(GuestChunkIter::for_multi_query(
                        inv.raw_iters[slot],
                        inv.host_api,
                        inv.iter_reset,
                    ));
                }
                let mut ctx = MultiQueryContext {
                    iters: iter_array,
                    iter_count: inv.iter_count,
                    commands: GuestCommandQueue::with_ext(inv.commands, inv.cmd_vtable_ext),
                    host_api: inv.host_api,
                    res_vtable: inv.res_vtable,
                };
                system(&mut ctx)
            },
        )
    }

    /// Commits all staged allocations from this initialization session to the guest lifecycle
    /// registry so they remain alive until `bevy_abi_mod_shutdown`.
    pub fn commit(mut self) {
        commit_allocations(&mut self.staged_allocations);
    }
}

impl Drop for GuestApp<'_> {
    fn drop(&mut self) {
        if !self.staged_allocations.is_empty() {
            let aborted = core::mem::take(&mut self.staged_allocations);
            abort_allocations(aborted);
        }
    }
}

/// Executes a [`GuestApp`] setup function inside [`catch_guest_panic`], committing allocations
/// on [`FfiResult::OK`] or automatically rolling them back on error/panic.
///
/// # Safety
/// Must be called with valid handshake arguments supplied to `bevy_abi_mod_init`.
pub unsafe fn run_guest_app_init<F>(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
    build_fn: F,
) -> FfiResult
where
    F: FnOnce(&mut GuestApp<'_>) -> FfiResult,
{
    catch_guest_panic(|| {
        // SAFETY: Forwarding C-ABI `bevy_abi_mod_init` handshake arguments validated by `from_raw`.
        let mut app = match unsafe { GuestApp::from_raw(get_proc_address, host_api, host_context) }
        {
            Ok(app) => app,
            Err(err) => return err,
        };
        let status = build_fn(&mut app);
        if status.is_ok() {
            app.commit();
        }
        status
    })
}

/// Convenient prelude module for guest mod authors.
pub mod prelude {
    pub use crate::{
        AbiPod, Chunk, ChunkSlice, ChunkSliceMut, CommandBridgeVTable, CommandBridgeVTableExt,
        GuestApp, GuestChunkIter, GuestCommandQueue, GuestComponent, GuestPlugin,
        MAX_GUEST_MULTI_QUERIES, MultiQueryContext, ResourceBridgeVTable, SystemContext,
        catch_guest_panic, cleanup_registered_guest_state, export_mod, register_runtime_component,
        resolve_iter_reset, resolve_register_conditional_multi_query,
        resolve_register_conditional_system, resolve_register_multi_query, run_guest_app_init,
        wrap_multi_query_callback, wrap_run_condition, wrap_system_callback,
    };
    pub use bevy_abi_mod_sys::{
        ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH, CmdDespawnFn,
        CmdInsertComponentFn, CmdInsertDefaultFn, CmdRemoveComponentFn, CmdSpawnEmptyFn,
        ComponentAccessDesc, ComponentAccessMode, ComponentDropFn, ComponentUuid,
        ConditionalMultiQuerySystemDescriptor, ConditionalSystemDescriptor,
        DynamicComponentDescriptor, DynamicSystemCallback, ExposableComponent, ExtensibleStruct,
        FFI_BOOL_FALSE, FFI_BOOL_TRUE, FfiBool, FfiEntity, FfiResult, FfiSlice, GetProcAddressFn,
        HostApi, IterResetFn, IterResourceMutableFn, IterResourceReadonlyFn,
        ModComponentDescriptor, MultiQuerySystemCallback, MultiQuerySystemDescriptor, QuerySpec,
        RawChunkIter, RawCommandQueue, RegisterComponentFn, RegisterConditionalMultiQuerySystemFn,
        RegisterConditionalSystemFn, RegisterMultiQuerySystemFn, RegisterSystemFn,
        RunConditionCallback, ScheduledMultiQuerySystemDescriptor, ScheduledSystemDescriptor,
        StrView, SymbolEntry, SystemDescriptor, ffi_bool_to_rust, rust_bool_to_ffi,
    };
}

#[cfg(test)]
#[allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment,
    clippy::transmute_ptr_to_ptr,
    unreachable_pub
)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    unsafe fn panicking_system(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        panic!("mod crash");
    }

    unsafe fn healthy_system(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    wrap_system_callback!(fn c_panicking_system calls panicking_system);
    wrap_system_callback!(fn c_healthy_system calls healthy_system);

    #[test]
    fn test_guest_panic_containment_returns_panic_caught() {
        let status = unsafe {
            c_panicking_system(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(status, FfiResult::PANIC_CAUGHT);

        let ok_status = unsafe {
            c_healthy_system(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(ok_status, FfiResult::OK);
    }

    #[test]
    fn test_chunk_slice_and_mut_bounds() {
        let mut data = [10u32, 20u32, 30u32, 40u32];
        let stride = core::mem::size_of::<u32>();
        let count = data.len();

        let slice = ChunkSlice::new(data.as_ptr() as *const u8, stride, count);
        assert_eq!(slice.count(), 4);
        assert_eq!(slice.stride(), 4);
        assert!(!slice.is_empty());

        let p0 = slice.get_ptr(0).unwrap();
        assert_eq!(unsafe { *(p0 as *const u32) }, 10);

        let p2 = slice.get_ptr(2).unwrap();
        assert_eq!(unsafe { *(p2 as *const u32) }, 30);

        assert!(slice.get_ptr(4).is_none());
        assert!(slice.get_ptr(100).is_none());

        let massive_slice = ChunkSlice::new(data.as_ptr() as *const u8, stride, usize::MAX);
        assert!(massive_slice.get_ptr(usize::MAX / 2).is_none());

        let mut slice_mut = ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, count);
        let p1 = slice_mut.get_ptr_mut(1).unwrap();
        unsafe { *(p1 as *mut u32) = 99 };
        assert_eq!(data[1], 99);
        assert!(slice_mut.get_ptr_mut(4).is_none());

        let mut massive_slice_mut =
            ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, usize::MAX);
        assert!(massive_slice_mut.get_ptr_mut(usize::MAX / 2).is_none());
    }

    #[derive(Copy, Clone, Debug, PartialEq)]
    #[repr(C)]
    struct PosData {
        x: f32,
        y: f32,
    }

    #[test]
    fn test_chunk_slice_as_typed_slice() {
        let data = [PosData { x: 1.0, y: 2.0 }, PosData { x: 3.0, y: 4.0 }];
        let stride = core::mem::size_of::<PosData>();
        let slice = ChunkSlice::new(data.as_ptr() as *const u8, stride, 2);

        let typed = slice
            .as_typed_slice::<PosData>()
            .expect("valid typed slice");
        assert_eq!(typed.len(), 2);
        assert_eq!(typed[0], PosData { x: 1.0, y: 2.0 });
        assert_eq!(typed[1], PosData { x: 3.0, y: 4.0 });

        let wrong_stride = ChunkSlice::new(data.as_ptr() as *const u8, stride + 4, 2);
        assert!(wrong_stride.as_typed_slice::<PosData>().is_none());

        let misaligned_ptr = unsafe { (data.as_ptr() as *const u8).add(1) };
        let misaligned = ChunkSlice::new(misaligned_ptr, stride, 1);
        assert!(misaligned.as_typed_slice::<PosData>().is_none());

        let empty = ChunkSlice::new(core::ptr::null(), stride, 0);
        assert_eq!(empty.as_typed_slice::<PosData>(), Some(&[][..]));

        let empty_bad_stride = ChunkSlice::new(core::ptr::null(), stride + 1, 0);
        assert!(empty_bad_stride.as_typed_slice::<PosData>().is_none());

        let massive = ChunkSlice::new(data.as_ptr() as *const u8, stride, usize::MAX);
        assert!(massive.as_typed_slice::<PosData>().is_none());
    }

    #[test]
    fn test_chunk_slice_mut_as_typed_slice() {
        let mut data = [PosData { x: 1.0, y: 2.0 }, PosData { x: 3.0, y: 4.0 }];
        let stride = core::mem::size_of::<PosData>();
        let mut slice_mut = ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, 2);

        {
            let typed_mut = slice_mut
                .as_typed_slice::<PosData>()
                .expect("valid typed slice mut");
            typed_mut[0].x = 10.0;
            typed_mut[1].y = 40.0;
        }

        assert_eq!(data[0], PosData { x: 10.0, y: 2.0 });
        assert_eq!(data[1], PosData { x: 3.0, y: 40.0 });

        let mut bad_stride_slice = ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride + 4, 2);
        assert!(bad_stride_slice.as_typed_slice::<PosData>().is_none());

        let mut empty = ChunkSliceMut::new(core::ptr::null_mut(), stride, 0);
        assert_eq!(empty.as_typed_slice::<PosData>(), Some(&mut [][..]));

        let mut massive_mut = ChunkSliceMut::new(data.as_mut_ptr() as *mut u8, stride, usize::MAX);
        assert!(massive_mut.as_typed_slice::<PosData>().is_none());
    }

    #[test]
    fn test_guest_command_queue_insert_runtime_component() {
        static CAPTURED_LEN: AtomicUsize = AtomicUsize::new(0);
        static CAPTURED_ENTITY: AtomicU64 = AtomicU64::new(0);

        unsafe extern "C" fn mock_insert(
            _queue: *mut RawCommandQueue,
            entity: FfiEntity,
            _name: *const c_char,
            _data: *const u8,
            len: usize,
        ) -> FfiResult {
            CAPTURED_LEN.store(len, Ordering::SeqCst);
            CAPTURED_ENTITY.store(entity.0, Ordering::SeqCst);
            FfiResult::OK
        }

        let mut queue = GuestCommandQueue::new(
            core::ptr::without_provenance_mut(0x1234),
            CommandBridgeVTable {
                spawn_empty: None,
                despawn: None,
                insert_component: Some(mock_insert),
            },
        );

        let pos = PosData { x: 5.0, y: 7.0 };
        let res = queue.insert_runtime_component(FfiEntity(42), c"PosData", &pos);
        assert_eq!(res, FfiResult::OK);
        assert_eq!(
            CAPTURED_LEN.load(Ordering::SeqCst),
            core::mem::size_of::<PosData>()
        );
        assert_eq!(CAPTURED_ENTITY.load(Ordering::SeqCst), 42);
    }

    #[test]
    fn test_register_runtime_component_mock() {
        static CAPTURED_NAME: Mutex<Option<std::string::String>> = Mutex::new(None);
        static CAPTURED_SIZE: AtomicUsize = AtomicUsize::new(0);
        static CAPTURED_ALIGN: AtomicUsize = AtomicUsize::new(0);
        static CAPTURED_DROP: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);

        unsafe extern "C" fn mock_register_component(
            _ctx: *mut c_void,
            desc: *const ModComponentDescriptor,
        ) -> FfiResult {
            assert!(!desc.is_null());
            let d = unsafe { &*desc };
            *CAPTURED_NAME.lock().unwrap() = Some(
                unsafe { CStr::from_ptr(d.name) }
                    .to_str()
                    .unwrap()
                    .to_string(),
            );
            CAPTURED_SIZE.store(d.size, Ordering::SeqCst);
            CAPTURED_ALIGN.store(d.align, Ordering::SeqCst);
            CAPTURED_DROP.store(d.drop_fn.is_some(), Ordering::SeqCst);
            FfiResult::OK
        }

        unsafe extern "C" fn stub_register_system(
            _: *mut c_void,
            _: *const bevy_abi_mod_sys::SystemDescriptor,
        ) -> FfiResult {
            FfiResult::OK
        }
        unsafe extern "C" fn stub_iter_next_chunk(_: *mut RawChunkIter) -> usize {
            0
        }
        unsafe extern "C" fn stub_iter_column_readonly(
            _: *const RawChunkIter,
            _: usize,
        ) -> *const u8 {
            core::ptr::null()
        }
        unsafe extern "C" fn stub_iter_column_mutable(_: *mut RawChunkIter, _: usize) -> *mut u8 {
            core::ptr::null_mut()
        }
        unsafe extern "C" fn stub_iter_mark_changed(
            _: *mut RawChunkIter,
            _: usize,
            _: usize,
        ) -> FfiResult {
            FfiResult::OK
        }
        unsafe extern "C" fn stub_iter_entities(
            _: *const RawChunkIter,
        ) -> *const bevy_abi_mod_sys::FfiEntity {
            core::ptr::null()
        }
        unsafe extern "C" fn stub_iter_column_stride(_: *const RawChunkIter, _: usize) -> usize {
            0
        }

        let dummy_host_api = HostApi {
            struct_size: core::mem::size_of::<HostApi>() as u32,
            abi_version: bevy_abi_mod_sys::ABI_VERSION_PACKED,
            register_system: stub_register_system,
            iter_next_chunk: stub_iter_next_chunk,
            iter_column_readonly: stub_iter_column_readonly,
            iter_column_mutable: stub_iter_column_mutable,
            iter_mark_changed: stub_iter_mark_changed,
            iter_entities: stub_iter_entities,
            iter_column_stride: stub_iter_column_stride,
            register_component: mock_register_component,
        };

        let dummy_ctx = core::ptr::without_provenance_mut(0x1234);

        let res = unsafe {
            register_runtime_component::<PosData>(&dummy_host_api, dummy_ctx, c"PosData")
        };
        assert_eq!(res, FfiResult::OK);
        assert_eq!(CAPTURED_NAME.lock().unwrap().as_deref(), Some("PosData"));
        assert_eq!(
            CAPTURED_SIZE.load(Ordering::SeqCst),
            core::mem::size_of::<PosData>()
        );
        assert_eq!(
            CAPTURED_ALIGN.load(Ordering::SeqCst),
            core::mem::align_of::<PosData>()
        );
        assert!(!CAPTURED_DROP.load(Ordering::SeqCst));

        #[allow(dead_code)]
        struct HasDrop(std::string::String);
        let res_drop = unsafe {
            register_runtime_component::<HasDrop>(&dummy_host_api, dummy_ctx, c"HasDrop")
        };
        assert_eq!(res_drop, FfiResult::OK);
        assert_eq!(CAPTURED_NAME.lock().unwrap().as_deref(), Some("HasDrop"));
        assert_eq!(
            CAPTURED_SIZE.load(Ordering::SeqCst),
            core::mem::size_of::<HasDrop>()
        );
        assert_eq!(
            CAPTURED_ALIGN.load(Ordering::SeqCst),
            core::mem::align_of::<HasDrop>()
        );
        assert!(CAPTURED_DROP.load(Ordering::SeqCst));

        assert_eq!(
            unsafe {
                register_runtime_component::<PosData>(core::ptr::null(), dummy_ctx, c"PosData")
            },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe {
                register_runtime_component::<PosData>(
                    &dummy_host_api,
                    core::ptr::null_mut(),
                    c"PosData",
                )
            },
            FfiResult::NULL_POINTER
        );
    }

    unsafe impl GuestComponent for PosData {
        const NAME: &'static CStr = c"PosData";
    }

    #[test]
    fn test_phase3_phase4_guest_component_descriptors_and_commands() {
        assert_eq!(
            PosData::optional_read_desc().mode,
            ComponentAccessMode::OPTIONAL_READ
        );
        assert_eq!(
            PosData::optional_write_desc().mode,
            ComponentAccessMode::OPTIONAL_WRITE
        );
        assert_eq!(PosData::changed_desc().mode, ComponentAccessMode::CHANGED);
        assert_eq!(PosData::added_desc().mode, ComponentAccessMode::ADDED);
        assert_eq!(
            PosData::resource_read_desc().mode,
            ComponentAccessMode::RESOURCE_READ
        );
        assert_eq!(
            PosData::resource_write_desc().mode,
            ComponentAccessMode::RESOURCE_WRITE
        );

        static REMOVED_ENT: AtomicU64 = AtomicU64::new(0);
        static DEFAULT_ENT: AtomicU64 = AtomicU64::new(0);

        unsafe extern "C" fn mock_remove(
            _q: *mut RawCommandQueue,
            entity: FfiEntity,
            _name: *const c_char,
        ) -> FfiResult {
            REMOVED_ENT.store(entity.0, Ordering::SeqCst);
            FfiResult::OK
        }

        unsafe extern "C" fn mock_insert_default(
            _q: *mut RawCommandQueue,
            entity: FfiEntity,
            _name: *const c_char,
        ) -> FfiResult {
            DEFAULT_ENT.store(entity.0, Ordering::SeqCst);
            FfiResult::OK
        }

        let ext = CommandBridgeVTableExt {
            base: CommandBridgeVTable::default(),
            remove_component: Some(mock_remove),
            insert_default: Some(mock_insert_default),
        };
        let mut queue = GuestCommandQueue::with_ext(core::ptr::without_provenance_mut(0x9999), ext);
        assert_eq!(
            queue.remove_guest_component::<PosData>(FfiEntity(77)),
            FfiResult::OK
        );
        assert_eq!(REMOVED_ENT.load(Ordering::SeqCst), 77);
        assert_eq!(
            queue.insert_guest_default::<PosData>(FfiEntity(88)),
            FfiResult::OK
        );
        assert_eq!(DEFAULT_ENT.load(Ordering::SeqCst), 88);
    }

    unsafe fn panicking_cond(_ud: *mut c_void) -> bool {
        panic!("cond panic");
    }
    unsafe fn true_cond(_ud: *mut c_void) -> bool {
        true
    }
    wrap_run_condition!(fn c_panicking_cond calls panicking_cond);
    wrap_run_condition!(fn c_true_cond calls true_cond);

    #[test]
    fn test_wrap_run_condition_panic_containment() {
        assert_eq!(
            unsafe { c_panicking_cond(core::ptr::null_mut()) },
            FFI_BOOL_FALSE
        );
        assert_eq!(unsafe { c_true_cond(core::ptr::null_mut()) }, FFI_BOOL_TRUE);
    }

    #[test]
    fn test_multi_query_context_blocks_iter_resource_aliasing_and_resolves_group1_resource() {
        use core::sync::atomic::AtomicPtr;

        static SHARED_RES: Mutex<PosData> = Mutex::new(PosData { x: 10.0, y: 20.0 });
        static CAPTURED_CB: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
        static CAPTURED_UD: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

        const GROUP0_SENTINEL: usize = 0xA000;
        const GROUP1_SENTINEL: usize = 0xB000;

        unsafe extern "C" fn mock_res_readonly(
            iter: *const RawChunkIter,
            _name: *const c_char,
        ) -> *const u8 {
            if iter as usize == GROUP1_SENTINEL {
                let guard = SHARED_RES.lock().unwrap();
                (&*guard as *const PosData).cast::<u8>()
            } else {
                core::ptr::null()
            }
        }

        unsafe extern "C" fn mock_res_mutable(
            iter: *mut RawChunkIter,
            _name: *const c_char,
        ) -> *mut u8 {
            if iter as usize == GROUP1_SENTINEL {
                let mut guard = SHARED_RES.lock().unwrap();
                (&mut *guard as *mut PosData).cast::<u8>()
            } else {
                core::ptr::null_mut()
            }
        }

        unsafe extern "C" fn mock_reg_multi(
            _ctx: *mut c_void,
            desc: *const MultiQuerySystemDescriptor,
        ) -> FfiResult {
            let d = unsafe { &*desc };
            CAPTURED_CB.store(d.callback as *mut c_void, Ordering::SeqCst);
            CAPTURED_UD.store(d.user_data, Ordering::SeqCst);
            FfiResult::OK
        }

        unsafe extern "C" fn mock_get_proc_address(symbol_name: *const c_char) -> *const c_void {
            let name = unsafe { CStr::from_ptr(symbol_name) }.to_str().unwrap();
            match name {
                "register_multi_query_system" => mock_reg_multi as *const c_void,
                "res_readonly" => mock_res_readonly as *const c_void,
                "res_mutable" => mock_res_mutable as *const c_void,
                _ => core::ptr::null(),
            }
        }

        unsafe extern "C" fn stub_reg_sys(_: *mut c_void, _: *const SystemDescriptor) -> FfiResult {
            FfiResult::OK
        }
        unsafe extern "C" fn stub_next(_: *mut RawChunkIter) -> usize {
            0
        }
        unsafe extern "C" fn stub_ro(_: *const RawChunkIter, _: usize) -> *const u8 {
            core::ptr::null()
        }
        unsafe extern "C" fn stub_rw(_: *mut RawChunkIter, _: usize) -> *mut u8 {
            core::ptr::null_mut()
        }
        unsafe extern "C" fn stub_mark(_: *mut RawChunkIter, _: usize, _: usize) -> FfiResult {
            FfiResult::OK
        }
        unsafe extern "C" fn stub_ents(_: *const RawChunkIter) -> *const FfiEntity {
            core::ptr::null()
        }
        unsafe extern "C" fn stub_stride(_: *const RawChunkIter, _: usize) -> usize {
            0
        }
        unsafe extern "C" fn stub_reg_comp(
            _: *mut c_void,
            _: *const ModComponentDescriptor,
        ) -> FfiResult {
            FfiResult::OK
        }

        let host_api = HostApi {
            struct_size: core::mem::size_of::<HostApi>() as u32,
            abi_version: bevy_abi_mod_sys::ABI_VERSION_PACKED,
            register_system: stub_reg_sys,
            iter_next_chunk: stub_next,
            iter_column_readonly: stub_ro,
            iter_column_mutable: stub_rw,
            iter_mark_changed: stub_mark,
            iter_entities: stub_ents,
            iter_column_stride: stub_stride,
            register_component: stub_reg_comp,
        };

        *SHARED_RES.lock().unwrap() = PosData { x: 10.0, y: 20.0 };

        let mut app = unsafe {
            GuestApp::from_raw(
                mock_get_proc_address,
                &host_api as *const HostApi,
                core::ptr::without_provenance_mut(0x5555),
            )
            .expect("GuestApp::from_raw")
        };
        let captured_res_vt = app.resource_vtable();

        let q0_terms = [PosData::read_desc()];
        let q1_terms = [PosData::resource_write_desc()];
        let groups: [&[ComponentAccessDesc]; 2] = [&q0_terms, &q1_terms];

        assert_eq!(
            app.add_multi_query_system(c"mq_res_safety", &groups, move |ctx| {
                {
                    let (q0, q1) = ctx.iters_2_mut(0, 1).unwrap();
                    q0.set_resource_vtable(captured_res_vt);
                    q1.set_resource_vtable(captured_res_vt);
                    assert!(q0.resource::<PosData>(c"PosData").is_none());
                    assert!(q1.resource::<PosData>(c"PosData").is_none());
                    assert!(q0.resource_mut::<PosData>(c"PosData").is_none());
                    assert!(q1.resource_mut::<PosData>(c"PosData").is_none());
                }

                assert_eq!(ctx.guest_resource::<PosData>().unwrap().x, 10.0);
                let res_mut = ctx.guest_resource_mut::<PosData>().unwrap();
                res_mut.x += 5.0;
                res_mut.y += 7.0;
                FfiResult::OK
            }),
            FfiResult::OK
        );
        app.commit();

        let cb_ptr = CAPTURED_CB.load(Ordering::SeqCst);
        let ud_ptr = CAPTURED_UD.load(Ordering::SeqCst);
        assert!(!cb_ptr.is_null());
        let cb: MultiQuerySystemCallback = unsafe { core::mem::transmute(cb_ptr) };
        let raw_iters = [
            core::ptr::without_provenance_mut(GROUP0_SENTINEL),
            core::ptr::without_provenance_mut(GROUP1_SENTINEL),
        ];
        let status = unsafe { cb(raw_iters.as_ptr(), 2, core::ptr::null_mut(), ud_ptr) };
        assert_eq!(status, FfiResult::OK);
        assert_eq!(*SHARED_RES.lock().unwrap(), PosData { x: 15.0, y: 27.0 });

        cleanup_registered_guest_state();
    }
}
