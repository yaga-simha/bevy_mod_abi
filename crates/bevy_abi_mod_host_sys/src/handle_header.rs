//! `#[repr(C)]` magic + generation handle headers and `HostContext` (`BamHostCtx`).
//!
//! Every concrete host struct cast to/from an opaque C-ABI handle (`HostContext`, `LoadContext`,
//! `HostSystemCtx`, `HostIter`, and `HostCommands`) begins at byte offset `0` with [`HandleHeader`]:
//! - Distinct 32-bit magic constants catch cross-handle type confusion (e.g. passing `BamCommands*`
//!   to `iter_next_chunk`).
//! - Monotonic non-zero `generation` + `POISONED_MAGIC` written on `Drop` catch stale/dangling
//!   handles in debug builds and under `--features strict-ffi-checks` (and unconditionally on
//!   cold registration/lookup paths).

use crate::registry::RegistrySnapshot;
use bevy_abi_mod_sys::{BamHostCtx, FfiResult};
use core::cell::RefCell;
use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Magic constant stamped at offset `0` of [`HostContext`] (`BamHostCtx`, `"BAMH"`).
pub const HOST_CTX_MAGIC: u32 = 0x4241_4D48;

/// Magic constant stamped at offset `0` of [`crate::ffi::register::LoadContext`] (`BamLoadCtx`, `"BAML"`).
pub const LOAD_CTX_MAGIC: u32 = 0x4241_4D4C;

/// Magic constant stamped at offset `0` of [`crate::system::HostSystemCtx`] (`BamSystemCtx`, `"BAMS"`).
pub const SYS_CTX_MAGIC: u32 = 0x4241_4D53;

/// Magic constant stamped at offset `0` of [`crate::ffi::iter::HostIter`] (`BamIter`, `"BAMI"`).
pub const ITER_MAGIC: u32 = 0x4241_4D49;

/// Magic constant stamped at offset `0` of [`crate::ffi::commands::HostCommands`] (`BamCommands`, `"BAMC"`).
pub const COMMANDS_MAGIC: u32 = 0x4241_4D43;

/// Poisoned magic constant written into [`HandleHeader::magic`] during `Drop`.
pub const POISONED_MAGIC: u32 = 0xDEAD_BEEF;

/// Whether hot-path handle header checks (`HostIter`, `HostCommands`, `HostSystemCtx`) are enabled.
pub const STRICT_FFI_CHECKS: bool = cfg!(any(debug_assertions, feature = "strict-ffi-checks"));

static NEXT_HANDLE_GENERATION: AtomicU32 = AtomicU32::new(1);

#[inline]
fn next_non_zero_generation() -> u32 {
    loop {
        let next_gen = NEXT_HANDLE_GENERATION.fetch_add(1, Ordering::Relaxed);
        if next_gen != 0 {
            return next_gen;
        }
    }
}

/// Leading 8-byte `#[repr(C)]` validation header placed at byte offset `0` of every concrete host
/// handle struct (`HostContext`, `LoadContext`, `HostSystemCtx`, `HostIter`, `HostCommands`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandleHeader {
    /// Handle-kind discriminator (`*_MAGIC`, or [`POISONED_MAGIC`] after `Drop`).
    pub magic: u32,
    /// Monotonic non-zero handle generation (`0` after `Drop`).
    pub generation: u32,
}

impl HandleHeader {
    /// Creates an active [`HandleHeader`] with `magic` and a fresh non-zero generation.
    #[inline]
    pub fn new(magic: u32) -> Self {
        Self {
            magic,
            generation: next_non_zero_generation(),
        }
    }

    /// Creates a [`HandleHeader`] with `magic` and an explicit caller-supplied non-zero `generation`
    /// (for zero-atomic reuse of per-frame stack wrappers when `generation != 0`).
    #[inline]
    pub const fn with_generation(magic: u32, generation: u32) -> Self {
        Self { magic, generation }
    }

    /// Returns `true` if `self.magic == expected_magic` and `self.generation != 0`.
    #[inline]
    pub const fn matches(self, expected_magic: u32) -> bool {
        self.magic == expected_magic && self.generation != 0
    }

    /// Poisons this header in place (`magic = POISONED_MAGIC`, `generation = 0`).
    #[inline]
    pub fn poison(&mut self) {
        // Volatile writes prevent dead-store elimination when `Drop` runs right before stack frame exit,
        // ensuring debug / strict-ffi-checks unit tests observe poisoned headers reliably.
        // SAFETY: `&mut self.magic` and `&mut self.generation` are valid, aligned, exclusively owned fields.
        unsafe {
            core::ptr::write_volatile(&mut self.magic, POISONED_MAGIC);
            core::ptr::write_volatile(&mut self.generation, 0);
        }
    }
}

const _: () = {
    assert!(core::mem::size_of::<HandleHeader>() == 8);
    assert!(core::mem::align_of::<HandleHeader>() == 4);
};

/// Validates an opaque `*const` handle pointer and casts it to `&T`.
///
/// Always rejects null pointers (`FfiResult::NULL_POINTER`). When `always_check || STRICT_FFI_CHECKS`
/// is true, also checks pointer alignment for `T` and verifies the leading [`HandleHeader`] at byte
/// offset `0` matches `expected_magic` with a non-zero generation (`FfiResult::INVALID_STATE`).
///
/// # Safety
/// - `T` must be `#[repr(C)]` with a [`HandleHeader`] at byte offset `0`.
/// - When non-null (and header-valid if checked), `ptr` must point to a readable, initialized `T`
///   without active mutable aliases for lifetime `'a`.
#[inline]
pub(crate) unsafe fn validate_handle_ref<'a, T>(
    ptr: *const c_void,
    expected_magic: u32,
    always_check: bool,
) -> Result<&'a T, FfiResult> {
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if always_check || STRICT_FFI_CHECKS {
        let align = core::mem::align_of::<T>();
        if !(ptr as usize).is_multiple_of(align) {
            return Err(FfiResult::INVALID_STATE);
        }
        // SAFETY: Alignment verified above (`align_of::<T>() >= align_of::<HandleHeader>()` on all
        // `#[repr(C)]` handle structs starting with `HandleHeader`); caller guarantees readable header.
        let hdr = unsafe { core::ptr::read(ptr.cast::<HandleHeader>()) };
        if !hdr.matches(expected_magic) {
            return Err(FfiResult::INVALID_STATE);
        }
    }
    // SAFETY: Caller guarantees `ptr` points to a valid `T` for `'a`.
    Ok(unsafe { &*ptr.cast::<T>() })
}

/// Validates an opaque `*mut` handle pointer and casts it to `&mut T`.
///
/// # Safety
/// - `T` must be `#[repr(C)]` with a [`HandleHeader`] at byte offset `0`.
/// - When non-null (and header-valid if checked), `ptr` must point to an exclusively borrowed,
///   initialized `T` for lifetime `'a`.
#[inline]
pub(crate) unsafe fn validate_handle_mut<'a, T>(
    ptr: *mut c_void,
    expected_magic: u32,
    always_check: bool,
) -> Result<&'a mut T, FfiResult> {
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if always_check || STRICT_FFI_CHECKS {
        let align = core::mem::align_of::<T>();
        if !(ptr as usize).is_multiple_of(align) {
            return Err(FfiResult::INVALID_STATE);
        }
        // SAFETY: Alignment verified above; caller guarantees readable `HandleHeader` at offset 0.
        let hdr = unsafe { core::ptr::read(ptr.cast::<HandleHeader>()) };
        if !hdr.matches(expected_magic) {
            return Err(FfiResult::INVALID_STATE);
        }
    }
    // SAFETY: Caller guarantees `ptr` points to an exclusively borrowed valid `T` for `'a`.
    Ok(unsafe { &mut *ptr.cast::<T>() })
}

/// Concrete world-scoped registry and diagnostic context backing opaque [`BamHostCtx`].
///
/// Embedded inside `LoadContext` during `bevy_abi_mod_init` and inside `HostSystemCtx` during
/// `GuestSystem::run_unsafe`. Never shared concurrently across threads.
#[repr(C)]
pub struct HostContext {
    /// Validation header (`HOST_CTX_MAGIC`) at byte offset `0`.
    pub header: HandleHeader,
    snapshot: Arc<RegistrySnapshot>,
    default_namespace: Option<Arc<str>>,
    last_error: RefCell<String>,
}

const _: () = assert!(core::mem::offset_of!(HostContext, header) == 0);

impl HostContext {
    /// Creates a new [`HostContext`] over `snapshot` with a fresh diagnostic buffer.
    #[inline]
    pub fn new(snapshot: Arc<RegistrySnapshot>) -> Self {
        Self::with_error_buffer(snapshot, String::new())
    }

    /// Creates a new [`HostContext`] reusing an existing `String` capacity for zero-allocation diagnostics.
    #[inline]
    pub fn with_error_buffer(snapshot: Arc<RegistrySnapshot>, mut error_buf: String) -> Self {
        error_buf.clear();
        Self {
            header: HandleHeader::new(HOST_CTX_MAGIC),
            snapshot,
            default_namespace: None,
            last_error: RefCell::new(error_buf),
        }
    }

    /// Attaches a default module namespace (manifest name) used to qualify unqualified component names.
    #[inline]
    pub fn with_namespace(mut self, ns: Option<Arc<str>>) -> Self {
        self.default_namespace = ns;
        self
    }

    /// Returns the optional default module namespace attached to this context.
    #[inline]
    pub fn default_namespace(&self) -> Option<&str> {
        self.default_namespace.as_deref()
    }

    /// Borrows the current immutable [`RegistrySnapshot`].
    #[inline]
    pub fn snapshot(&self) -> &RegistrySnapshot {
        &self.snapshot
    }

    /// Clones the `Arc<RegistrySnapshot>` handle (`O(1)`).
    #[inline]
    pub fn snapshot_arc(&self) -> Arc<RegistrySnapshot> {
        Arc::clone(&self.snapshot)
    }

    /// Replaces the active [`RegistrySnapshot`] (used by `LoadContext` after dynamic component registration).
    #[inline]
    pub fn set_snapshot(&mut self, snapshot: Arc<RegistrySnapshot>) {
        self.snapshot = snapshot;
    }

    /// Records a human-readable UTF-8 diagnostic message on this context.
    #[inline]
    pub fn set_last_error(&self, message: impl AsRef<str>) {
        if let Ok(mut buf) = self.last_error.try_borrow_mut() {
            buf.clear();
            buf.push_str(message.as_ref());
        }
    }

    /// Returns a cloned copy of the most recent diagnostic message on this context.
    #[inline]
    pub fn last_error_message(&self) -> String {
        self.last_error
            .try_borrow()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Clears the diagnostic message buffer on this context.
    #[inline]
    pub fn clear_last_error(&self) {
        if let Ok(mut buf) = self.last_error.try_borrow_mut() {
            buf.clear();
        }
    }

    /// Extracts the owned diagnostic `String` buffer so `GuestSystem` can reuse its capacity across frames.
    #[inline]
    pub fn reclaim_error_buffer(&mut self) -> String {
        core::mem::take(self.last_error.get_mut())
    }

    /// Returns a const [`BamHostCtx`] pointer to `self`.
    #[inline]
    pub const fn as_ffi_ptr(&self) -> *const BamHostCtx {
        (self as *const Self).cast::<BamHostCtx>()
    }
}

impl Drop for HostContext {
    #[inline]
    fn drop(&mut self) {
        self.header.poison();
    }
}

/// Copies the most recent UTF-8 diagnostic message recorded on `ctx` into `buf[0..cap]`
/// (`snprintf` convention: NUL-terminated when `buf != null && cap > 0`, returning the full
/// message byte length excluding the trailing NUL).
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - If `buf` is non-null and `cap > 0`, `buf` must point to at least `cap` writable bytes.
pub unsafe extern "C" fn host_last_error(
    ctx: *const BamHostCtx,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    // SAFETY: Validating `ctx` against `HOST_CTX_MAGIC` unconditionally (`always_check = true`).
    let Ok(host_ctx) =
        (unsafe { validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true) })
    else {
        return 0;
    };

    let Ok(msg) = host_ctx.last_error.try_borrow() else {
        return 0;
    };
    let bytes = msg.as_bytes();
    let total_len = bytes.len();

    if !buf.is_null() && cap > 0 {
        let copy_len = total_len.min(cap - 1);
        if copy_len > 0 {
            // SAFETY: Caller guarantees `buf` points to at least `cap > copy_len` writable bytes,
            // and `bytes` lives inside `host_ctx.last_error` which cannot overlap `buf`.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), copy_len);
            }
        }
        // SAFETY: `copy_len <= cap - 1 < cap`, so `buf.add(copy_len)` is within writable bounds.
        unsafe {
            *buf.cast::<u8>().add(copy_len) = 0;
        }
    }

    total_len
}
