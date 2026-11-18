//! RAII `LoadTransaction` rolling back drop trampolines and `ModRegistry` snapshot before `dlclose`.

use crate::drop_trampoline::{
    StagedDropBinding, StagedHookBinding, WorldDropGuard, prune_drop_trampolines_for_guard,
};
use crate::registry::{ModRegistry, RegistrySnapshot};
use bevy_ecs::world::World;
use core::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// RAII transaction guard bound after opening a [`crate::loader::shared_lib::LoadedLibrary`] so that
/// if `bevy_abi_mod_init` fails (and `commit` is not reached), all drop trampoline slots (including
/// freshly allocated slots returned to `FREE_LIST`) and `ModRegistry` snapshot mutations are rolled
/// back **strictly before** `LoadedLibrary::drop` (`dlclose`).
pub(crate) struct LoadTransaction {
    world_ptr: *mut World,
    is_active: Arc<AtomicBool>,
    pre_snapshot: Arc<RegistrySnapshot>,
    staged_drop_bindings: Arc<Mutex<Vec<StagedDropBinding>>>,
    staged_hook_bindings: Arc<Mutex<Vec<StagedHookBinding>>>,
    committed: bool,
}

impl LoadTransaction {
    /// Starts a new rollback guard restoring `pre_snapshot` on failure.
    ///
    /// Declare this binding *after* `LoadedLibrary::open(...)` so Rust's reverse local drop order
    /// guarantees `LoadTransaction::drop` executes prior to `dlclose`.
    pub(crate) fn new(
        world_ptr: *mut World,
        is_active: Arc<AtomicBool>,
        pre_snapshot: Arc<RegistrySnapshot>,
    ) -> Self {
        Self {
            world_ptr,
            is_active,
            pre_snapshot,
            staged_drop_bindings: Arc::new(Mutex::new(Vec::new())),
            staged_hook_bindings: Arc::new(Mutex::new(Vec::new())),
            committed: false,
        }
    }

    /// Returns a shared handle to the staged drop-binding log for `LoadContext`.
    #[inline]
    pub(crate) fn staged_drop_bindings(&self) -> Arc<Mutex<Vec<StagedDropBinding>>> {
        Arc::clone(&self.staged_drop_bindings)
    }

    /// Returns a shared handle to the staged hook-binding log for `LoadContext`.
    #[inline]
    pub(crate) fn staged_hook_bindings(&self) -> Arc<Mutex<Vec<StagedHookBinding>>> {
        Arc::clone(&self.staged_hook_bindings)
    }

    /// Marks `bevy_abi_mod_init` as succeeded, disarming automatic drop rollback and returning the
    /// staged bindings so `rollback_unscheduled_mod_in_world` can still reclaim fresh slots if
    /// post-init schedule/strict validation fails before scheduling.
    #[inline]
    pub(crate) fn commit(mut self) -> (Vec<StagedDropBinding>, Vec<StagedHookBinding>) {
        self.committed = true;
        let drops = {
            let mut guard = self
                .staged_drop_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        let hooks = {
            let mut guard = self
                .staged_hook_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        (drops, hooks)
    }
}

impl Drop for LoadTransaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        let bindings = {
            let mut guard = self
                .staged_drop_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        let hook_bindings = {
            let mut guard = self
                .staged_hook_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };

        // 1. Roll back staged drop & hook bindings in `WorldDropGuard` (returning fresh slots to `FREE_LIST`
        //    and restoring `previous_snapshot` on hot-updated slots) and restore `ModRegistry`.
        if !self.world_ptr.is_null() {
            // SAFETY: `self.world_ptr` comes from the exclusive `&mut World` passed into `load_mod_core`
            // and no inner `&mut World` alias is live while `LoadTransaction` drops.
            let world = unsafe { &mut *self.world_ptr };
            if let Some(mut drop_guard) = world.get_resource_mut::<WorldDropGuard>() {
                drop_guard.rollback_staged_bindings(&bindings);
                drop_guard.rollback_staged_hook_bindings(&hook_bindings);
            }
            if let Some(mut reg) = world.get_resource_mut::<ModRegistry>() {
                reg.restore_snapshot(Arc::clone(&self.pre_snapshot));
            }
        }

        // 2. Disarm `is_active = false` and prune any remaining references before `dlclose`.
        prune_drop_trampolines_for_guard(&self.is_active);
    }
}

#[cfg(test)]
mod tests {
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
}
