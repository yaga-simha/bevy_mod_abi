//! Deferred [`ModCommandsExt`] helpers on Bevy [`Commands`] for parallel systems (`B-02`, `F-01`).

use crate::world_ext::ModWorldExt;
use bevy_abi_mod_host_sys::{LoadOptions, ModId};
use bevy_app::Update;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::Schedules;
use bevy_ecs::system::Commands;
use bevy_ecs::world::World;
use std::path::Path;

type DeferredLifecycleFn = Box<dyn FnOnce(&mut World) + Send + Sync>;

/// Internal queue holding deferred [`ModCommandsExt`] operations that flushed while `Update` was
/// leased out of [`Schedules`] by `World::try_schedule_scope(Update, ...)`, replaying them in
/// `Last` once `Update` has been returned to [`Schedules`].
#[derive(Resource, Default)]
pub(crate) struct PendingModLifecycleOps {
    queue: Vec<DeferredLifecycleFn>,
}

pub(crate) fn flush_pending_mod_lifecycle_ops(world: &mut World) {
    let ops: Vec<DeferredLifecycleFn> = world
        .get_resource_mut::<PendingModLifecycleOps>()
        .map(|mut pending| core::mem::take(&mut pending.queue))
        .unwrap_or_default();
    for op in ops {
        op(world);
    }
}

fn run_or_defer_lifecycle_op<F>(world: &mut World, op: F)
where
    F: FnOnce(&mut World) + Send + Sync + 'static,
{
    let update_leased_out = world.contains_resource::<PendingModLifecycleOps>()
        && world
            .get_resource::<Schedules>()
            .is_some_and(|schedules| !schedules.contains(Update));
    if update_leased_out
        && let Some(mut pending) = world.get_resource_mut::<PendingModLifecycleOps>()
    {
        pending.queue.push(Box::new(op));
        return;
    }
    op(world);
}

/// Deferred mod load, unload, and reload commands usable from any normal parallel Bevy system (`B-02`, `F-01`).
///
/// Queued operations execute during an exclusive `&mut World` point once active schedules are
/// resident in [`Schedules`], preserving the safety invariant that shared libraries are never
/// opened or unmapped while worker threads run schedule systems.
pub trait ModCommandsExt {
    /// Queues a deferred [`ModWorldExt::load_mod`] operation with [`LoadOptions::default()`].
    fn load_mod<P: AsRef<Path>>(&mut self, path: P);

    /// Queues a deferred [`ModWorldExt::load_mod_with`] operation with explicit [`LoadOptions`].
    fn load_mod_with<P: AsRef<Path>>(&mut self, path: P, options: LoadOptions);

    /// Queues a deferred [`ModWorldExt::unload_mod`] operation for `id`.
    fn unload_mod(&mut self, id: ModId);

    /// Queues a deferred [`ModWorldExt::reload_mod`] operation for `id` (`F-01`).
    fn reload_mod(&mut self, id: ModId);

    /// Queues a deferred [`ModWorldExt::reload_mod_with`] operation for `id` (`F-01`).
    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions);
}

impl ModCommandsExt for Commands<'_, '_> {
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) {
        self.load_mod_with(path, LoadOptions::default());
    }

    fn load_mod_with<P: AsRef<Path>>(&mut self, path: P, options: LoadOptions) {
        let path_buf = path.as_ref().to_path_buf();
        self.queue(move |world: &mut World| {
            run_or_defer_lifecycle_op(world, move |w| {
                if let Err(err) = w.load_mod_with(&path_buf, options) {
                    tracing::error!(
                        path = %path_buf.display(),
                        error = %err,
                        "deferred Commands::load_mod_with failed"
                    );
                }
            });
        });
    }

    fn unload_mod(&mut self, id: ModId) {
        self.queue(move |world: &mut World| {
            run_or_defer_lifecycle_op(world, move |w| {
                if let Err(err) = w.unload_mod(id) {
                    tracing::error!(
                        mod_id = ?id,
                        error = %err,
                        "deferred Commands::unload_mod failed"
                    );
                }
            });
        });
    }

    fn reload_mod(&mut self, id: ModId) {
        self.reload_mod_with(id, LoadOptions::default().with_shadow_copy(true));
    }

    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions) {
        self.queue(move |world: &mut World| {
            run_or_defer_lifecycle_op(world, move |w| {
                if let Err(err) = w.reload_mod_with(id, options) {
                    tracing::error!(
                        mod_id = ?id,
                        error = %err,
                        "deferred Commands::reload_mod_with failed"
                    );
                }
            });
        });
    }
}
