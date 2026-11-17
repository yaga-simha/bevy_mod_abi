//! Context-carrying `"bam.v0.*"` symbol and VTable resolution (`host_get_proc_address`).

use crate::handle_header::{HOST_CTX_MAGIC, HostContext, validate_handle_ref};
use bevy_abi_mod_sys::BamHostCtx;
use core::ffi::{CStr, c_char, c_void};

/// Resolves a `"bam.v0.<name>"` symbol or component VTable (`"bam.v0.<canonical_name>.vtable"`)
/// against the world-scoped [`BamHostCtx`].
///
/// Returns null and populates `last_error` on `ctx` if `name` is null, invalid UTF-8, lacks the
/// `"bam.v0."` namespace prefix, or is unregistered/evicted in `ctx`.
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - If non-null, `name` must point to a NUL-terminated C string.
pub unsafe extern "C" fn host_get_proc_address(
    ctx: *const BamHostCtx,
    name: *const c_char,
) -> *const c_void {
    // SAFETY: Validating `ctx` against `HOST_CTX_MAGIC` unconditionally (`always_check = true`).
    let Ok(host_ctx) =
        (unsafe { validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true) })
    else {
        return core::ptr::null();
    };

    if name.is_null() {
        host_ctx.set_last_error("get_proc_address called with null symbol name pointer");
        return core::ptr::null();
    }

    // SAFETY: Caller guarantees `name` is a NUL-terminated C string when non-null.
    let Ok(sym_name) = (unsafe { CStr::from_ptr(name) }).to_str() else {
        host_ctx.set_last_error("get_proc_address symbol name is not valid UTF-8");
        return core::ptr::null();
    };

    if !sym_name.starts_with("bam.v0.") {
        host_ctx.set_last_error(format!(
            "get_proc_address symbol '{sym_name}' must use the 'bam.v0.' namespace prefix"
        ));
        return core::ptr::null();
    }

    match host_ctx.snapshot().resolve_symbol(sym_name) {
        Some(ptr) => ptr,
        None => {
            host_ctx.set_last_error(format!("symbol '{sym_name}' not found in host registry"));
            core::ptr::null()
        }
    }
}
