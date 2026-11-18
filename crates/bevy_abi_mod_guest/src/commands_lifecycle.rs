//! Extended entity lifecycle, batch spawn, trigger, scene, and runtime observer guest commands.

use bevy_abi_mod_guest_sys::SpawnEntry;
use bevy_abi_mod_sys::{FfiEntity, FfiResult};

use crate::commands::GuestCommands;

impl<'a> GuestCommands<'a> {
    /// Reserves a fresh empty entity whose lifetime persists across mod unload
    /// (`BamHostApi::cmd_spawn_persistent`), returning its [`FfiEntity`] identifier.
    #[inline]
    pub fn spawn_empty_persistent(&mut self) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_persistent(&[])
    }

    /// Reserves a fresh entity whose lifetime persists across mod unload
    /// (`BamHostApi::cmd_spawn_persistent`) and batch-inserts `entries` in a single archetype
    /// transition.
    #[inline]
    pub fn spawn_persistent(&mut self, entries: &[SpawnEntry<'_>]) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_persistent(entries)
    }
}
