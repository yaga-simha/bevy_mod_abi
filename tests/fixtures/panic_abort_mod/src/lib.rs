#![allow(unreachable_pub)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! `panic = "abort"` Rust guest fixture.
//!
//! When compiled with `panic = "abort"`, `std::panic::catch_unwind` inside
//! `bevy_abi_mod_guest_sys` is a no-op and any panic inside `abort_panic_sys`
//! deterministically aborts the OS process at the panic site without crossing `extern "C"`.

use bevy_abi_mod_guest::prelude::*;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static ARM_PANIC: AtomicBool = AtomicBool::new(false);
static HEALTHY_TICKS: AtomicU32 = AtomicU32::new(0);

/// Arms the `panic = "abort"` trigger for the next execution of `abort_panic_sys`.
///
/// # Safety
/// Safe to invoke from test harness threads; operates atomically with `SeqCst` ordering.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn panic_abort_mod_arm_panic() {
    ARM_PANIC.store(true, Ordering::SeqCst);
}

/// Returns the number of non-panicking ticks completed by `abort_panic_sys`.
///
/// # Safety
/// Safe to invoke from test harness threads; operates atomically with `SeqCst` ordering.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn panic_abort_mod_healthy_ticks() -> u32 {
    HEALTHY_TICKS.load(Ordering::SeqCst)
}

fn setup(app: &mut GuestApp<'_>) -> FfiResult {
    app.system(c"abort_panic_sys").build(|_ctx| {
        if ARM_PANIC.load(Ordering::SeqCst) {
            panic!("intentional panic inside panic=abort guest fixture");
        }
        HEALTHY_TICKS.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    })
}

export_mod!(name: c"panic_abort_mod", version: c"0.2.0", setup);
