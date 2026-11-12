//! RAII `LoadTransaction` rolling back drop trampolines and `ModRegistry` snapshot before `dlclose` (`S-06`).

use crate::drop_trampoline::prune_drop_trampolines_for_guard;
use crate::registry::{ModRegistry, RegistrySnapshot};
use bevy_ecs::world::World;
use core::sync::atomic::AtomicBool;
use std::sync::Arc;

/// RAII transaction guard bound after opening a [`crate::loader::shared_lib::LoadedLibrary`] so that
/// if `bevy_abi_mod_init` or post-init validation fails (and `commit` is not reached), all drop
/// trampoline slots and `ModRegistry` snapshot mutations are rolled back **strictly before**
/// `LoadedLibrary::drop` (`dlclose`).
pub(crate) struct LoadTransaction {
    world_ptr: *mut World,
    is_active: Arc<AtomicBool>,
    pre_snapshot: Arc<RegistrySnapshot>,
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
            committed: false,
        }
    }

    /// Marks initialization as succeeded, disarming automatic rollback on drop.
    #[inline]
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for LoadTransaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        // 1. Disarm `is_active = false` and prune all drop trampoline entries before `dlclose`.
        prune_drop_trampolines_for_guard(&self.is_active);

        // 2. Restore the pre-load `RegistrySnapshot` on `World`'s `ModRegistry`.
        if !self.world_ptr.is_null() {
            // SAFETY: `self.world_ptr` comes from the exclusive `&mut World` passed into `load_mod_core`
            // and no inner `&mut World` alias is live while `LoadTransaction` drops.
            let world = unsafe { &mut *self.world_ptr };
            if let Some(mut reg) = world.get_resource_mut::<ModRegistry>() {
                reg.restore_snapshot(Arc::clone(&self.pre_snapshot));
            }
        }
    }
}
