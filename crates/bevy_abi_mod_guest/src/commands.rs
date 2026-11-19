//! Safe handle-based and batched deferred command buffer (`GuestCommands`).

use core::{
    ffi::CStr,
    ops::{Deref, DerefMut},
};

pub use bevy_abi_mod_guest_sys::SpawnBatchColumn;
use bevy_abi_mod_guest_sys::{Host, SpawnEntry, ValidatedCommands, resolve_component};
use bevy_abi_mod_sys::{
    AbiPod, BamCommands, BamComponent, BamEventKind, BamTermDesc, ComponentUuid, FfiEntity,
    FfiResult, ModComponent,
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

    /// Queues deferred insertion of a marker/tag relationship edge
    /// `entity --[relation]--> target` (`BamHostApi::cmd_insert_relation`).
    #[inline]
    pub fn insert_relation(
        &mut self,
        entity: FfiEntity,
        relation: BamComponent,
        target: FfiEntity,
    ) -> FfiResult {
        self.inner.insert_relation(entity, relation, target)
    }

    /// Queues deferred insertion of a payload-carrying relationship edge
    /// `entity --[relation(payload)]--> target` (`T: AbiPod`) via
    /// `BamHostApi::cmd_insert_relation`.
    #[inline]
    pub fn insert_relation_with_payload<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        relation: BamComponent,
        target: FfiEntity,
        payload: &T,
    ) -> FfiResult {
        self.inner
            .insert_relation_with_payload(entity, relation, target, payload)
    }

    /// Queues deferred removal of a typed relationship edge `entity --[relation]--> target`
    /// (`BamHostApi::cmd_remove_relation`).
    #[inline]
    pub fn remove_relation(
        &mut self,
        entity: FfiEntity,
        relation: BamComponent,
        target: FfiEntity,
    ) -> FfiResult {
        self.inner.remove_relation(entity, relation, target)
    }

    /// Captures a raw command-buffer checkpoint token via `BamHostApi::cmd_checkpoint`.
    #[inline]
    pub fn checkpoint(&mut self) -> Result<usize, FfiResult> {
        self.inner.checkpoint()
    }

    /// Rolls back all deferred commands queued after `checkpoint` via `BamHostApi::cmd_rollback`.
    #[inline]
    pub fn rollback(&mut self, checkpoint: usize) -> FfiResult {
        self.inner.rollback(checkpoint)
    }

    /// Starts a scoped [`GuestTransaction`] capturing a checkpoint on this command buffer.
    ///
    /// If the returned [`GuestTransaction`] is dropped without calling
    /// [`GuestTransaction::commit`], all queued commands since `begin_transaction` are
    /// automatically rolled back via `BamHostApi::cmd_rollback`.
    #[inline]
    pub fn begin_transaction(&mut self) -> Result<GuestTransaction<'_>, FfiResult> {
        GuestTransaction::begin(self)
    }

    /// Executes `f` inside a scoped [`GuestTransaction`]: captures a checkpoint via
    /// `cmd_checkpoint` on entry, commits automatically when `f` returns `Ok(val)`, and rolls back
    /// automatically via `cmd_rollback` when `f` returns `Err(err)` or unwinds.
    pub fn transaction<R, F>(&mut self, f: F) -> Result<R, FfiResult>
    where
        F: FnOnce(&mut GuestTransaction<'_>) -> Result<R, FfiResult>,
    {
        let mut tx = self.begin_transaction()?;
        match f(&mut tx) {
            Ok(val) => {
                tx.commit();
                Ok(val)
            }
            Err(err) => {
                let _ = tx.rollback();
                Err(err)
            }
        }
    }
}

/// Scoped transactional deferred command buffer backed by `BamHostApi::cmd_checkpoint` and
/// `BamHostApi::cmd_rollback`.
///
/// Created via [`GuestCommands::begin_transaction`] or [`GuestCommands::transaction`]. Uncommitted
/// transactions automatically roll back when dropped (including on early `?` exits or panics),
/// while [`Self::commit`] keeps all queued commands and [`Self::rollback`] explicitly discards
/// them and returns the host's [`FfiResult`].
pub struct GuestTransaction<'a> {
    commands: GuestCommands<'a>,
    checkpoint: usize,
    settled: bool,
}

impl<'a> GuestTransaction<'a> {
    /// Begins a new transaction on `commands` by capturing a `cmd_checkpoint` token.
    #[inline]
    pub fn begin(commands: &'a mut GuestCommands<'_>) -> Result<Self, FfiResult> {
        let mut reborrowed = GuestCommands::from_validated(commands.inner.reborrow());
        let checkpoint = reborrowed.inner.checkpoint()?;
        Ok(Self {
            commands: reborrowed,
            checkpoint,
            settled: false,
        })
    }

    /// Returns the opaque checkpoint token captured when this transaction began.
    #[inline]
    pub const fn checkpoint_token(&self) -> usize {
        self.checkpoint
    }

    /// Borrows the underlying [`GuestCommands`] immutably.
    #[inline]
    pub const fn as_commands(&self) -> &GuestCommands<'a> {
        &self.commands
    }

    /// Borrows the underlying [`GuestCommands`] mutably.
    #[inline]
    pub fn as_commands_mut(&mut self) -> &mut GuestCommands<'a> {
        &mut self.commands
    }

    /// Commits all deferred commands queued within this transaction so dropping it will not roll
    /// them back.
    #[inline]
    pub fn commit(mut self) {
        self.settled = true;
    }

    /// Explicitly rolls back all deferred commands queued since this transaction began and marks
    /// the transaction settled so [`Drop`] does not invoke rollback a second time.
    #[inline]
    pub fn rollback(mut self) -> FfiResult {
        self.settled = true;
        self.commands.inner.rollback(self.checkpoint)
    }

    /// Returns `true` if the underlying command buffer holds a non-null [`BamCommands`] handle.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.commands.is_valid()
    }

    /// Returns the cached [`Host`] table for this mod instance.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.commands.host()
    }

    /// Returns the raw `*mut BamCommands` handle for FFI interoperability.
    #[inline]
    pub const fn as_raw_ptr(&self) -> *mut BamCommands {
        self.commands.as_raw_ptr()
    }

    /// Resolves a [`BamComponent`] handle by `name`, `uuid`, or both via the host registry.
    #[inline]
    pub fn resolve_component(
        &self,
        name: Option<&CStr>,
        uuid: Option<ComponentUuid>,
    ) -> Result<BamComponent, FfiResult> {
        self.commands.resolve_component(name, uuid)
    }

    /// Resolves a dynamic [`ModComponent`] handle on the host.
    #[inline]
    pub fn resolve_dynamic_component<T: ModComponent>(&self) -> Result<BamComponent, FfiResult> {
        self.commands.resolve_dynamic_component::<T>()
    }

    /// Reserves a fresh empty entity atomically and returns its [`FfiEntity`] identifier.
    #[inline]
    pub fn spawn_empty(&mut self) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn_empty()
    }

    /// Reserves a fresh empty entity whose lifetime persists across mod unload.
    #[inline]
    pub fn spawn_empty_persistent(&mut self) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn_empty_persistent()
    }

    /// Reserves a fresh entity and batch-inserts `entries` in a single archetype transition.
    #[inline]
    pub fn spawn(&mut self, entries: &[SpawnEntry<'_>]) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn(entries)
    }

    /// Reserves a persistent entity and batch-inserts `entries` in a single archetype transition.
    #[inline]
    pub fn spawn_persistent(&mut self, entries: &[SpawnEntry<'_>]) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn_persistent(entries)
    }

    /// Spawns `entity_count` entities in one FFI call using Structure-of-Arrays columns.
    #[inline]
    pub fn spawn_batch(
        &mut self,
        columns: &[SpawnBatchColumn<'_>],
        entity_count: usize,
    ) -> Result<Vec<FfiEntity>, FfiResult> {
        self.commands.spawn_batch(columns, entity_count)
    }

    /// Spawns `entity_count` entities using Structure-of-Arrays columns into `out_entities`.
    #[inline]
    pub fn spawn_batch_into(
        &mut self,
        columns: &[SpawnBatchColumn<'_>],
        entity_count: usize,
        out_entities: Option<&mut [FfiEntity]>,
    ) -> Result<(), FfiResult> {
        self.commands
            .spawn_batch_into(columns, entity_count, out_entities)
    }

    /// Queues deferred despawning of `entity`.
    #[inline]
    pub fn despawn(&mut self, entity: FfiEntity) -> FfiResult {
        self.commands.despawn(entity)
    }

    /// Queues a deferred query-filtered bulk despawn of entities matching `terms`.
    #[inline]
    pub fn despawn_all(&mut self, terms: &[BamTermDesc]) -> Result<(), FfiResult> {
        self.commands.despawn_all(terms)
    }

    /// Queues deferred insertion of `value` (`T: AbiPod`) onto `entity`.
    #[inline]
    pub fn insert<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: &T,
    ) -> FfiResult {
        self.commands.insert(entity, component, value)
    }

    /// Queues deferred insertion of host-constructed `T::default()` onto `entity`.
    #[inline]
    pub fn insert_default(&mut self, entity: FfiEntity, component: BamComponent) -> FfiResult {
        self.commands.insert_default(entity, component)
    }

    /// Queues deferred conditional insertion (`InsertMode::Keep`) onto `entity`.
    #[inline]
    pub fn insert_if_new<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: &T,
    ) -> Result<(), FfiResult> {
        self.commands.insert_if_new(entity, component, value)
    }

    /// Queues deferred removal of `component` from `entity`.
    #[inline]
    pub fn remove(&mut self, entity: FfiEntity, component: BamComponent) -> FfiResult {
        self.commands.remove(entity, component)
    }

    /// Resolves `T: GuestComponent` and queues deferred insertion onto `entity`.
    #[inline]
    pub fn insert_component<T: GuestComponent>(
        &mut self,
        entity: FfiEntity,
        value: &T,
    ) -> FfiResult {
        self.commands.insert_component(entity, value)
    }

    /// Resolves `T: GuestComponent` and queues deferred default insertion onto `entity`.
    #[inline]
    pub fn insert_component_default<T: GuestComponent>(&mut self, entity: FfiEntity) -> FfiResult {
        self.commands.insert_component_default::<T>(entity)
    }

    /// Resolves `T: GuestComponent` and queues deferred conditional insertion onto `entity`.
    #[inline]
    pub fn insert_component_if_new<T: GuestComponent>(
        &mut self,
        entity: FfiEntity,
        value: &T,
    ) -> Result<(), FfiResult> {
        self.commands.insert_component_if_new(entity, value)
    }

    /// Resolves `T: GuestComponent` and queues deferred removal from `entity`.
    #[inline]
    pub fn remove_component<T: GuestComponent>(&mut self, entity: FfiEntity) -> FfiResult {
        self.commands.remove_component::<T>(entity)
    }

    /// Queues deferred insertion of a marker/tag relationship edge `entity --[relation]--> target`.
    #[inline]
    pub fn insert_relation(
        &mut self,
        entity: FfiEntity,
        relation: BamComponent,
        target: FfiEntity,
    ) -> FfiResult {
        self.commands.insert_relation(entity, relation, target)
    }

    /// Queues deferred insertion of a payload-carrying relationship edge
    /// `entity --[relation(payload)]--> target` (`T: AbiPod`).
    #[inline]
    pub fn insert_relation_with_payload<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        relation: BamComponent,
        target: FfiEntity,
        payload: &T,
    ) -> FfiResult {
        self.commands
            .insert_relation_with_payload(entity, relation, target, payload)
    }

    /// Queues deferred removal of a typed relationship edge `entity --[relation]--> target`.
    #[inline]
    pub fn remove_relation(
        &mut self,
        entity: FfiEntity,
        relation: BamComponent,
        target: FfiEntity,
    ) -> FfiResult {
        self.commands.remove_relation(entity, relation, target)
    }

    /// Attaches `FfiChildOf::new(parent)` onto `child`.
    #[inline]
    pub fn add_child(&mut self, parent: FfiEntity, child: FfiEntity) -> FfiResult {
        self.commands.add_child(parent, child)
    }

    /// Removes `FfiChildOf` from `child`.
    #[inline]
    pub fn remove_parent(&mut self, child: FfiEntity) -> FfiResult {
        self.commands.remove_parent(child)
    }

    /// Spawns a new entity parented under `parent` with `extra_entries`.
    #[inline]
    pub fn spawn_child(
        &mut self,
        parent: FfiEntity,
        extra_entries: &[SpawnEntry<'_>],
    ) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn_child(parent, extra_entries)
    }

    /// Moves an owned dynamic [`ModComponent`] `value` onto `entity`.
    #[inline]
    pub fn insert_dynamic<T: ModComponent>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: T,
    ) -> FfiResult {
        self.commands.insert_dynamic(entity, component, value)
    }

    /// Resolves `T: ModComponent` and moves `value` onto `entity`.
    #[inline]
    pub fn insert_dynamic_component<T: ModComponent>(
        &mut self,
        entity: FfiEntity,
        value: T,
    ) -> FfiResult {
        self.commands.insert_dynamic_component(entity, value)
    }

    /// Spawns a mod-scoped entity carrying owned dynamic [`ModComponent`] `value`.
    #[inline]
    pub fn spawn_dynamic<T: ModComponent>(
        &mut self,
        component: BamComponent,
        value: T,
    ) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn_dynamic(component, value)
    }

    /// Resolves `T: ModComponent` and spawns a mod-scoped entity carrying `value`.
    #[inline]
    pub fn spawn_dynamic_component<T: ModComponent>(
        &mut self,
        value: T,
    ) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn_dynamic_component(value)
    }

    /// Spawns a persistent entity carrying owned dynamic [`ModComponent`] `value`.
    #[inline]
    pub fn spawn_dynamic_persistent<T: ModComponent>(
        &mut self,
        component: BamComponent,
        value: T,
    ) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn_dynamic_persistent(component, value)
    }

    /// Resolves `T: ModComponent` and spawns a persistent entity carrying `value`.
    #[inline]
    pub fn spawn_dynamic_component_persistent<T: ModComponent>(
        &mut self,
        value: T,
    ) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn_dynamic_component_persistent(value)
    }

    /// Spawns a mod-scoped entity carrying owned dynamic `value` alongside `extra` components.
    #[inline]
    pub fn spawn_with_dynamic<T: ModComponent>(
        &mut self,
        component: BamComponent,
        value: T,
        extra: &[SpawnEntry<'_>],
    ) -> Result<FfiEntity, FfiResult> {
        self.commands.spawn_with_dynamic(component, value, extra)
    }

    /// Spawns a persistent entity carrying owned dynamic `value` alongside `extra` components.
    #[inline]
    pub fn spawn_with_dynamic_persistent<T: ModComponent>(
        &mut self,
        component: BamComponent,
        value: T,
        extra: &[SpawnEntry<'_>],
    ) -> Result<FfiEntity, FfiResult> {
        self.commands
            .spawn_with_dynamic_persistent(component, value, extra)
    }

    /// Triggers a global Bevy `Event` (`E: GuestComponent`).
    #[inline]
    pub fn trigger<E: GuestComponent>(&mut self, event: &E) -> Result<(), FfiResult> {
        self.commands.trigger(event)
    }

    /// Triggers an entity-targeted Bevy `EntityEvent` (`E: GuestComponent`) on `target`.
    #[inline]
    pub fn trigger_entity<E: GuestComponent>(
        &mut self,
        target: FfiEntity,
        event: &E,
    ) -> Result<(), FfiResult> {
        self.commands.trigger_entity(target, event)
    }

    /// Triggers a pre-resolved event handle `handle` on `target`.
    #[inline]
    pub fn trigger_handle<E: AbiPod>(
        &mut self,
        handle: BamComponent,
        target: FfiEntity,
        event: &E,
    ) -> Result<(), FfiResult> {
        self.commands.trigger_handle(handle, target, event)
    }

    /// Triggers a propagating `EntityEvent` `E` upward along `ChildOf`.
    #[inline]
    pub fn trigger_propagated<E: GuestComponent>(
        &mut self,
        target: FfiEntity,
        event: &E,
    ) -> Result<(), FfiResult> {
        self.commands.trigger_propagated(target, event)
    }

    /// Triggers a propagating `EntityEvent` `E` upward along custom relationship `R`.
    #[inline]
    pub fn trigger_propagated_along<E: GuestComponent, R: GuestComponent>(
        &mut self,
        target: FfiEntity,
        event: &E,
    ) -> Result<(), FfiResult> {
        self.commands
            .trigger_propagated_along::<E, R>(target, event)
    }

    /// Triggers a pre-resolved event handle with upward propagation enabled.
    #[inline]
    pub fn trigger_propagated_handle<E: AbiPod>(
        &mut self,
        event_handle: BamComponent,
        target: FfiEntity,
        event: &E,
        propagate_relationship: BamComponent,
    ) -> Result<(), FfiResult> {
        self.commands
            .trigger_propagated_handle(event_handle, target, event, propagate_relationship)
    }

    /// Queues a synthetic lifecycle or `Ready` trigger.
    #[inline]
    pub fn trigger_lifecycle(
        &mut self,
        kind: BamEventKind,
        target: FfiEntity,
        watched_components: &[BamComponent],
    ) -> Result<(), FfiResult> {
        self.commands
            .trigger_lifecycle(kind, target, watched_components)
    }

    /// Queues a synthetic scene `Ready` trigger for `target`.
    #[inline]
    pub fn trigger_ready(&mut self, target: FfiEntity) -> Result<(), FfiResult> {
        self.commands.trigger_ready(target)
    }

    /// Queues a propagating `Ready` trigger upward along `ChildOf`.
    #[inline]
    pub fn trigger_ready_propagated(&mut self, target: FfiEntity) -> Result<(), FfiResult> {
        self.commands.trigger_ready_propagated(target)
    }

    /// Queues a propagating `Ready` trigger upward along custom relationship `R`.
    #[inline]
    pub fn trigger_ready_propagated_along<R: GuestComponent>(
        &mut self,
        target: FfiEntity,
    ) -> Result<(), FfiResult> {
        self.commands.trigger_ready_propagated_along::<R>(target)
    }

    /// Queues deferred spawning of an entity-scoped runtime observer for `E`.
    #[inline]
    pub fn observe_entity<E, F>(
        &mut self,
        target: FfiEntity,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        E: GuestComponent,
        F: FnMut(FfiEntity, &E, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.commands.observe_entity(target, callback)
    }

    /// Named variant of [`Self::observe_entity`].
    #[inline]
    pub fn observe_entity_named<E, F>(
        &mut self,
        name: Option<&str>,
        target: FfiEntity,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        E: GuestComponent,
        F: FnMut(FfiEntity, &E, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.commands.observe_entity_named(name, target, callback)
    }

    /// Queues deferred spawning of a runtime observer for pre-resolved handle `event`.
    #[inline]
    pub fn observe_handle_runtime<E, F>(
        &mut self,
        name: Option<&str>,
        event: BamComponent,
        target: FfiEntity,
        flags: u32,
        propagate_relationship: BamComponent,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        E: AbiPod,
        F: FnMut(FfiEntity, &E, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.commands.observe_handle_runtime(
            name,
            event,
            target,
            flags,
            propagate_relationship,
            callback,
        )
    }

    /// Queues deferred spawning of a mutable entity-scoped runtime observer for `E`.
    #[inline]
    pub fn observe_entity_mut<E, F>(
        &mut self,
        target: FfiEntity,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        E: GuestComponent,
        F: FnMut(FfiEntity, &mut E, &mut bool, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.commands.observe_entity_mut(target, callback)
    }

    /// Queues deferred spawning of a runtime lifecycle or `Ready` observer.
    #[inline]
    pub fn observe_entity_lifecycle<F>(
        &mut self,
        event_kind: BamEventKind,
        watched_components: &[BamComponent],
        target: FfiEntity,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.commands
            .observe_entity_lifecycle(event_kind, watched_components, target, callback)
    }

    /// Queues deferred spawning of a runtime `Ready` observer on `target`.
    #[inline]
    pub fn observe_entity_ready<F>(
        &mut self,
        target: FfiEntity,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.commands.observe_entity_ready(target, callback)
    }

    /// Queues deferred spawning of a runtime `Ready` observer with propagation options.
    #[inline]
    pub fn observe_entity_ready_with<F>(
        &mut self,
        target: FfiEntity,
        propagate: bool,
        propagate_relationship: BamComponent,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        F: FnMut(FfiEntity, &mut GuestCommands<'_>) + Send + 'static,
    {
        self.commands
            .observe_entity_ready_with(target, propagate, propagate_relationship, callback)
    }

    /// Begins a nested sub-transaction inside this transaction.
    #[inline]
    pub fn begin_transaction(&mut self) -> Result<GuestTransaction<'_>, FfiResult> {
        self.commands.begin_transaction()
    }

    /// Runs `f` inside a nested sub-transaction: rolls back only the inner commands on `Err`.
    #[inline]
    pub fn transaction<R, F>(&mut self, f: F) -> Result<R, FfiResult>
    where
        F: FnOnce(&mut GuestTransaction<'_>) -> Result<R, FfiResult>,
    {
        self.commands.transaction(f)
    }
}

impl<'a> Deref for GuestTransaction<'a> {
    type Target = GuestCommands<'a>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.commands
    }
}

impl DerefMut for GuestTransaction<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.commands
    }
}

impl Drop for GuestTransaction<'_> {
    #[inline]
    fn drop(&mut self) {
        if !self.settled {
            self.settled = true;
            let _ = self.commands.inner.rollback(self.checkpoint);
        }
    }
}

impl<'a> From<ValidatedCommands<'a>> for GuestCommands<'a> {
    #[inline]
    fn from(inner: ValidatedCommands<'a>) -> Self {
        Self::from_validated(inner)
    }
}
