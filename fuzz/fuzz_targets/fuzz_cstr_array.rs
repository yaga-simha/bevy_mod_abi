#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

//! `T-06` Fuzz Target 4: C-string / `after` / `before` ordering array & UTF-8 parsing across
//! `host_register_system`, `host_get_proc_address`, `host_resolve_component`, `host_last_error`,
//! and `ModNamedSystemSet::parse_target`.

use bevy_abi_mod_host_sys::{
    LoadContext, ModNamedSystemSet, ModRegistry, host_get_proc_address, host_last_error,
    host_load_host_ctx, host_register_system, host_resolve_component,
};
use bevy_abi_mod_sys::{BamComponent, BamSystemCtx, BamSystemDesc, ComponentUuid, FfiResult};
use core::ffi::{c_char, c_void};
use libfuzzer_sys::fuzz_target;
use std::hint::black_box;

unsafe extern "C" fn noop_cb(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
    FfiResult::OK
}

/// Converts arbitrary bytes into a NUL-terminated buffer preserving non-NUL arbitrary (including
/// invalid UTF-8) byte sequences.
fn nul_terminated_raw(bytes: &[u8]) -> Vec<u8> {
    let mut v: Vec<u8> = bytes.iter().copied().take(96).filter(|&b| b != 0).collect();
    v.push(0);
    v
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // 1. Direct Rust-side ordering target parser if valid UTF-8
    if let Ok(s) = core::str::from_utf8(data) {
        let _ = black_box(ModNamedSystemSet::parse_target(s, "fuzz_mod"));
    }

    // Split `data` on `0x00` bytes into variable C-string chunks (valid and invalid UTF-8).
    let mut chunks: Vec<Vec<u8>> = data
        .split(|&b| b == 0)
        .take(16)
        .map(nul_terminated_raw)
        .collect();
    if chunks.is_empty() {
        chunks.push(vec![b's', b'y', b's', 0]);
    }

    let sys_name_c = nul_terminated_raw(data);
    let mid = chunks.len() / 2;

    let mut after_ptrs: Vec<*const c_char> = chunks[..mid]
        .iter()
        .enumerate()
        .map(|(idx, c)| {
            if (data[0] & 0x10 != 0) && idx == 0 {
                core::ptr::null()
            } else {
                c.as_ptr().cast::<c_char>()
            }
        })
        .collect();

    let mut before_ptrs: Vec<*const c_char> = chunks[mid..]
        .iter()
        .enumerate()
        .map(|(idx, c)| {
            if (data[0] & 0x20 != 0) && idx == 0 {
                core::ptr::null()
            } else {
                c.as_ptr().cast::<c_char>()
            }
        })
        .collect();

    let mut ctx = LoadContext::new(ModRegistry::new());
    let load_ptr = ctx.as_ffi_ptr();
    // SAFETY: `load_ptr` is an active `LoadContext`.
    let host_ctx = unsafe { host_load_host_ctx(load_ptr.cast_const()) };

    // 2. Fuzz `host_get_proc_address` & `host_resolve_component` with arbitrary C-strings
    for chunk in &chunks {
        let c_ptr = chunk.as_ptr().cast::<c_char>();
        // SAFETY: `host_ctx` is valid and `c_ptr` is NUL-terminated.
        let sym = unsafe { host_get_proc_address(host_ctx, c_ptr) };
        let _ = black_box(sym);

        let mut out_comp = BamComponent::INVALID;
        let uuid = ComponentUuid::from_canonical_name("fuzz::Target");
        let uuid_ptr = if chunk.len() & 1 == 0 {
            &uuid as *const ComponentUuid
        } else {
            core::ptr::null()
        };
        // SAFETY: `host_ctx` is valid, `c_ptr` is NUL-terminated, `out_comp` is writable.
        let res = unsafe { host_resolve_component(host_ctx, c_ptr, uuid_ptr, &mut out_comp) };
        let _ = black_box(res);
    }

    // 3. Fuzz `host_register_system` `after` and `before` C-string pointer arrays
    let mut desc = BamSystemDesc::new(
        sys_name_c.as_ptr().cast::<c_char>(),
        core::ptr::null(),
        0,
        noop_cb,
        core::ptr::null_mut(),
    );
    desc.after = if after_ptrs.is_empty() {
        core::ptr::null()
    } else {
        after_ptrs.as_mut_ptr().cast_const()
    };
    desc.after_count = after_ptrs.len();
    desc.before = if before_ptrs.is_empty() {
        core::ptr::null()
    } else {
        before_ptrs.as_mut_ptr().cast_const()
    };
    desc.before_count = before_ptrs.len();

    // SAFETY: `load_ptr` is active; `sys_name_c`, `after_ptrs`, and `before_ptrs` live on stack.
    let reg_res = unsafe { host_register_system(load_ptr, &desc) };
    let _ = black_box(reg_res);

    // 4. Fuzz `host_last_error` truncation / zero-capacity / exact-capacity boundary
    let cap = (data[0] as usize) % 40;
    let mut err_buf = vec![0xAAu8; cap.max(1)];
    let buf_ptr = if cap == 0 {
        core::ptr::null_mut()
    } else {
        err_buf.as_mut_ptr().cast::<c_char>()
    };
    // SAFETY: When `cap > 0`, `err_buf` has `cap` writable bytes; when `cap == 0`, null is allowed.
    let total_len = unsafe { host_last_error(host_ctx, buf_ptr, cap) };
    if cap > 0 {
        // Must always NUL-terminate within `0..cap`.
        assert!(err_buf[..cap].contains(&0));
    }
    let _ = black_box(total_len);
});
