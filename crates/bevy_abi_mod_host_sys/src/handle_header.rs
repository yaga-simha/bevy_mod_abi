//! `#[repr(C)]` magic + generation handle headers and `HostContext` (`BamHostCtx`).
//!
//! Every concrete host struct cast to/from an opaque C-ABI handle (`HostContext`, `LoadContext`,
//! `HostSystemCtx`, `HostIter`, and `HostCommands`) begins at byte offset `0` with [`HandleHeader`]:
//! - Distinct 32-bit magic constants catch cross-handle type confusion (e.g. passing `BamCommands*`
//!   to `iter_next_chunk`).
//! - Monotonic non-zero `generation` + `POISONED_MAGIC` written on `Drop` catch stale/dangling
//!   handles in debug builds and under `--features strict-ffi-checks` (and unconditionally on
//!   cold registration/lookup paths).

use crate::ffi::commands::HostCommands;
use crate::ffi::iter::HostIter;
use crate::ffi::register::LoadContext;
use crate::registry::RegistrySnapshot;
use crate::system::HostSystemCtx;
use bevy_abi_mod_sys::{
    BamCommands, BamHostCtx, BamIter, BamLoadCtx, BamSystemCtx, FfiEntity, FfiResult,
};
use bevy_ecs::entity::Entity;
use bevy_ecs::ptr::PtrMut;
use core::alloc::Layout;
use core::cell::{RefCell, UnsafeCell};
use core::ffi::{CStr, c_char, c_void};
use core::num::NonZeroUsize;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Early-return helper for FFI functions returning [`FfiResult`].
macro_rules! ffi_try {
    ($expr:expr) => {
        match $expr {
            Ok(val) => val,
            Err(err) => return err,
        }
    };
}
pub(crate) use ffi_try;

/// Magic constant stamped at offset `0` of [`HostContext`] (`BamHostCtx`, `"BAMH"`).
pub const HOST_CTX_MAGIC: u32 = 0x4241_4D48;

/// Magic constant stamped at offset `0` of [`crate::LoadContext`] (`BamLoadCtx`, `"BAML"`).
pub const LOAD_CTX_MAGIC: u32 = 0x4241_4D4C;

/// Magic constant stamped at offset `0` of [`crate::system::HostSystemCtx`] (`BamSystemCtx`, `"BAMS"`).
pub const SYS_CTX_MAGIC: u32 = 0x4241_4D53;

/// Magic constant stamped at offset `0` of [`crate::HostIter`] (`BamIter`, `"BAMI"`).
pub const ITER_MAGIC: u32 = 0x4241_4D49;

/// Magic constant stamped at offset `0` of [`crate::HostCommands`] (`BamCommands`, `"BAMC"`).
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

/// Validates an opaque `*const BamHostCtx` (unconditionally checking magic/generation).
///
/// # Safety
/// When non-null and header-valid, `ptr` must point to a live [`HostContext`] for `'a`.
#[inline]
pub(crate) unsafe fn validate_host_ctx<'a>(
    ptr: *const BamHostCtx,
) -> Result<&'a HostContext, FfiResult> {
    // SAFETY: Caller upholds `validate_handle_ref` contract for `HostContext`.
    unsafe { validate_handle_ref(ptr.cast::<c_void>(), HOST_CTX_MAGIC, true) }
}

/// Validates an opaque `*const BamLoadCtx` (unconditionally checking magic/generation).
///
/// # Safety
/// When non-null and header-valid, `ptr` must point to a live [`LoadContext`] for `'a`.
#[inline]
pub(crate) unsafe fn validate_load_ctx_ref<'a>(
    ptr: *const BamLoadCtx,
) -> Result<&'a LoadContext, FfiResult> {
    // SAFETY: Caller upholds `validate_handle_ref` contract for `LoadContext`.
    unsafe { validate_handle_ref(ptr.cast::<c_void>(), LOAD_CTX_MAGIC, true) }
}

/// Validates an opaque `*mut BamLoadCtx` (unconditionally checking magic/generation).
///
/// # Safety
/// When non-null and header-valid, `ptr` must point to an exclusively borrowed [`LoadContext`] for `'a`.
#[inline]
pub(crate) unsafe fn validate_load_ctx_mut<'a>(
    ptr: *mut BamLoadCtx,
) -> Result<&'a mut LoadContext, FfiResult> {
    // SAFETY: Caller upholds `validate_handle_mut` contract for `LoadContext`.
    unsafe { validate_handle_mut(ptr.cast::<c_void>(), LOAD_CTX_MAGIC, true) }
}

/// Validates an opaque `*mut BamCommands` under `STRICT_FFI_CHECKS`.
///
/// # Safety
/// When non-null and header-valid, `ptr` must point to an exclusively borrowed [`HostCommands`] for `'a`.
#[inline]
pub(crate) unsafe fn validate_commands_mut<'a, 'w>(
    ptr: *mut BamCommands,
) -> Result<&'a mut HostCommands<'w>, FfiResult> {
    // SAFETY: Caller upholds `validate_handle_mut` contract for `HostCommands`.
    unsafe { validate_handle_mut(ptr.cast::<c_void>(), COMMANDS_MAGIC, false) }
}

/// Validates an opaque `*const BamIter` under `STRICT_FFI_CHECKS`.
///
/// # Safety
/// When non-null and header-valid, `ptr` must point to a live [`HostIter`] for `'a`.
#[inline]
pub(crate) unsafe fn validate_iter_ref<'a, 'w>(
    ptr: *const BamIter,
) -> Result<&'a HostIter<'w>, FfiResult> {
    // SAFETY: Caller upholds `validate_handle_ref` contract for `HostIter`.
    unsafe { validate_handle_ref(ptr.cast::<c_void>(), ITER_MAGIC, false) }
}

/// Validates an opaque `*mut BamIter` under `STRICT_FFI_CHECKS`.
///
/// # Safety
/// When non-null and header-valid, `ptr` must point to an exclusively borrowed [`HostIter`] for `'a`.
#[inline]
pub(crate) unsafe fn validate_iter_mut<'a, 'w>(
    ptr: *mut BamIter,
) -> Result<&'a mut HostIter<'w>, FfiResult> {
    // SAFETY: Caller upholds `validate_handle_mut` contract for `HostIter`.
    unsafe { validate_handle_mut(ptr.cast::<c_void>(), ITER_MAGIC, false) }
}

/// Validates an opaque `*const BamSystemCtx` under `STRICT_FFI_CHECKS`.
///
/// # Safety
/// When non-null and header-valid, `ptr` must point to a live [`HostSystemCtx`] for `'a`.
#[inline]
pub(crate) unsafe fn validate_sys_ctx_ref<'a, 'w>(
    ptr: *const BamSystemCtx,
) -> Result<&'a HostSystemCtx<'w>, FfiResult> {
    // SAFETY: Caller upholds `validate_handle_ref` contract for `HostSystemCtx`.
    unsafe { validate_handle_ref(ptr.cast::<c_void>(), SYS_CTX_MAGIC, false) }
}

/// Validates an opaque `*mut BamSystemCtx` under `STRICT_FFI_CHECKS`.
///
/// # Safety
/// When non-null and header-valid, `ptr` must point to an exclusively borrowed [`HostSystemCtx`] for `'a`.
#[inline]
pub(crate) unsafe fn validate_sys_ctx_mut<'a, 'w>(
    ptr: *mut BamSystemCtx,
) -> Result<&'a mut HostSystemCtx<'w>, FfiResult> {
    // SAFETY: Caller upholds `validate_handle_mut` contract for `HostSystemCtx`.
    unsafe { validate_handle_mut(ptr.cast::<c_void>(), SYS_CTX_MAGIC, false) }
}

/// Decodes a required non-null, non-placeholder [`FfiEntity`] into a Bevy [`Entity`].
#[inline]
pub(crate) fn decode_live_entity(entity: FfiEntity) -> Result<Entity, FfiResult> {
    if entity.is_null() {
        return Err(FfiResult::INVALID_ENTITY);
    }
    let Some(decoded) = Entity::try_from_bits(entity.to_bits()) else {
        return Err(FfiResult::INVALID_ENTITY);
    };
    if decoded == Entity::PLACEHOLDER {
        return Err(FfiResult::INVALID_ENTITY);
    }
    Ok(decoded)
}

/// Decodes an optional [`FfiEntity`] (`FfiEntity::NULL` -> `Ok(None)`), rejecting invalid/placeholder bits.
#[inline]
pub(crate) fn decode_optional_entity(entity: FfiEntity) -> Result<Option<Entity>, FfiResult> {
    if entity.is_null() {
        Ok(None)
    } else {
        decode_live_entity(entity).map(Some)
    }
}

/// Validates a NUL-terminated non-empty UTF-8 C string pointer.
///
/// # Safety
/// When non-null, `ptr` must point to a valid NUL-terminated C string readable for `'a`.
#[inline]
pub(crate) unsafe fn validate_non_empty_cstr<'a>(ptr: *const c_char) -> Result<&'a str, FfiResult> {
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    // SAFETY: Caller guarantees `ptr` points to a valid NUL-terminated C string for `'a`.
    let Ok(s) = (unsafe { CStr::from_ptr(ptr) }).to_str() else {
        return Err(FfiResult::INVALID_UTF8);
    };
    if s.is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    Ok(s)
}

/// Validates a required aligned writable output pointer and initializes it to `default`.
///
/// # Safety
/// When non-null and aligned, `ptr` must point to valid writable `T` storage.
#[inline]
pub(crate) unsafe fn init_out_ptr<'a, T>(ptr: *mut T, default: T) -> Result<&'a mut T, FfiResult> {
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if !ptr.is_aligned() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    // SAFETY: `ptr` is non-null, aligned, and caller guarantees writable `T` storage.
    unsafe {
        *ptr = default;
        Ok(&mut *ptr)
    }
}

/// Validates an optional aligned writable output pointer and initializes it to `default` if non-null.
///
/// # Safety
/// When non-null and aligned, `ptr` must point to valid writable `T` storage.
#[inline]
pub(crate) unsafe fn init_opt_out_ptr<'a, T>(
    ptr: *mut T,
    default: T,
) -> Result<Option<&'a mut T>, FfiResult> {
    if ptr.is_null() {
        return Ok(None);
    }
    // SAFETY: Non-null check performed above; caller upholds writable storage contract.
    unsafe { init_out_ptr(ptr, default) }.map(Some)
}

/// Acquires a [`Mutex`] guard, recovering cleanly from mutex poisoning.
#[inline]
pub(crate) fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Acquires an [`RwLock`] read guard, recovering cleanly from lock poisoning.
#[inline]
pub(crate) fn read_unpoison<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

/// Acquires an [`RwLock`] write guard, recovering cleanly from lock poisoning.
#[inline]
pub(crate) fn write_unpoison<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

/// RAII aligned heap byte buffer for dynamic component/event scratch storage and FFI transfers.
pub(crate) struct AlignedBuf {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl AlignedBuf {
    #[inline]
    fn dangling_for(layout: Layout) -> NonNull<u8> {
        let align = NonZeroUsize::new(layout.align()).unwrap_or(NonZeroUsize::MIN);
        NonNull::without_provenance(align)
    }

    /// Allocates a zero-initialized buffer matching `layout` (no heap allocation for ZSTs).
    pub(crate) fn from_layout_zeroed(layout: Layout) -> Self {
        if layout.size() == 0 {
            return Self {
                ptr: Self::dangling_for(layout),
                layout,
            };
        }
        // SAFETY: `layout.size() > 0` and `layout.align()` is a valid power of two.
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        let Some(ptr) = NonNull::new(raw) else {
            std::alloc::handle_alloc_error(layout);
        };
        Self { ptr, layout }
    }

    /// Allocates a zero-initialized buffer for `(size, align)` (clamping `align` to `>= 1`).
    pub(crate) fn new_zeroed(size: usize, align: usize) -> Result<Self, FfiResult> {
        let layout =
            Layout::from_size_align(size, align.max(1)).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
        Ok(Self::from_layout_zeroed(layout))
    }

    /// Allocates an aligned buffer and copies `size` bytes from `src`.
    ///
    /// # Safety
    /// If `size > 0`, `src` must be non-null and readable for `size` bytes.
    pub(crate) unsafe fn copy_from(
        src: *const u8,
        size: usize,
        align: usize,
    ) -> Result<Self, FfiResult> {
        let layout =
            Layout::from_size_align(size, align.max(1)).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
        if size == 0 {
            return Ok(Self {
                ptr: Self::dangling_for(layout),
                layout,
            });
        }
        if src.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        // SAFETY: `size > 0` and `layout.align()` is a valid power of two.
        let raw = unsafe { std::alloc::alloc(layout) };
        let Some(ptr) = NonNull::new(raw) else {
            std::alloc::handle_alloc_error(layout);
        };
        // SAFETY: Caller guarantees `src` is readable for `size` bytes; `ptr` is freshly
        // allocated for `size` writable bytes and does not overlap `src`.
        unsafe {
            core::ptr::copy_nonoverlapping(src, ptr.as_ptr(), size);
        }
        Ok(Self { ptr, layout })
    }

    #[inline]
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr().cast_const()
    }

    #[inline]
    pub(crate) fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    #[inline]
    pub(crate) fn as_mut_c_void(&self) -> *mut c_void {
        self.ptr.as_ptr().cast::<c_void>()
    }

    #[inline]
    pub(crate) fn non_null(&self) -> NonNull<u8> {
        self.ptr
    }

    #[inline]
    pub(crate) fn as_ptr_mut(&mut self) -> PtrMut<'_> {
        // SAFETY: `self.ptr` is aligned to `self.layout.align()` and points to `self.layout.size()`
        // initialized bytes owned exclusively by `&mut self`.
        unsafe { PtrMut::new(self.ptr) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        if self.layout.size() > 0 {
            // SAFETY: `self.ptr` was allocated with `std::alloc::alloc`/`alloc_zeroed(self.layout)`.
            unsafe {
                std::alloc::dealloc(self.ptr.as_ptr(), self.layout);
            }
        }
    }
}

// SAFETY: `AlignedBuf` uniquely owns its heap buffer of raw bytes.
unsafe impl Send for AlignedBuf {}
// SAFETY: Shared references expose no interior mutability on the owned byte buffer.
unsafe impl Sync for AlignedBuf {}

/// Concrete world-scoped registry and diagnostic context backing opaque [`BamHostCtx`].
///
/// Embedded inside `LoadContext` during `bevy_abi_mod_init` and inside `HostSystemCtx` during
/// `GuestSystem::run_unsafe`. Never shared concurrently across threads.
#[repr(C)]
pub struct HostContext {
    /// Validation header (`HOST_CTX_MAGIC`) at byte offset `0`.
    pub header: HandleHeader,
    /// Active registry snapshot stored in an `UnsafeCell` so updating the snapshot inside
    /// `LoadContext` during `bevy_abi_mod_init` (`register_component`, `register_relation`,
    /// `migrate_component_column`) does not invalidate cached `*const BamHostCtx` pointers
    /// previously returned by `host_load_host_ctx` under Tree Borrows / Stacked Borrows.
    snapshot: UnsafeCell<Arc<RegistrySnapshot>>,
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
            snapshot: UnsafeCell::new(snapshot),
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
        // SAFETY: `HostContext` is `!Sync` (single-threaded) and `snapshot` is only ever
        // mutated via `set_snapshot(&mut self)` between FFI calls when no `&RegistrySnapshot`
        // borrow is live.
        unsafe { &*self.snapshot.get() }
    }

    /// Clones the `Arc<RegistrySnapshot>` handle (`O(1)`).
    #[inline]
    pub fn snapshot_arc(&self) -> Arc<RegistrySnapshot> {
        // SAFETY: Same single-threaded non-overlapping invariant as `snapshot()`.
        Arc::clone(unsafe { &*self.snapshot.get() })
    }

    /// Replaces the active [`RegistrySnapshot`] (used by `LoadContext` after dynamic component registration).
    #[inline]
    pub fn set_snapshot(&mut self, snapshot: Arc<RegistrySnapshot>) {
        *self.snapshot.get_mut() = snapshot;
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
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let Ok(host_ctx) = (unsafe { validate_host_ctx(ctx) }) else {
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
