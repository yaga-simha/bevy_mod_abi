//! Per-instance cached [`Host`] function table, load-context validation, and `"bam.v0.*"` / VTable
//! symbol resolution.

use core::ffi::{CStr, c_void};

use bevy_abi_mod_sys::{
    ABI_VERSION, AbiPod, BamComponent, BamHostApi, BamHostCtx, BamLoadCtx, ComponentUuid,
    FfiResult, VTableHeader, read_descriptor,
};

use crate::{
    component::register_pod_component,
    instance::GuestInstance,
    trampoline::{
        PreparedRunIf, SystemRegistrationConfig, ValidatedSystemInvocation, register_system_closure,
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
