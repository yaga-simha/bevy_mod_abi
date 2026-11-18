//! Safe handle-based and batched deferred command buffer (`GuestCommands`).

use core::ffi::CStr;

pub use bevy_abi_mod_guest_sys::SpawnBatchColumn;
use bevy_abi_mod_guest_sys::{Host, SpawnEntry, ValidatedCommands, resolve_component};
use bevy_abi_mod_sys::{
    AbiPod, BamCommands, BamComponent, BamTermDesc, ComponentUuid, FfiEntity, FfiResult,
};

use crate::component::{GuestComponent, resolve_guest_component};

/// Safe deferred command buffer bound to an active system execution context.
///
/// Supports O(1) pre-resolved [`BamComponent`] handle operations (`spawn_empty`, batched `spawn`,
/// `despawn`, `insert`, `insert_default`, `remove`) as well as typed [`GuestComponent`] convenience
/// helpers.
///
/// Constructing `GuestCommands` from raw C pointers in safe Rust is rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::GuestCommands;
///
/// let _cmds = GuestCommands::from_raw(core::ptr::null_mut());
/// ```
///
/// Inserting non-`AbiPod` types (such as `String` or `bool`) is also rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::{BamComponent, FfiEntity, GuestCommands};
///
/// fn bad_insert(cmds: &mut GuestCommands<'_>, e: FfiEntity, c: BamComponent) {
///     let s = String::from("heap");
///     let _ = cmds.insert(e, c, &s);
/// }
/// ```
pub struct GuestCommands<'a> {
    pub(crate) inner: ValidatedCommands<'a>,
}

impl<'a> GuestCommands<'a> {
    /// Wraps an already-validated [`ValidatedCommands`] handle.
    #[inline]
    pub const fn from_validated(inner: ValidatedCommands<'a>) -> Self {
        Self { inner }
    }

    /// Returns `true` if this command buffer holds an active non-null [`BamCommands`] handle.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.inner.is_valid()
    }

    /// Returns the cached [`Host`] table for this mod instance.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.inner.host()
    }

    /// Returns the raw `*mut BamCommands` handle for FFI interoperability.
    #[inline]
    pub const fn as_raw_ptr(&self) -> *mut BamCommands {
        self.inner.as_raw()
    }

    /// Resolves a [`BamComponent`] handle by `name`, `uuid`, or both via the host registry.
    #[inline]
    pub fn resolve_component(
        &self,
        name: Option<&CStr>,
        uuid: Option<ComponentUuid>,
    ) -> Result<BamComponent, FfiResult> {
        resolve_component(self.inner.host(), name, uuid)
    }

    /// Reserves a fresh empty entity atomically and returns its [`FfiEntity`] identifier.
    #[inline]
    pub fn spawn_empty(&mut self) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn_empty()
    }

    /// Reserves a fresh entity and batch-inserts `entries` in a single archetype transition.
    #[inline]
    pub fn spawn(&mut self, entries: &[SpawnEntry<'_>]) -> Result<FfiEntity, FfiResult> {
        self.inner.spawn(entries)
    }

    /// Queues deferred despawning of `entity`.
    #[inline]
    pub fn despawn(&mut self, entity: FfiEntity) -> FfiResult {
        self.inner.despawn(entity)
    }

    /// Queues deferred insertion of `value` (`T: AbiPod`) onto `entity` using pre-resolved
    /// [`BamComponent`] handle.
    #[inline]
    pub fn insert<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: &T,
    ) -> FfiResult {
        self.inner.insert(entity, component, value)
    }

    /// Queues deferred insertion of host-constructed `T::default()` for pre-resolved `component`
    /// onto `entity`.
    #[inline]
    pub fn insert_default(&mut self, entity: FfiEntity, component: BamComponent) -> FfiResult {
        self.inner.insert_default(entity, component)
    }

    /// Queues deferred removal of pre-resolved `component` from `entity`.
    #[inline]
    pub fn remove(&mut self, entity: FfiEntity, component: BamComponent) -> FfiResult {
        self.inner.remove(entity, component)
    }

    /// Convenience helper resolving `T: GuestComponent` (including manifest-qualified names)
    /// and inserting `value`.
    ///
    /// Prefer pre-resolving [`BamComponent`] handles at mod init for hot per-frame loops.
    #[inline]
    pub fn insert_component<T: GuestComponent>(
        &mut self,
        entity: FfiEntity,
        value: &T,
    ) -> FfiResult {
        let host = self.inner.host();
        match resolve_guest_component::<T>(host, host.manifest_name()) {
            Ok(comp) => self.inner.insert(entity, comp, value),
            Err(err) => err,
        }
    }

    /// Convenience helper resolving `T: GuestComponent` (including manifest-qualified names)
    /// and inserting `T::default()` on the host.
    #[inline]
    pub fn insert_component_default<T: GuestComponent>(&mut self, entity: FfiEntity) -> FfiResult {
        let host = self.inner.host();
        match resolve_guest_component::<T>(host, host.manifest_name()) {
            Ok(comp) => self.inner.insert_default(entity, comp),
            Err(err) => err,
        }
    }

    /// Convenience helper resolving `T: GuestComponent` (including manifest-qualified names)
    /// and removing `T` from `entity`.
    #[inline]
    pub fn remove_component<T: GuestComponent>(&mut self, entity: FfiEntity) -> FfiResult {
        let host = self.inner.host();
        match resolve_guest_component::<T>(host, host.manifest_name()) {
            Ok(comp) => self.inner.remove(entity, comp),
            Err(err) => err,
        }
    }

    /// Triggers a global Bevy `Event` (`E: GuestComponent`) resolved on the host via deferred
    /// `world.trigger(...)`.
    #[inline]
    pub fn trigger<E: GuestComponent>(&mut self, event: &E) -> Result<(), FfiResult> {
        self.trigger_entity::<E>(FfiEntity::NULL, event)
    }

    /// Triggers an entity-targeted Bevy `EntityEvent` (`E: GuestComponent`) on `target` via deferred
    /// `world.trigger(...)` / `world.trigger_dynamic_targets`.
    #[inline]
    pub fn trigger_entity<E: GuestComponent>(
        &mut self,
        target: FfiEntity,
        event: &E,
    ) -> Result<(), FfiResult> {
        let host = self.inner.host();
        let comp = resolve_guest_component::<E>(host, host.manifest_name())?;
        self.trigger_handle(comp, target, event)
    }

    /// Triggers a pre-resolved event handle `handle` (Pass [`FfiEntity::NULL`] for global triggers).
    #[inline]
    pub fn trigger_handle<E: AbiPod>(
        &mut self,
        handle: BamComponent,
        target: FfiEntity,
        event: &E,
    ) -> Result<(), FfiResult> {
        let status = self.inner.trigger(handle, target, event);
        if status.is_ok() { Ok(()) } else { Err(status) }
    }

    /// Spawns `entity_count` entities in one FFI call using Structure-of-Arrays component
    /// columns, returning all reserved [`FfiEntity`] handles.
    #[inline]
    pub fn spawn_batch(
        &mut self,
        columns: &[SpawnBatchColumn<'_>],
        entity_count: usize,
    ) -> Result<Vec<FfiEntity>, FfiResult> {
        let mut out = vec![FfiEntity::NULL; entity_count];
        let status = self
            .inner
            .spawn_batch_columns(columns, entity_count, Some(&mut out));
        if status.is_ok() { Ok(out) } else { Err(status) }
    }

    /// Spawns `entity_count` entities in one FFI call using Structure-of-Arrays component
    /// columns, optionally writing reserved [`FfiEntity`] handles into `out_entities`.
    #[inline]
    pub fn spawn_batch_into(
        &mut self,
        columns: &[SpawnBatchColumn<'_>],
        entity_count: usize,
        out_entities: Option<&mut [FfiEntity]>,
    ) -> Result<(), FfiResult> {
        let status = self
            .inner
            .spawn_batch_columns(columns, entity_count, out_entities);
        if status.is_ok() { Ok(()) } else { Err(status) }
    }

    /// Queues a deferred query-filtered bulk despawn of non-resource entities matching `terms`.
    #[inline]
    pub fn despawn_all(&mut self, terms: &[BamTermDesc]) -> Result<(), FfiResult> {
        let status = self.inner.despawn_all(terms);
        if status.is_ok() { Ok(()) } else { Err(status) }
    }

    /// Queues a deferred conditional insertion (`InsertMode::Keep`) of `value` (`T: AbiPod`) onto
    /// `entity` only if `component` is not already present.
    #[inline]
    pub fn insert_if_new<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: &T,
    ) -> Result<(), FfiResult> {
        let status = self.inner.insert_if_new(entity, component, value);
        if status.is_ok() { Ok(()) } else { Err(status) }
    }

    /// Resolves `T: GuestComponent` and queues a deferred conditional insertion (`InsertMode::Keep`)
    /// onto `entity` only if absent.
    #[inline]
    pub fn insert_component_if_new<T: GuestComponent>(
        &mut self,
        entity: FfiEntity,
        value: &T,
    ) -> Result<(), FfiResult> {
        let host = self.inner.host();
        let comp = resolve_guest_component::<T>(host, host.manifest_name())?;
        self.insert_if_new(entity, comp, value)
    }
}

impl<'a> From<ValidatedCommands<'a>> for GuestCommands<'a> {
    #[inline]
    fn from(inner: ValidatedCommands<'a>) -> Self {
        Self::from_validated(inner)
    }
}
