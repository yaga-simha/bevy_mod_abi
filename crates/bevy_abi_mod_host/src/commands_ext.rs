//! Deferred [`ModCommandsExt`] helpers on Bevy [`Commands`] for parallel systems (`B-02`).

use crate::world_ext::ModWorldExt;
use bevy_abi_mod_host_sys::{LoadOptions, ModId};
use bevy_ecs::system::Commands;
use bevy_ecs::world::World;
use std::path::Path;

/// Deferred mod load and unload commands usable from any normal parallel Bevy system (`B-02`).
///
/// Queued operations execute during Bevy's exclusive command-flush point (`&mut World`), preserving
/// the safety invariant that shared libraries are never opened or unmapped while worker threads run
/// schedule systems.
pub trait ModCommandsExt {
    /// Queues a deferred [`ModWorldExt::load_mod`] operation with [`LoadOptions::default()`].
    fn load_mod<P: AsRef<Path>>(&mut self, path: P);

    /// Queues a deferred [`ModWorldExt::load_mod_with`] operation with explicit [`LoadOptions`].
    fn load_mod_with<P: AsRef<Path>>(&mut self, path: P, options: LoadOptions);

    /// Queues a deferred [`ModWorldExt::unload_mod`] operation for `id`.
    fn unload_mod(&mut self, id: ModId);
}

impl ModCommandsExt for Commands<'_, '_> {
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) {
        self.load_mod_with(path, LoadOptions::default());
    }

    fn load_mod_with<P: AsRef<Path>>(&mut self, path: P, options: LoadOptions) {
        let path_buf = path.as_ref().to_path_buf();
        self.queue(move |world: &mut World| {
            if let Err(err) = world.load_mod_with(&path_buf, options) {
                tracing::error!(
                    path = %path_buf.display(),
                    error = %err,
                    "deferred Commands::load_mod_with failed"
                );
            }
        });
    }

    fn unload_mod(&mut self, id: ModId) {
        self.queue(move |world: &mut World| {
            if let Err(err) = world.unload_mod(id) {
                tracing::error!(
                    mod_id = ?id,
                    error = %err,
                    "deferred Commands::unload_mod failed"
                );
            }
        });
    }
}
