//! RAII `LoadTransaction` rolling back drop trampolines, registry entries, and symbols before `dlclose`.

use crate::drop_trampoline::prune_drop_trampolines_for_guard;
use crate::loader::ModId;
use crate::registry::ModRegistry;
use bevy_ecs::world::World;
use core::sync::atomic::AtomicBool;
use std::sync::Arc;

/// RAII transaction guard bound after opening a [`libloading::Library`] so that if initialization
/// fails (and `commit` is not reached), all drop trampoline slots, guest symbol entries, and
/// `ModRegistry` mutations are disarmed and rolled back *before* `Library::drop` (`dlclose`).
pub(crate) struct LoadTransaction {
    world_ptr: *mut World,
    mod_id: ModId,
    is_active: Arc<AtomicBool>,
    pre_registry: ModRegistry,
    committed: bool,
}

impl LoadTransaction {
    /// Starts a new rollback guard for `mod_id`.
    ///
    /// Declare this binding *after* `let lib = Library::new(...)` so Rust's reverse local drop
    /// order guarantees `LoadTransaction::drop` runs prior to `dlclose`.
    pub(crate) fn new(
        world_ptr: *mut World,
        mod_id: ModId,
        is_active: Arc<AtomicBool>,
        pre_registry: ModRegistry,
    ) -> Self {
        Self {
            world_ptr,
            mod_id,
            is_active,
            pre_registry,
            committed: false,
        }
    }

    /// Marks initialization as succeeded, disarming automatic rollback on drop.
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

        // 2. Mark `self.mod_id` revoked in the shared lineage and restore `World`'s `ModRegistry`.
        self.pre_registry.evict_mod(self.mod_id);
        if !self.world_ptr.is_null() {
            // SAFETY: `self.world_ptr` comes from the exclusive `&mut World` passed into `load_mod_core`
            // and no inner `&mut World` alias is live while `LoadTransaction` drops.
            let world = unsafe { &mut *self.world_ptr };
            if world.contains_resource::<ModRegistry>() {
                world.insert_resource(self.pre_registry.clone());
            }
        }
    }
}
