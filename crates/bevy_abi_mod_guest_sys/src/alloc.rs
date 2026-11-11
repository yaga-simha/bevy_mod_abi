//! Guest-owned heap allocation tracking and panic containment across `extern "C"` boundaries.

use core::ffi::c_void;
use std::sync::Mutex;

use bevy_abi_mod_sys::FfiResult;

/// Executes a closure inside `std::panic::catch_unwind` within the guest's own Rust runtime,
/// converting any unwinding panic into [`FfiResult::PANIC_CAUGHT`] before crossing `extern "C"`.
#[inline]
pub fn catch_guest_panic<F>(f: F) -> FfiResult
where
    F: FnOnce() -> FfiResult,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(_) => FfiResult::PANIC_CAUGHT,
    }
}

/// Opaque type-erased guest-heap allocation tracked for transactional rollback or LIFO shutdown.
pub struct ErasedAllocation {
    ptr: *mut c_void,
    drop_fn: unsafe fn(*mut c_void),
}

// SAFETY: Every allocation tracked in `ErasedAllocation` is constructed from a `Send + 'static`
// value and is dropped at most once during rollback or `bevy_abi_mod_shutdown`.
unsafe impl Send for ErasedAllocation {}

pub(crate) unsafe fn drop_boxed_allocation<T>(ptr: *mut c_void) {
    if !ptr.is_null() {
        // SAFETY: `ptr` was allocated via `Box::into_raw(Box::new(...))` of type `T`
        // inside this guest `.cdylib`'s allocator and is dropped at most once.
        drop(unsafe { Box::from_raw(ptr.cast::<T>()) });
    }
}

struct ShutdownHook<F: FnOnce()> {
    hook: Option<F>,
}

impl<F: FnOnce()> Drop for ShutdownHook<F> {
    fn drop(&mut self) {
        if let Some(f) = self.hook.take() {
            f();
        }
    }
}

impl ErasedAllocation {
    /// Allocates `value` on the guest heap, returning its tracked [`ErasedAllocation`] and raw `*mut T`.
    pub fn new_boxed<T: Send + 'static>(value: T) -> (Self, *mut T) {
        let raw = Box::into_raw(Box::new(value));
        (
            Self {
                ptr: raw.cast::<c_void>(),
                drop_fn: drop_boxed_allocation::<T>,
            },
            raw,
        )
    }

    /// Wraps a shutdown closure into a tracked [`ErasedAllocation`] that invokes `cleanup` when dropped.
    pub fn new_shutdown_hook<F: FnOnce() + Send + 'static>(cleanup: F) -> Self {
        let raw = Box::into_raw(Box::new(ShutdownHook {
            hook: Some(cleanup),
        }));
        Self {
            ptr: raw.cast::<c_void>(),
            drop_fn: drop_boxed_allocation::<ShutdownHook<F>>,
        }
    }

    pub(crate) fn from_raw_parts(ptr: *mut c_void, drop_fn: unsafe fn(*mut c_void)) -> Self {
        Self { ptr, drop_fn }
    }
}

static GUEST_ALLOCATIONS: Mutex<Vec<ErasedAllocation>> = Mutex::new(Vec::new());

fn drop_allocation_list_lifo(mut list: Vec<ErasedAllocation>) {
    while let Some(alloc) = list.pop() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: Each `ErasedAllocation` owns a unique guest-heap allocation paired with
            // its matching monomorphized `drop_fn`.
            unsafe { (alloc.drop_fn)(alloc.ptr) };
        }));
    }
}

/// Commits staged allocations from a successful `GuestApp` initialization into `GUEST_ALLOCATIONS`.
pub fn commit_allocations(allocs: &mut Vec<ErasedAllocation>) {
    if allocs.is_empty() {
        return;
    }
    let mut guard = GUEST_ALLOCATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.append(allocs);
}

/// Rolls back and drops staged allocations in LIFO order when `GuestApp` initialization aborts.
pub fn abort_allocations(allocs: Vec<ErasedAllocation>) {
    if !allocs.is_empty() {
        drop_allocation_list_lifo(allocs);
    }
}

/// Drops all guest-side system closures, `user_data` allocations, and shutdown hooks
/// registered via `GuestApp` in reverse registration (LIFO) order.
pub fn cleanup_registered_guest_state() {
    let drained = {
        let mut guard = GUEST_ALLOCATIONS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        core::mem::take(&mut *guard)
    };
    drop_allocation_list_lifo(drained);
}
