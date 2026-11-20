use super::LoadTransaction;
use crate::drop_trampoline::{
    MAX_DROP_TRAMPOLINES, WorldDropGuard, available_drop_trampoline_slots,
};
use crate::ffi::register::{LoadContext, host_register_component};
use crate::registry::ModRegistry;
use bevy_abi_mod_sys::{BamComponent, ComponentDesc, ComponentUuid, FfiResult};
use bevy_ecs::world::World;
use core::ffi::{CStr, c_void};
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

unsafe extern "C" fn noop_drop(_ptr: *mut c_void) {}

#[test]
fn load_transaction_drop_reclaims_fresh_slots_across_1100_failed_inits_in_one_live_world() {
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModRegistry::new());
    let baseline_free = available_drop_trampoline_slots();

    // Exceed `MAX_DROP_TRAMPOLINES` on a single live `World` without dropping `world`!
    let iters = if cfg!(miri) {
        12
    } else {
        MAX_DROP_TRAMPOLINES + 76
    };
    for i in 0..iters {
        let reg = world.resource::<ModRegistry>().clone();
        let pre_snap = reg.snapshot_arc();
        let is_active = Arc::new(AtomicBool::new(true));
        let world_ptr = &mut world as *mut World;

        let tx = LoadTransaction::new(world_ptr, Arc::clone(&is_active), pre_snap);
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, Arc::clone(&is_active), world_ptr, None)
                .with_staged_drop_bindings(tx.staged_drop_bindings());

        let cname_str = format!("failed_mod::Comp{i}\0");
        let cname = CStr::from_bytes_with_nul(cname_str.as_bytes()).unwrap();
        let desc = ComponentDesc::new(
            cname.as_ptr(),
            ComponentUuid::from_bytes([
                (i & 0xFF) as u8,
                ((i >> 8) & 0xFF) as u8,
                0xAA,
                0xBB,
                1,
                2,
                3,
                4,
                5,
                6,
                7,
                8,
                9,
                10,
                11,
                12,
            ]),
            4,
            4,
        )
        .with_drop(noop_drop);

        let mut out = BamComponent::INVALID;
        // SAFETY: Valid `load_ctx`, `desc`, and `out`.
        let st = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut out) };
        assert_eq!(st, FfiResult::OK, "iteration {i} leaked drop slots");
        load_ctx.finish_init();
        // Drop `load_ctx` and `tx` WITHOUT `tx.commit()`, simulating `bevy_abi_mod_init` error!
        drop(load_ctx);
        drop(tx);

        assert!(!is_active.load(Ordering::Acquire));
    }

    assert_eq!(world.resource::<WorldDropGuard>().len(), 0);
    assert_eq!(available_drop_trampoline_slots(), baseline_free);
}
