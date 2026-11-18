//! Per-World [`GuestInstance`] state container and `bevy_abi_mod_init` / `bevy_abi_mod_shutdown`
//! lifecycle helpers with zero process-global mutable state.

use core::ffi::{CStr, c_void};

use bevy_abi_mod_sys::{
    BamHostApi, BamInstance, BamLoadCtx, BamManifest, BamStateWriterFn, FfiResult,
};

use crate::{
    host::{Host, ValidatedLoadCtx},
    panic::{catch_guest_result, catch_guest_unit},
    trampoline::ReentrantCell,
};

type SaveStateHook = Box<dyn FnMut() -> Result<Vec<u8>, FfiResult> + Send + 'static>;
type RestoreStateHook = Box<dyn FnMut(&[u8]) -> FfiResult + Send + 'static>;

struct ErasedAllocation {
    ptr: *mut c_void,
    drop_fn: unsafe fn(*mut c_void),
}

// SAFETY: `ErasedAllocation` is constructed exclusively from `Box<T>` where `T: Send + 'static`.
unsafe impl Send for ErasedAllocation {}

impl Drop for ErasedAllocation {
    fn drop(&mut self) {
        let ptr = self.ptr;
        let drop_fn = self.drop_fn;
        catch_guest_unit(move || {
            // SAFETY: `ptr` was allocated via `Box::into_raw` of the exact `T` paired with
            // `drop_fn` and is dropped exactly once here when `ErasedAllocation` drops.
            unsafe { drop_fn(ptr) };
        });
    }
}

unsafe fn drop_boxed<T>(ptr: *mut c_void) {
    // SAFETY: Caller guarantees `ptr` came from `Box::into_raw` for `T` and has not been freed.
    drop(unsafe { Box::from_raw(ptr.cast::<T>()) });
}

/// Thread-safe append-only tracker for runtime observer/closure allocations created after
/// `bevy_abi_mod_init` (such as `ValidatedCommands::observe`), owned by [`GuestInstance`] and
/// drained in LIFO order during `bevy_abi_mod_shutdown` prior to init-time allocations.
pub(crate) struct RuntimeAllocTracker {
    allocations: std::sync::Mutex<Vec<ErasedAllocation>>,
}

impl RuntimeAllocTracker {
    fn new() -> Self {
        Self {
            allocations: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn track<T: Send + 'static>(&self, boxed: Box<T>) -> *mut T {
        let raw = Box::into_raw(boxed);
        let mut guard = self
            .allocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.push(ErasedAllocation {
            ptr: raw.cast::<c_void>(),
            drop_fn: drop_boxed::<T>,
        });
        raw
    }

    fn len(&self) -> usize {
        self.allocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn drain_lifo(&self) {
        let mut drained = {
            let mut guard = self
                .allocations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            core::mem::take(&mut *guard)
        };
        while let Some(alloc) = drained.pop() {
            drop(alloc);
        }
    }
}

struct OwnedManifestBytes {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: `OwnedManifestBytes` uniquely owns a heap-allocated immutable UTF-8 `[u8]` buffer (when
// `len > 0`) created via `Box::into_raw` and freed exclusively in `OwnedManifestBytes::drop`.
unsafe impl Send for OwnedManifestBytes {}

impl OwnedManifestBytes {
    fn new(name: String) -> Self {
        let len = name.len();
        if len == 0 {
            Self {
                ptr: core::ptr::NonNull::<u8>::dangling().as_ptr(),
                len: 0,
            }
        } else {
            let boxed: Box<[u8]> = name.into_bytes().into_boxed_slice();
            Self {
                ptr: Box::into_raw(boxed).cast::<u8>(),
                len,
            }
        }
    }
}

impl Drop for OwnedManifestBytes {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: `self.ptr` was allocated via `Box::into_raw` of a `Box<[u8]>` of length
            // `self.len` in `OwnedManifestBytes::new` and all `Host` copies inside `GuestInstance`
            // closures/hooks have already finished dropping before this field drops.
            drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(self.ptr, self.len)) });
        }
    }
}

/// Heap-allocated per-World guest mod instance returned via `*instance_out` from
/// `bevy_abi_mod_init` and reclaimed in `bevy_abi_mod_shutdown`.
///
/// Owns the validated [`Host`] table, all registered system/`run_if` closure states, owned
/// descriptor strings, hot-reload state hooks, and per-instance shutdown hooks so loading
/// the same `.cdylib` into multiple Bevy `World`s simultaneously shares zero mutable global state.
pub struct GuestInstance {
    host: Host,
    allocations: Vec<ErasedAllocation>,
    shutdown_hooks: Vec<Box<dyn FnOnce() + Send + 'static>>,
    save_state_hook: Option<ReentrantCell<SaveStateHook>>,
    restore_state_hook: Option<ReentrantCell<RestoreStateHook>>,
    // Declared last so field drop runs strictly after `GuestInstance::drop` tears down every
    // closure/hook holding a copy of `host`.
    _runtime_allocs: Box<RuntimeAllocTracker>,
    _manifest_bytes: OwnedManifestBytes,
}

impl GuestInstance {
    /// Creates an empty [`GuestInstance`] bound to `host` and `manifest_name`.
    pub fn new(host: Host, manifest_name: impl Into<String>) -> Self {
        let manifest_bytes = OwnedManifestBytes::new(manifest_name.into());
        let runtime_allocs = Box::new(RuntimeAllocTracker::new());
        // SAFETY: When `manifest_bytes.len > 0`, `manifest_bytes.ptr` points to immutable, valid
        // UTF-8 heap bytes that remain pinned at stable provenance until `_manifest_bytes` drops
        // after all `shutdown_hooks`, `_runtime_allocs`, and `allocations`. Similarly,
        // `runtime_allocs` stays heap-pinned until after all closures holding `host` finish dropping.
        let host = unsafe {
            host.with_manifest_name(manifest_bytes.ptr.cast_const(), manifest_bytes.len)
                .with_runtime_allocs(core::ptr::from_ref(&*runtime_allocs))
        };
        Self {
            host,
            allocations: Vec::new(),
            shutdown_hooks: Vec::new(),
            save_state_hook: None,
            restore_state_hook: None,
            _runtime_allocs: runtime_allocs,
            _manifest_bytes: manifest_bytes,
        }
    }

    /// Returns the cached [`Host`] table for this mod instance.
    #[inline]
    pub const fn host(&self) -> &Host {
        &self.host
    }

    /// Returns the canonical mod name declared in [`BamManifest::name`].
    #[inline]
    pub fn manifest_name(&self) -> &str {
        self.host.manifest_name()
    }

    /// Transfers ownership of `boxed` into this instance, returning a stable raw pointer valid
    /// until `bevy_abi_mod_shutdown` (or until rollback if `bevy_abi_mod_init` fails).
    pub fn track_allocation<T: Send + 'static>(&mut self, boxed: Box<T>) -> *mut T {
        let raw = Box::into_raw(boxed);
        self.allocations.push(ErasedAllocation {
            ptr: raw.cast::<c_void>(),
            drop_fn: drop_boxed::<T>,
        });
        raw
    }

    /// Registers a per-instance shutdown hook executed in reverse registration (LIFO) order during
    /// `bevy_abi_mod_shutdown` before closure allocations are freed.
    ///
    /// Shutdown hooks registered during a failed `bevy_abi_mod_init` are discarded without running
    /// during transactional rollback.
    pub fn add_shutdown_hook<F>(&mut self, hook: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.shutdown_hooks.push(Box::new(hook));
    }

    /// Registers or replaces the hot-reload state snapshot callback for this instance.
    pub fn set_save_state_hook<F>(&mut self, hook: F)
    where
        F: FnMut() -> Result<Vec<u8>, FfiResult> + Send + 'static,
    {
        self.save_state_hook = Some(ReentrantCell::new(Box::new(hook)));
    }

    /// Registers or replaces the hot-reload state restoration callback for this instance.
    pub fn set_restore_state_hook<F>(&mut self, hook: F)
    where
        F: FnMut(&[u8]) -> FfiResult + Send + 'static,
    {
        self.restore_state_hook = Some(ReentrantCell::new(Box::new(hook)));
    }

    /// Returns the number of tracked heap allocations owned by this instance.
    #[inline]
    pub fn allocation_count(&self) -> usize {
        self.allocations.len() + self._runtime_allocs.len()
    }

    /// Invokes all registered per-instance shutdown hooks in LIFO order, catching any panics.
    pub fn run_shutdown_hooks(&mut self) {
        while let Some(hook) = self.shutdown_hooks.pop() {
            catch_guest_unit(hook);
        }
    }
}

impl Drop for GuestInstance {
    fn drop(&mut self) {
        // Drop tracked allocations in reverse (LIFO) registration order so later closures or
        // descriptors drop before earlier dependencies. Unrun `shutdown_hooks` (from failed init
        // rollback) are dropped without invoking them.
        self.shutdown_hooks.clear();
        self.save_state_hook = None;
        self.restore_state_hook = None;
        self._runtime_allocs.drain_lifo();
        while let Some(alloc) = self.allocations.pop() {
            drop(alloc);
        }
    }
}

/// Panic-safe `bevy_abi_mod_init` driver constructing a per-World [`GuestInstance`] and writing it
/// to `*instance_out` on [`FfiResult::OK`].
///
/// If `api` or `manifest` fails validation, `build_fn` returns a non-`OK` [`FfiResult`], or
/// `build_fn` panics, all staged allocations inside [`GuestInstance`] are immediately rolled back
/// on the guest heap and `*instance_out` remains `null`.
///
/// # Safety
/// - If `instance_out` is non-null, it must point to writable `*mut BamInstance` storage.
/// - `api` and `load_ctx` must satisfy [`Host::from_load_ctx`] when non-null.
/// - `manifest.name` and `manifest.version`, when non-null, must point to NUL-terminated C strings.
pub unsafe fn init_guest_instance<F>(
    api: *const BamHostApi,
    load_ctx: *mut BamLoadCtx,
    instance_out: *mut *mut BamInstance,
    manifest: &BamManifest,
    build_fn: F,
) -> FfiResult
where
    F: FnOnce(&mut GuestInstance, ValidatedLoadCtx<'_>) -> FfiResult,
{
    catch_guest_result(|| {
        if instance_out.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: Verified `instance_out` is non-null and writable per caller contract; initialize
        // to null upfront so any early error or panic leaves `*instance_out == null`.
        unsafe {
            *instance_out = core::ptr::null_mut();
        }

        let manifest_status = manifest.validate();
        if !manifest_status.is_ok() {
            return manifest_status;
        }
        // SAFETY: `manifest.validate()` confirmed `manifest.name` is non-null, and caller guarantees
        // it points to a NUL-terminated C string.
        let Ok(manifest_name) = unsafe { CStr::from_ptr(manifest.name) }.to_str() else {
            return FfiResult::INVALID_UTF8;
        };
        if manifest_name.is_empty() {
            return FfiResult::INVALID_ARGUMENT;
        }

        // SAFETY: Forwarding `api` and `load_ctx` under caller's `bevy_abi_mod_init` contract.
        let host = match unsafe { Host::from_load_ctx(api, load_ctx) } {
            Ok(h) => h,
            Err(err) => return err,
        };

        let mut instance = GuestInstance::new(host, manifest_name);
        let host_copy = *instance.host();
        // SAFETY: `load_ctx` was verified non-null in `Host::from_load_ctx` and remains valid for
        // the duration of `init_guest_instance`.
        let validated_load = match unsafe { ValidatedLoadCtx::from_raw(load_ctx, &host_copy) } {
            Ok(ctx) => ctx,
            Err(err) => return err,
        };

        let status = build_fn(&mut instance, validated_load);
        if !status.is_ok() {
            drop(instance);
            return status;
        }

        let raw_instance = Box::into_raw(Box::new(instance)).cast::<BamInstance>();
        // SAFETY: `instance_out` is non-null and writable; transfers ownership to the host until
        // `shutdown_guest_instance` is called with `raw_instance`.
        unsafe {
            *instance_out = raw_instance;
        }
        FfiResult::OK
    })
}

/// Panic-safe `bevy_abi_mod_shutdown` driver executing per-instance shutdown hooks in LIFO order
/// and freeing the [`GuestInstance`] returned by [`init_guest_instance`].
///
/// Passing a `null` `instance` pointer is a defined no-op.
///
/// # Safety
/// If `instance` is non-null, it must have been produced by [`init_guest_instance`] in this guest
/// binary and must not have been shut down previously or remain referenced by running systems.
pub unsafe fn shutdown_guest_instance(instance: *mut BamInstance) {
    if instance.is_null() {
        return;
    }
    catch_guest_unit(|| {
        // SAFETY: Caller guarantees `instance` was allocated via `Box::into_raw(Box::new(GuestInstance))`
        // in `init_guest_instance` and is uniquely reclaimed here.
        let mut boxed = unsafe { Box::from_raw(instance.cast::<GuestInstance>()) };
        boxed.run_shutdown_hooks();
        drop(boxed);
    });
}

/// Panic-safe `BamSaveStateFn` trampoline exporting guest state into the host-owned writer.
///
/// # Safety
/// - When `instance` is non-null, it must point to a live [`GuestInstance`] allocated by
///   [`init_guest_instance`] in this guest binary.
/// - `writer` and `writer_ctx` must satisfy [`BamStateWriterFn`] for the duration of this call.
pub unsafe extern "C" fn guest_save_state_trampoline(
    instance: *mut BamInstance,
    writer: Option<BamStateWriterFn>,
    writer_ctx: *mut c_void,
) -> FfiResult {
    catch_guest_result(|| {
        let Some(writer_fn) = writer else {
            return FfiResult::NULL_POINTER;
        };
        if instance.is_null() {
            return FfiResult::OK;
        }
        // SAFETY: `instance` points to a live `GuestInstance` prior to `bevy_abi_mod_shutdown`.
        let inst = unsafe { &*instance.cast::<GuestInstance>() };
        let Some(ref hook_cell) = inst.save_state_hook else {
            return FfiResult::OK;
        };
        let Some(blob_res) = hook_cell.try_with_mut(|hook| hook()) else {
            return FfiResult::ACCESS_CONFLICT;
        };
        let bytes = match blob_res {
            Ok(b) => b,
            Err(err) => return err,
        };
        if bytes.is_empty() {
            return FfiResult::OK;
        }
        // SAFETY: `writer_fn` copies `(bytes.as_ptr(), bytes.len())` into host-owned storage before
        // returning; `bytes` is dropped on the guest heap immediately afterward.
        unsafe { writer_fn(writer_ctx, bytes.as_ptr(), bytes.len()) }
    })
}

/// Panic-safe `BamRestoreStateFn` trampoline restoring host-buffered state into a newly initialized
/// [`GuestInstance`].
///
/// # Safety
/// - When `instance` is non-null, it must point to a live [`GuestInstance`] allocated by
///   [`init_guest_instance`] in this guest binary.
/// - When `len > 0`, `bytes` must be non-null and point to `len` initialized readable bytes.
pub unsafe extern "C" fn guest_restore_state_trampoline(
    instance: *mut BamInstance,
    bytes: *const u8,
    len: usize,
) -> FfiResult {
    catch_guest_result(|| {
        if len > 0 && bytes.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if instance.is_null() {
            return FfiResult::OK;
        }
        // SAFETY: `instance` points to a live `GuestInstance` after `bevy_abi_mod_init`.
        let inst = unsafe { &*instance.cast::<GuestInstance>() };
        let Some(ref hook_cell) = inst.restore_state_hook else {
            return FfiResult::OK;
        };
        let slice = if len == 0 {
            &[]
        } else {
            // SAFETY: When `len > 0`, `bytes` is non-null and valid for `len` bytes per caller contract.
            unsafe { core::slice::from_raw_parts(bytes, len) }
        };
        let Some(status) = hook_cell.try_with_mut(|hook| hook(slice)) else {
            return FfiResult::ACCESS_CONFLICT;
        };
        status
    })
}
