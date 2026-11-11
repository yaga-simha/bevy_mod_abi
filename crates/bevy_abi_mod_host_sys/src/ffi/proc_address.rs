//! C-ABI dynamic symbol resolution (`GetProcAddressFn`).

use crate::registry::symbols::{CURRENT_REGISTRY, GLOBAL_SYMBOLS};
use core::ffi::{CStr, c_char, c_void};

/// C-ABI symbol resolver function (`GetProcAddressFn`).
///
/// Checks thread-local `CURRENT_REGISTRY` first, falling back to `GLOBAL_SYMBOLS` when no
/// `World` registry guard is bound on the calling thread.
///
/// # Safety
/// `name` must point to a valid NUL-terminated C string or be null.
pub unsafe extern "C" fn host_get_proc_address(name: *const c_char) -> *const c_void {
    if name.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Caller passes a valid NUL-terminated C string.
    let Ok(sym_str) = (unsafe { CStr::from_ptr(name) }).to_str() else {
        return core::ptr::null();
    };

    let local_res = CURRENT_REGISTRY.try_with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|reg| reg.resolve_symbol(sym_str).unwrap_or(core::ptr::null()))
    });
    if let Ok(Some(ptr)) = local_res {
        return ptr;
    }

    if let Ok(guard) = GLOBAL_SYMBOLS.read()
        && let Some(&sym_addr) = guard.get(sym_str)
    {
        return sym_addr.as_ptr();
    }

    core::ptr::null()
}
