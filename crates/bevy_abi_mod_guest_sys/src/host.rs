//! Per-instance cached [`Host`] function table, load-context validation, and `"bam.v0.*"` / VTable
//! symbol resolution.

use core::ffi::{CStr, c_void};

use bevy_abi_mod_sys::{
    ABI_VERSION, AbiPod, BamAssetGetLoadStateFn, BamAssetLoadFn, BamAssetReleaseFn, BamComponent,
    BamEventKind, BamHostApi, BamHostCtx, BamLoadCtx, ComponentUuid, FfiAssetHandle,
    FfiAssetLoadState, FfiResult, MappedStringVTable, MappedWireVTable, StrView, VTableHeader,
    read_descriptor,
};

use crate::{
    component::{
        PodComponentHooks, register_component_with_layout, register_pod_component,
        register_pod_component_with_hooks,
    },
    instance::{GuestInstance, RuntimeAllocTracker},
    trampoline::{
        PreparedRunIf, SystemRegistrationConfig, ValidatedSystemInvocation,
        register_lifecycle_observer_closure, register_observer_closure,
        register_observer_closure_with_options, register_observer_mut_closure,
        register_system_closure,
    },
};

/// Required namespace prefix for custom game symbols looked up via `BamHostApi::get_proc_address`.
pub const CUSTOM_SYMBOL_PREFIX: &[u8] = b"bam.v0.";

#[derive(Clone, Copy, Debug)]
struct HostCtxPtr(*const BamHostCtx);

// SAFETY: `BamHostCtx` is an opaque world-scoped token managed by the host whose lifetime spans
// from `bevy_abi_mod_init` until `bevy_abi_mod_shutdown` for that mod instance. Passing this
// pointer back to host FFI functions from worker threads is synchronized by the host registry and
// Bevy's scheduler invariants.
unsafe impl Send for HostCtxPtr {}
// SAFETY: Shared references to `HostCtxPtr` only expose `*const BamHostCtx` for thread-safe host
// symbol/component queries and diagnostics.
unsafe impl Sync for HostCtxPtr {}

#[derive(Clone, Copy, Debug)]
struct ManifestNamePtr(*const u8);

// SAFETY: Points to immutable UTF-8 bytes owned by `GuestInstance` (or `NonNull::dangling()` when
// `manifest_name_len == 0`) that remain valid and unmutated until after all closures and hooks in
// `GuestInstance` finish dropping.
unsafe impl Send for ManifestNamePtr {}
// SAFETY: Shared read-only UTF-8 byte pointer safe to read concurrently across worker threads.
unsafe impl Sync for ManifestNamePtr {}

#[derive(Clone, Copy, Debug)]
struct RuntimeAllocsPtr(*const RuntimeAllocTracker);

// SAFETY: When non-null, points to a `Mutex`-synchronized `RuntimeAllocTracker` pinned on the heap
// inside `GuestInstance` that outlives all closures/commands holding `Host`.
unsafe impl Send for RuntimeAllocsPtr {}
// SAFETY: `RuntimeAllocTracker` uses internal `Mutex` synchronization for concurrent `track` calls.
unsafe impl Sync for RuntimeAllocsPtr {}

/// Validated wrapper around a host-exported [`MappedWireVTable<W>`] whose [`VTableHeader`] and
/// [`ComponentUuid`] were verified at symbol resolution time.
pub struct ValidatedWireVTable<W: AbiPod> {
    pub(crate) raw: MappedWireVTable<W>,
}

impl<W: AbiPod> Copy for ValidatedWireVTable<W> {}

impl<W: AbiPod> Clone for ValidatedWireVTable<W> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<W: AbiPod> ValidatedWireVTable<W> {
    /// Verified [`ComponentUuid`] recorded in the VTable header.
    #[inline]
    pub const fn uuid(&self) -> ComponentUuid {
        self.raw.header.uuid
    }

    /// Host-side `size_of::<T>()` recorded in the VTable header.
    #[inline]
    pub const fn host_size(&self) -> usize {
        self.raw.header.size
    }

    /// Host-side `align_of::<T>()` recorded in the VTable header.
    #[inline]
    pub const fn host_align(&self) -> usize {
        self.raw.header.align
    }

    /// Returns `true` if this bridged type supports mutable `set` writes.
    #[inline]
    pub const fn supports_set(&self) -> bool {
        self.raw.set.is_some()
    }
}

/// Validated wrapper around a host-exported [`MappedStringVTable`] whose [`VTableHeader`] and
/// [`ComponentUuid`] were verified at symbol resolution time.
#[derive(Clone, Copy)]
pub struct ValidatedStringVTable {
    pub(crate) raw: MappedStringVTable,
}

impl ValidatedStringVTable {
    /// Verified [`ComponentUuid`] recorded in the VTable header.
    #[inline]
    pub const fn uuid(&self) -> ComponentUuid {
        self.raw.header.uuid
    }

    /// Host-side `size_of::<T>()` recorded in the VTable header.
    #[inline]
    pub const fn host_size(&self) -> usize {
        self.raw.header.size
    }

    /// Host-side `align_of::<T>()` recorded in the VTable header.
    #[inline]
    pub const fn host_align(&self) -> usize {
        self.raw.header.align
    }

    /// Returns `true` if this bridged string type supports mutable `set` writes.
    #[inline]
    pub const fn supports_set(&self) -> bool {
        self.raw.set.is_some()
    }
}

/// Validated, per-instance cached copy of [`BamHostApi`] paired with its world-scoped
/// [`BamHostCtx`] token and optional owning manifest name.
///
/// Constructed once per mod load session during `bevy_abi_mod_init` by copying up to
/// `min(api.struct_size, size_of::<BamHostApi>())` bytes (`>= BamHostApi::MIN_V0_2_SIZE`) and
/// verifying ABI SemVer compatibility plus non-nullness of all 27 core ABI `0.2` slots.
#[derive(Clone, Copy, Debug)]
pub struct Host {
    api: BamHostApi,
    host_ctx: HostCtxPtr,
    manifest_name_ptr: ManifestNamePtr,
    manifest_name_len: usize,
    runtime_allocs_ptr: RuntimeAllocsPtr,
}

impl Host {
    /// Copies and validates `api_ptr` against [`ABI_VERSION`], binding it to `host_ctx`.
    ///
    /// # Safety
    /// - If `api_ptr` is non-null and aligned, it must be readable for at least `size_of::<u32>()`
    ///   bytes at offset `0`, and if `struct_size >= BamHostApi::MIN_V0_2_SIZE`, readable for
    ///   `min(struct_size, size_of::<BamHostApi>())` initialized bytes.
    /// - If `host_ctx` is non-null, it must point to an active [`BamHostCtx`] valid until the
    ///   corresponding `bevy_abi_mod_shutdown` call.
    pub unsafe fn from_raw(
        api_ptr: *const BamHostApi,
        host_ctx: *const BamHostCtx,
    ) -> Result<Self, FfiResult> {
        if api_ptr.is_null() || host_ctx.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        if !(api_ptr as usize).is_multiple_of(core::mem::align_of::<BamHostApi>()) {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        // SAFETY: `api_ptr` is non-null, aligned, and readable per caller contract.
        let api = unsafe { read_descriptor::<BamHostApi>(api_ptr.cast::<c_void>()) }?;
        let status = api.validate_for_guest(ABI_VERSION);
        if !status.is_ok() {
            return Err(status);
        }
        Ok(Self {
            api,
            host_ctx: HostCtxPtr(host_ctx),
            manifest_name_ptr: ManifestNamePtr(core::ptr::NonNull::<u8>::dangling().as_ptr()),
            manifest_name_len: 0,
            runtime_allocs_ptr: RuntimeAllocsPtr(core::ptr::null()),
        })
    }

    /// Copies and validates `api_ptr` and extracts the world-scoped [`BamHostCtx`] from `load_ctx`.
    ///
    /// # Safety
    /// - `api_ptr` must satisfy the safety requirements of [`Self::from_raw`].
    /// - If `load_ctx` is non-null, it must point to the active [`BamLoadCtx`] supplied to
    ///   `bevy_abi_mod_init`.
    pub unsafe fn from_load_ctx(
        api_ptr: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
    ) -> Result<Self, FfiResult> {
        if api_ptr.is_null() || load_ctx.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        if !(api_ptr as usize).is_multiple_of(core::mem::align_of::<BamHostApi>()) {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        // SAFETY: `api_ptr` is non-null, aligned, and readable per caller contract.
        let api = unsafe { read_descriptor::<BamHostApi>(api_ptr.cast::<c_void>()) }?;
        let status = api.validate_for_guest(ABI_VERSION);
        if !status.is_ok() {
            return Err(status);
        }
        let Some(load_host_ctx_fn) = api.load_host_ctx else {
            return Err(FfiResult::NULL_POINTER);
        };
        // SAFETY: `load_ctx` is non-null and active for `bevy_abi_mod_init` per caller contract.
        let host_ctx = unsafe { load_host_ctx_fn(load_ctx.cast_const()) };
        if host_ctx.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        Ok(Self {
            api,
            host_ctx: HostCtxPtr(host_ctx),
            manifest_name_ptr: ManifestNamePtr(core::ptr::NonNull::<u8>::dangling().as_ptr()),
            manifest_name_len: 0,
            runtime_allocs_ptr: RuntimeAllocsPtr(core::ptr::null()),
        })
    }

    /// Returns a copy of this [`Host`] stamped with the owning [`GuestInstance`]'s UTF-8
    /// `manifest_name` (`ptr`, `len`).
    ///
    /// # Safety
    /// If `len > 0`, `ptr` must be non-null and point to `len` immutable, valid UTF-8 bytes that
    /// outlive every copy of the returned [`Host`].
    #[inline]
    pub const unsafe fn with_manifest_name(mut self, ptr: *const u8, len: usize) -> Self {
        self.manifest_name_ptr = ManifestNamePtr(ptr);
        self.manifest_name_len = len;
        self
    }

    /// Returns a copy of this [`Host`] updated to reference `host_ctx` for the active invocation.
    ///
    /// # Safety
    /// If `host_ctx` is non-null, it must point to an active [`BamHostCtx`] valid for the
    /// lifetime of every copy of the returned [`Host`].
    #[inline]
    pub const unsafe fn with_host_ctx_ptr(mut self, host_ctx: *const BamHostCtx) -> Self {
        self.host_ctx = HostCtxPtr(host_ctx);
        self
    }

    /// Binds a heap-pinned [`RuntimeAllocTracker`] owned by [`GuestInstance`] so runtime
    /// `ValidatedCommands::observe*` closures are tracked and freed cleanly on shutdown.
    ///
    /// # Safety
    /// If `ptr` is non-null, it must point to a valid [`RuntimeAllocTracker`] that outlives every
    /// copy of the returned [`Host`].
    #[inline]
    pub(crate) const unsafe fn with_runtime_allocs(
        mut self,
        ptr: *const RuntimeAllocTracker,
    ) -> Self {
        self.runtime_allocs_ptr = RuntimeAllocsPtr(ptr);
        self
    }

    #[inline]
    pub(crate) fn track_runtime_allocation<T: Send + 'static>(
        &self,
        boxed: Box<T>,
    ) -> Option<*mut T> {
        if self.runtime_allocs_ptr.0.is_null() {
            return None;
        }
        // SAFETY: `with_runtime_allocs` guarantees `runtime_allocs_ptr.0` points to a live
        // `RuntimeAllocTracker` inside `GuestInstance` outliving `&self`.
        Some(unsafe { (&*self.runtime_allocs_ptr.0).track(boxed) })
    }

    /// Returns the owning mod instance's manifest name (`""` when constructed standalone via
    /// [`Self::from_raw`] without a [`GuestInstance`]).
    #[inline]
    pub const fn manifest_name(&self) -> &str {
        if self.manifest_name_len == 0 {
            return "";
        }
        // SAFETY: `with_manifest_name` guarantees that when `manifest_name_len > 0`,
        // `manifest_name_ptr.0` points to `manifest_name_len` immutable, valid UTF-8 bytes
        // outliving `&self`.
        unsafe {
            let bytes =
                core::slice::from_raw_parts(self.manifest_name_ptr.0, self.manifest_name_len);
            core::str::from_utf8_unchecked(bytes)
        }
    }

    /// Returns a shared reference to the validated [`BamHostApi`] table.
    #[inline]
    pub const fn api(&self) -> &BamHostApi {
        &self.api
    }

    /// Returns the world-scoped [`BamHostCtx`] token bound to this instance.
    #[inline]
    pub const fn host_ctx(&self) -> *const BamHostCtx {
        self.host_ctx.0
    }

    /// Reads the most recent human-readable UTF-8 diagnostic message recorded on this instance's
    /// [`BamHostCtx`], returning an empty string if no error is recorded.
    pub fn last_error(&self) -> String {
        let Some(last_error_fn) = self.api.last_error else {
            return String::new();
        };
        // SAFETY: `self.host_ctx.0` is a valid active `BamHostCtx`, and `(null, 0)` queries the
        // required byte length under the `snprintf` sizing contract without writing to `buf`.
        let byte_len = unsafe { last_error_fn(self.host_ctx.0, core::ptr::null_mut(), 0) };
        if byte_len == 0 {
            return String::new();
        }
        let Some(cap) = byte_len.checked_add(1) else {
            return String::new();
        };
        let mut buf = vec![0u8; cap];
        // SAFETY: `buf` provides `cap` writable bytes on the guest heap and `self.host_ctx.0` is valid.
        let reported = unsafe { last_error_fn(self.host_ctx.0, buf.as_mut_ptr().cast(), cap) };
        let valid_len = reported.min(byte_len);
        buf.truncate(valid_len);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Queries a host-exported VTable or `"bam.v0.*"` custom symbol pointer via
    /// `BamHostApi::get_proc_address`.
    ///
    /// Core engine operations live directly in [`BamHostApi`] slots and are never looked up by
    /// string name.
    #[inline]
    pub fn get_proc_address(&self, symbol: &CStr) -> *const c_void {
        let Some(get_proc_fn) = self.api.get_proc_address else {
            return core::ptr::null();
        };
        // SAFETY: `self.host_ctx.0` is an active `BamHostCtx` and `symbol` is a valid NUL-terminated CStr.
        unsafe { get_proc_fn(self.host_ctx.0, symbol.as_ptr()) }
    }

    /// Resolves a custom game symbol strictly namespaced under `"bam.v0.<name>"`.
    ///
    /// Rejects symbols that do not start with `"bam.v0."` with [`FfiResult::INVALID_ARGUMENT`],
    /// and returns [`FfiResult::NOT_FOUND`] when the host does not export `symbol`.
    #[inline]
    pub fn resolve_custom_symbol(&self, symbol: &CStr) -> Result<*const c_void, FfiResult> {
        if !symbol.to_bytes().starts_with(CUSTOM_SYMBOL_PREFIX) {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let ptr = self.get_proc_address(symbol);
        if ptr.is_null() {
            Err(FfiResult::NOT_FOUND)
        } else {
            Ok(ptr)
        }
    }

    /// Resolves a custom `"bam.v0.<name>"` symbol and casts it to `extern "C"` function pointer `F`.
    ///
    /// # Safety
    /// `F` must be an `extern "C"` function pointer type matching the exact signature and calling
    /// convention exported by the host under `symbol`.
    pub unsafe fn resolve_custom_fn<F: Copy>(&self, symbol: &CStr) -> Result<F, FfiResult> {
        if core::mem::size_of::<F>() != core::mem::size_of::<*const c_void>()
            || core::mem::align_of::<F>() != core::mem::align_of::<*const c_void>()
        {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let ptr = self.resolve_custom_symbol(symbol)?;
        // SAFETY: Verified `F` has pointer size and alignment, `ptr` is non-null, and the caller
        // guarantees `F` matches the host's `extern "C"` symbol signature.
        Ok(unsafe { core::ptr::read((&raw const ptr).cast::<F>()) })
    }

    /// Resolves a static component VTable exported under `symbol`, validating its leading
    /// [`VTableHeader`] (`struct_size >= size_of::<V>()`, compatible `abi_version`, and
    /// `header.uuid == expected_uuid`) before forming `&V`.
    ///
    /// # Safety
    /// - `V` must be a `#[repr(C)]` VTable struct prefixed at byte offset `0` by [`VTableHeader`].
    /// - Any symbol returned by the host for `symbol` must remain valid for lifetime `'a` and have
    ///   initialized VTable bytes matching `V`'s fields when the [`VTableHeader`] check succeeds.
    pub unsafe fn resolve_vtable<'a, V: Copy + 'static>(
        &self,
        symbol: &CStr,
        expected_uuid: ComponentUuid,
    ) -> Result<&'a V, FfiResult> {
        if core::mem::size_of::<V>() < VTableHeader::MIN_V0_2_SIZE
            || core::mem::align_of::<V>() < core::mem::align_of::<VTableHeader>()
        {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let raw = self.get_proc_address(symbol);
        if raw.is_null() {
            return Err(FfiResult::NOT_FOUND);
        }
        if !(raw as usize).is_multiple_of(core::mem::align_of::<V>()) {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        // SAFETY: `raw` is non-null, aligned to `V` (>= `VTableHeader` alignment), and points to a
        // `VTableHeader`-prefixed static VTable allocation per caller contract; `read_descriptor`
        // inspects `struct_size` first without forming `&V` over potentially shorter memory.
        let header = unsafe { read_descriptor::<VTableHeader>(raw) }?;
        let status = header.validate(core::mem::size_of::<V>(), expected_uuid);
        if !status.is_ok() {
            return Err(status);
        }
        // SAFETY: `header.validate` proved `struct_size >= size_of::<V>()`, ABI compatibility, and
        // exact `expected_uuid` identity; `raw` is aligned for `V` and valid for `'a`.
        Ok(unsafe { &*raw.cast::<V>() })
    }

    /// Resolves a host-bridged [`MappedWireVTable<W>`] under `"bam.v0.<name>.vtable"` and verifies
    /// its [`VTableHeader`] and `expected_uuid`.
    pub fn resolve_mapped_wire_vtable<W: AbiPod>(
        &self,
        symbol: &CStr,
        expected_uuid: ComponentUuid,
    ) -> Result<ValidatedWireVTable<W>, FfiResult> {
        if !symbol.to_bytes().starts_with(CUSTOM_SYMBOL_PREFIX) {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        // SAFETY: `MappedWireVTable<W>` is `#[repr(C)]` prefixed at byte offset 0 by `VTableHeader`,
        // and the host only exports static `MappedWireVTable` symbols matching `expected_uuid`.
        let vt = unsafe { self.resolve_vtable::<MappedWireVTable<W>>(symbol, expected_uuid) }?;
        if vt.get.is_none() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        Ok(ValidatedWireVTable { raw: *vt })
    }

    /// Resolves a host-bridged [`MappedStringVTable`] under `"bam.v0.<name>.vtable"` and verifies
    /// its [`VTableHeader`] and `expected_uuid`.
    pub fn resolve_mapped_string_vtable(
        &self,
        symbol: &CStr,
        expected_uuid: ComponentUuid,
    ) -> Result<ValidatedStringVTable, FfiResult> {
        if !symbol.to_bytes().starts_with(CUSTOM_SYMBOL_PREFIX) {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        // SAFETY: `MappedStringVTable` is `#[repr(C)]` prefixed at byte offset 0 by `VTableHeader`,
        // and the host only exports static `MappedStringVTable` symbols matching `expected_uuid`.
        let vt = unsafe { self.resolve_vtable::<MappedStringVTable>(symbol, expected_uuid) }?;
        if vt.get.is_none() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        Ok(ValidatedStringVTable { raw: *vt })
    }

    /// Initiates an asynchronous typed or untyped (`asset_type == BamComponent::INVALID`) asset
    /// load via `BamHostApi::asset_load`, returning a strong [`FfiAssetHandle`] tracked by the
    /// host for this mod instance.
    pub fn asset_load(
        &self,
        asset_type: BamComponent,
        path: &str,
    ) -> Result<FfiAssetHandle, FfiResult> {
        if path.is_empty() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        if !self.api.covers_slot(
            core::mem::offset_of!(BamHostApi, asset_load),
            core::mem::size_of::<Option<BamAssetLoadFn>>(),
        ) {
            return Err(FfiResult::UNSUPPORTED);
        }
        let Some(load_fn) = self.api.asset_load else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let mut out = FfiAssetHandle::NONE;
        // SAFETY: `self.host_ctx.0` is an active `BamHostCtx`, `StrView::from_str(path)` borrows
        // valid UTF-8 for this call, and `out` is aligned writable stack storage.
        let status = unsafe {
            load_fn(
                self.host_ctx.0,
                asset_type,
                StrView::from_str(path),
                &mut out,
            )
        };
        if status.is_ok() { Ok(out) } else { Err(status) }
    }

    /// Queries the current [`FfiAssetLoadState`] of `handle` via `BamHostApi::asset_get_load_state`.
    pub fn asset_get_load_state(
        &self,
        handle: &FfiAssetHandle,
    ) -> Result<FfiAssetLoadState, FfiResult> {
        let validation = handle.validate();
        if !validation.is_ok() {
            return Err(validation);
        }
        if handle.is_none() {
            return Ok(FfiAssetLoadState::NOT_LOADED);
        }
        if !self.api.covers_slot(
            core::mem::offset_of!(BamHostApi, asset_get_load_state),
            core::mem::size_of::<Option<BamAssetGetLoadStateFn>>(),
        ) {
            return Err(FfiResult::UNSUPPORTED);
        }
        let Some(state_fn) = self.api.asset_get_load_state else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let mut out = FfiAssetLoadState::NOT_LOADED;
        // SAFETY: `self.host_ctx.0` is valid and `handle`/`&mut out` are aligned stack references.
        let status = unsafe { state_fn(self.host_ctx.0, handle, &mut out) };
        if status.is_ok() { Ok(out) } else { Err(status) }
    }

    /// Releases a strong host-side asset pin previously acquired for `handle` via
    /// `BamHostApi::asset_release`.
    pub fn asset_release(&self, handle: &FfiAssetHandle) -> FfiResult {
        let validation = handle.validate();
        if !validation.is_ok() {
            return validation;
        }
        if handle.is_none() {
            return FfiResult::OK;
        }
        if !self.api.covers_slot(
            core::mem::offset_of!(BamHostApi, asset_release),
            core::mem::size_of::<Option<BamAssetReleaseFn>>(),
        ) {
            return FfiResult::UNSUPPORTED;
        }
        let Some(release_fn) = self.api.asset_release else {
            return FfiResult::UNSUPPORTED;
        };
        // SAFETY: `self.host_ctx.0` is valid and `handle` is an aligned shared reference.
        unsafe { release_fn(self.host_ctx.0, handle) }
    }
}

/// Validated init-only [`BamLoadCtx`] wrapper tied to the duration of `bevy_abi_mod_init`.
pub struct ValidatedLoadCtx<'a> {
    raw: *mut BamLoadCtx,
    host: &'a Host,
}

impl<'a> ValidatedLoadCtx<'a> {
    /// Wraps a raw `load_ctx` pointer validated by the caller for the duration of `'a`.
    ///
    /// # Safety
    /// `raw` must be a non-null, active [`BamLoadCtx`] belonging to the same load session as `host`
    /// and must not outlive `bevy_abi_mod_init`.
    #[inline]
    pub unsafe fn from_raw(raw: *mut BamLoadCtx, host: &'a Host) -> Result<Self, FfiResult> {
        if raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        Ok(Self { raw, host })
    }

    /// Returns the underlying `*mut BamLoadCtx` handle.
    #[inline]
    pub const fn as_raw(&self) -> *mut BamLoadCtx {
        self.raw
    }

    /// Returns the validated [`Host`] table bound to this load session.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.host
    }

    /// Registers a cross-language `T: AbiPod` dynamic component (`drop_fn = None`, `vtable_ptr = null`)
    /// on this validated load context, storing an owned copy of `name` inside `instance`.
    #[inline]
    pub fn register_pod_component<T: AbiPod>(
        &self,
        instance: &mut GuestInstance,
        name: &CStr,
        uuid: ComponentUuid,
        flags: u32,
    ) -> Result<BamComponent, FfiResult> {
        // SAFETY: `self.raw` was validated as an active `BamLoadCtx` in `ValidatedLoadCtx::from_raw`,
        // and `vtable_ptr` is null.
        unsafe {
            register_pod_component::<T>(instance, self.raw, name, uuid, flags, core::ptr::null())
        }
    }

    /// Registers a cross-language `T: AbiPod` dynamic component with optional `on_add` / `on_remove`
    /// hooks on this validated load context.
    #[inline]
    pub fn register_pod_component_with_hooks<T: AbiPod>(
        &self,
        instance: &mut GuestInstance,
        name: &CStr,
        uuid: ComponentUuid,
        flags: u32,
        hooks: PodComponentHooks<T>,
    ) -> Result<BamComponent, FfiResult> {
        // SAFETY: `self.raw` was validated as an active `BamLoadCtx` in `ValidatedLoadCtx::from_raw`,
        // and `vtable_ptr` is null.
        unsafe {
            register_pod_component_with_hooks::<T>(
                instance,
                self.raw,
                name,
                uuid,
                flags,
                core::ptr::null(),
                hooks,
            )
        }
    }

    /// Registers a runtime layout-defined dynamic component (`name`, `size`, `align`, `drop_fn`)
    /// backed by `ComponentDescriptor::new_with_layout` on the host.
    #[inline]
    pub fn register_component_with_layout(
        &self,
        instance: &mut GuestInstance,
        name: &str,
        size: usize,
        align: usize,
        drop_fn: Option<bevy_abi_mod_sys::ComponentDropFn>,
    ) -> Result<BamComponent, FfiResult> {
        // SAFETY: `self.raw` was validated in `ValidatedLoadCtx::from_raw`.
        unsafe { register_component_with_layout(instance, self.raw, name, size, align, drop_fn) }
    }

    /// Registers a dynamic Bevy ECS [`Observer`](bevy_abi_mod_sys::BamObserverDesc) callback on
    /// this validated load context, storing all owned state inside `instance`.
    #[inline]
    pub fn register_observer<E, F>(
        &self,
        instance: &mut GuestInstance,
        name: &str,
        event: BamComponent,
        target: bevy_abi_mod_sys::FfiEntity,
        callback: F,
    ) -> FfiResult
    where
        E: AbiPod,
        F: FnMut(
                bevy_abi_mod_sys::FfiEntity,
                &E,
                &mut crate::commands::ValidatedCommands<'_>,
            ) -> FfiResult
            + Send
            + 'static,
    {
        // SAFETY: `self.raw` was validated as an active `BamLoadCtx` in `ValidatedLoadCtx::from_raw`.
        unsafe {
            register_observer_closure::<E, F>(instance, self.raw, name, event, target, callback)
        }
    }

    /// Registers a custom [`BamObserverDesc`](bevy_abi_mod_sys::BamObserverDesc) callback with
    /// explicit propagation flags and optional `propagate_relationship` component handle.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn register_observer_with_options<E, F>(
        &self,
        instance: &mut GuestInstance,
        name: &str,
        event: BamComponent,
        target: bevy_abi_mod_sys::FfiEntity,
        flags: u32,
        propagate_relationship: BamComponent,
        callback: F,
    ) -> FfiResult
    where
        E: AbiPod,
        F: FnMut(
                bevy_abi_mod_sys::FfiEntity,
                &E,
                &mut crate::commands::ValidatedCommands<'_>,
            ) -> FfiResult
            + Send
            + 'static,
    {
        // SAFETY: `self.raw` was validated as an active `BamLoadCtx` in `ValidatedLoadCtx::from_raw`.
        unsafe {
            register_observer_closure_with_options::<E, F>(
                instance,
                self.raw,
                name,
                event,
                target,
                flags,
                propagate_relationship,
                callback,
            )
        }
    }

    /// Registers a mutable [`BamObserverDesc`](bevy_abi_mod_sys::BamObserverDesc) callback (`&mut E`
    /// plus `&mut bool` propagation flag) on this validated load context.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn register_observer_mut<E, F>(
        &self,
        instance: &mut GuestInstance,
        name: &str,
        event: BamComponent,
        target: bevy_abi_mod_sys::FfiEntity,
        flags: u32,
        propagate_relationship: BamComponent,
        callback: F,
    ) -> FfiResult
    where
        E: AbiPod,
        F: FnMut(
                bevy_abi_mod_sys::FfiEntity,
                &mut E,
                &mut bool,
                &mut crate::commands::ValidatedCommands<'_>,
            ) -> FfiResult
            + Send
            + 'static,
    {
        // SAFETY: `self.raw` was validated as an active `BamLoadCtx` in `ValidatedLoadCtx::from_raw`.
        unsafe {
            register_observer_mut_closure::<E, F>(
                instance,
                self.raw,
                name,
                event,
                target,
                flags,
                propagate_relationship,
                callback,
            )
        }
    }

    /// Registers a component lifecycle or scene `Ready` observer (`ADD`, `INSERT`, `DISCARD`,
    /// `REMOVE`, `DESPAWN`, `READY`) on this validated load context.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn register_lifecycle_observer<F>(
        &self,
        instance: &mut GuestInstance,
        name: &str,
        event_kind: BamEventKind,
        watched_components: &[BamComponent],
        target: bevy_abi_mod_sys::FfiEntity,
        flags: u32,
        propagate_relationship: BamComponent,
        callback: F,
    ) -> FfiResult
    where
        F: FnMut(
                bevy_abi_mod_sys::FfiEntity,
                &mut crate::commands::ValidatedCommands<'_>,
            ) -> FfiResult
            + Send
            + 'static,
    {
        // SAFETY: `self.raw` was validated as an active `BamLoadCtx` in `ValidatedLoadCtx::from_raw`.
        unsafe {
            register_lifecycle_observer_closure(
                instance,
                self.raw,
                name,
                event_kind,
                watched_components,
                target,
                flags,
                propagate_relationship,
                callback,
            )
        }
    }

    /// Registers a unified [`bevy_abi_mod_sys::BamSystemDesc`] on this validated load context,
    /// storing all closures and owned descriptor buffers inside `instance`.
    #[inline]
    pub fn register_system<F>(
        &self,
        instance: &mut GuestInstance,
        config: &SystemRegistrationConfig<'_>,
        run_if: Option<PreparedRunIf>,
        closure: F,
    ) -> FfiResult
    where
        F: FnMut(&mut ValidatedSystemInvocation<'_>) -> FfiResult + Send + 'static,
    {
        // SAFETY: `self.raw` was validated as an active `BamLoadCtx` in `ValidatedLoadCtx::from_raw`,
        // and `PreparedRunIf` encapsulates a valid `(BamRunIfFn, *mut c_void)` pair.
        unsafe {
            register_system_closure(
                instance,
                self.raw,
                config,
                run_if.map(PreparedRunIf::into_raw),
                closure,
            )
        }
    }
}
