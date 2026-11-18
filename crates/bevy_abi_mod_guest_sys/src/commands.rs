//! Validated handle-based and batched deferred command wrappers over [`BamCommands`].

use core::marker::PhantomData;

use bevy_abi_mod_sys::{
    AbiPod, BamCmdDespawnAllFn, BamCmdInsertFn, BamCmdObserveFn, BamCmdSpawnBatchFn, BamCmdSpawnFn,
    BamCmdSpawnSceneFn, BamCmdTriggerExFn, BamCmdTriggerFn, BamCommands, BamComponent,
    BamEventKind, BamHostApi, BamObserverDesc, BamTermDesc, BamTriggerDesc, FfiAssetHandle,
    FfiEntity, FfiResult,
};

use crate::{
    host::Host,
    trampoline::{
        allocate_runtime_lifecycle_observer_desc, allocate_runtime_observer_desc,
        allocate_runtime_observer_mut_desc,
    },
};

/// Stack scratch capacity before falling back to a temporary `Vec` in [`ValidatedCommands::spawn`].
const INLINE_SPAWN_CAP: usize = 16;

/// Type-erased `(BamComponent, *const u8)` pair for batched single-archetype [`ValidatedCommands::spawn`].
///
/// Constructed safely via [`SpawnEntry::new`] from any `T: AbiPod` reference without materializing
/// `&[u8]` over struct padding bytes.
#[derive(Clone, Copy, Debug)]
pub struct SpawnEntry<'a> {
    component: BamComponent,
    data_ptr: *const u8,
    _marker: PhantomData<&'a ()>,
}

impl<'a> SpawnEntry<'a> {
    /// Binds a pre-resolved [`BamComponent`] handle to an `AbiPod` component value for batched spawn.
    #[inline]
    pub const fn new<T: AbiPod>(component: BamComponent, value: &'a T) -> Self {
        Self {
            component,
            data_ptr: core::ptr::from_ref(value).cast::<u8>(),
            _marker: PhantomData,
        }
    }

    /// Binds a pre-resolved zero-sized marker [`BamComponent`] handle (`size == 0`) with a non-null
    /// dangling data pointer.
    #[inline]
    pub const fn marker(component: BamComponent) -> Self {
        Self {
            component,
            data_ptr: core::ptr::NonNull::<u8>::dangling().as_ptr(),
            _marker: PhantomData,
        }
    }

    /// Constructs a raw `SpawnEntry` from a caller-managed byte pointer.
    ///
    /// # Safety
    /// If the registered size of `component` is non-zero, `data_ptr` must be non-null and readable
    /// for that many bytes for lifetime `'a`.
    #[inline]
    pub const unsafe fn from_raw(component: BamComponent, data_ptr: *const u8) -> Self {
        Self {
            component,
            data_ptr,
            _marker: PhantomData,
        }
    }

    /// Returns the [`BamComponent`] handle for this entry.
    #[inline]
    pub const fn component(&self) -> BamComponent {
        self.component
    }

    /// Returns the raw payload pointer for this entry.
    #[inline]
    pub const fn data_ptr(&self) -> *const u8 {
        self.data_ptr
    }
}

/// Columnar slice descriptor `(BamComponent, *const u8, len)` for multi-entity
/// [`ValidatedCommands::spawn_batch_columns`].
#[derive(Clone, Copy, Debug)]
pub struct SpawnBatchColumn<'a> {
    component: BamComponent,
    column_ptr: *const u8,
    entity_count: usize,
    _marker: PhantomData<&'a ()>,
}

impl<'a> SpawnBatchColumn<'a> {
    /// Binds a contiguous slice of `values` (`T: AbiPod`) to `component` for multi-entity spawn.
    #[inline]
    pub const fn new<T: AbiPod>(component: BamComponent, values: &'a [T]) -> Self {
        let ptr = if core::mem::size_of::<T>() == 0 || values.is_empty() {
            core::ptr::NonNull::<u8>::dangling().as_ptr().cast_const()
        } else {
            values.as_ptr().cast::<u8>()
        };
        Self {
            component,
            column_ptr: ptr,
            entity_count: values.len(),
            _marker: PhantomData,
        }
    }

    /// Binds a zero-sized marker `component` repeated across `entity_count` spawned entities.
    #[inline]
    pub const fn marker(component: BamComponent, entity_count: usize) -> Self {
        Self {
            component,
            column_ptr: core::ptr::NonNull::<u8>::dangling().as_ptr().cast_const(),
            entity_count,
            _marker: PhantomData,
        }
    }

    /// Constructs a raw [`SpawnBatchColumn`] from a caller-managed contiguous column byte pointer.
    ///
    /// # Safety
    /// If the registered size `S` of `component` is non-zero and `entity_count > 0`, `column_ptr`
    /// must be non-null and readable for `entity_count * S` bytes for lifetime `'a`.
    #[inline]
    pub const unsafe fn from_raw(
        component: BamComponent,
        column_ptr: *const u8,
        entity_count: usize,
    ) -> Self {
        Self {
            component,
            column_ptr,
            entity_count,
            _marker: PhantomData,
        }
    }

    /// Returns the [`BamComponent`] handle for this column.
    #[inline]
    pub const fn component(&self) -> BamComponent {
        self.component
    }

    /// Returns the raw column byte pointer.
    #[inline]
    pub const fn column_ptr(&self) -> *const u8 {
        self.column_ptr
    }

    /// Returns the number of entity rows carried in this column slice.
    #[inline]
    pub const fn entity_count(&self) -> usize {
        self.entity_count
    }
}

/// Validated deferred command buffer wrapper bound to an active system invocation.
pub struct ValidatedCommands<'a> {
    raw: *mut BamCommands,
    host: &'a Host,
}

impl<'a> ValidatedCommands<'a> {
    /// Wraps a raw [`BamCommands`] pointer obtained from `BamHostApi::sys_commands`.
    ///
    /// A null `raw` pointer constructs a disabled buffer whose command methods fail cleanly with
    /// [`FfiResult::NULL_POINTER`].
    ///
    /// # Safety
    /// If `raw` is non-null, it must point to the active [`BamCommands`] belonging to the current
    /// system invocation on `host` and must not outlive `'a`.
    #[inline]
    pub const unsafe fn from_raw(raw: *mut BamCommands, host: &'a Host) -> Self {
        Self { raw, host }
    }

    /// Constructs a disabled command buffer (`raw = null`) that rejects operations with
    /// [`FfiResult::NULL_POINTER`].
    #[inline]
    pub const fn empty(host: &'a Host) -> Self {
        Self {
            raw: core::ptr::null_mut(),
            host,
        }
    }

    /// Returns `true` if this command buffer holds a non-null [`BamCommands`] handle.
    #[inline]
    pub fn is_valid(&self) -> bool {
        !self.raw.is_null()
    }

    /// Returns the underlying `*mut BamCommands` pointer.
    #[inline]
    pub const fn as_raw(&self) -> *mut BamCommands {
        self.raw
    }

    /// Returns the cached [`Host`] table for this system invocation.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.host
    }

    /// Reserves a fresh empty entity atomically and returns its [`FfiEntity`] identifier.
    #[inline]
    pub fn spawn_empty(&mut self) -> Result<FfiEntity, FfiResult> {
        if self.raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        let Some(spawn_empty_fn) = self.host.api().cmd_spawn_empty else {
            return Err(FfiResult::NULL_POINTER);
        };
        let mut out = FfiEntity::NULL;
        // SAFETY: `self.raw` is a valid active `BamCommands` for `'a` and `out` is writable stack storage.
        let status = unsafe { spawn_empty_fn(self.raw, &mut out) };
        if status.is_ok() { Ok(out) } else { Err(status) }
    }

    /// Reserves an entity and batch-inserts `entries` in a single archetype placement via
    /// `BamHostApi::cmd_spawn`.
    ///
    /// Note: The caller must ensure each [`SpawnEntry`]'s `T` matches the exact byte size and layout
    /// registered for its [`BamComponent`] handle.
    pub fn spawn(&mut self, entries: &[SpawnEntry<'_>]) -> Result<FfiEntity, FfiResult> {
        if entries.len() <= INLINE_SPAWN_CAP {
            let mut comps = [BamComponent::INVALID; INLINE_SPAWN_CAP];
            let mut ptrs = [core::ptr::null::<u8>(); INLINE_SPAWN_CAP];
            for (i, entry) in entries.iter().enumerate() {
                if !entry.component.is_valid() {
                    return Err(FfiResult::INVALID_COMPONENT);
                }
                comps[i] = entry.component;
                ptrs[i] = entry.data_ptr;
            }
            // SAFETY: Each `SpawnEntry` holds a valid `T: AbiPod` reference or caller-verified raw
            // payload for the duration of `entries`, and slice lengths match `entries.len()`.
            unsafe { self.spawn_raw(&comps[..entries.len()], &ptrs[..entries.len()]) }
        } else {
            let mut comps = Vec::with_capacity(entries.len());
            let mut ptrs = Vec::with_capacity(entries.len());
            for entry in entries {
                if !entry.component.is_valid() {
                    return Err(FfiResult::INVALID_COMPONENT);
                }
                comps.push(entry.component);
                ptrs.push(entry.data_ptr);
            }
            // SAFETY: `comps` and `ptrs` have identical length and point to live `SpawnEntry` payloads.
            unsafe { self.spawn_raw(&comps, &ptrs) }
        }
    }

    /// Raw batched entity spawn forwarding parallel `components` and `data_ptrs` slices to
    /// `BamHostApi::cmd_spawn`.
    ///
    /// # Safety
    /// For every `i` in `0..components.len()`, if the registered byte size of `components[i]` is
    /// non-zero, `data_ptrs[i]` must be non-null and readable for that registered byte size.
    pub unsafe fn spawn_raw(
        &mut self,
        components: &[BamComponent],
        data_ptrs: &[*const u8],
    ) -> Result<FfiEntity, FfiResult> {
        if self.raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        if components.len() != data_ptrs.len() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let Some(spawn_fn) = self.host.api().cmd_spawn else {
            return Err(FfiResult::NULL_POINTER);
        };
        let count = components.len();
        let comps_ptr = if count == 0 {
            core::ptr::null()
        } else {
            components.as_ptr()
        };
        let ptrs_ptr = if count == 0 {
            core::ptr::null()
        } else {
            data_ptrs.as_ptr()
        };
        let mut out = FfiEntity::NULL;
        // SAFETY: `self.raw` is valid, `comps_ptr`/`ptrs_ptr` cover `count` elements, and caller
        // guarantees each `data_ptrs[i]` matches `components[i]`'s registered size.
        let status = unsafe { spawn_fn(self.raw, comps_ptr, ptrs_ptr, count, &mut out) };
        if status.is_ok() { Ok(out) } else { Err(status) }
    }

    /// Queues deferred despawning of `entity`.
    #[inline]
    pub fn despawn(&mut self, entity: FfiEntity) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if entity.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        let Some(despawn_fn) = self.host.api().cmd_despawn else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is an active `BamCommands` handle for `'a`.
        unsafe { despawn_fn(self.raw, entity) }
    }

    /// Queues deferred insertion of `value` (`T: AbiPod`) onto `entity` using pre-resolved
    /// `component` handle.
    #[inline]
    pub fn insert<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: &T,
    ) -> FfiResult {
        let data_ptr = core::ptr::from_ref(value).cast::<u8>();
        let data_len = core::mem::size_of::<T>();
        // SAFETY: `value` is a live `&T` (`T: AbiPod`) readable via raw byte copy for `size_of::<T>()`
        // bytes without forming `&[u8]` over any padding bytes.
        unsafe { self.insert_raw(entity, component, data_ptr, data_len) }
    }

    /// Queues deferred raw byte insertion (`data_ptr[0..data_len]`) for `component` onto `entity`.
    ///
    /// # Safety
    /// If `data_len > 0`, `data_ptr` must be non-null and readable for `data_len` bytes matching
    /// the registered component layout (and ownership invariants if `component` has a `drop_fn`).
    #[inline]
    pub unsafe fn insert_raw(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        data_ptr: *const u8,
        data_len: usize,
    ) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if entity.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        if !component.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        if data_len > 0 && data_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let Some(insert_fn) = self.host.api().cmd_insert else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is active and caller guarantees `data_ptr` is readable for `data_len` bytes.
        unsafe { insert_fn(self.raw, entity, component, data_ptr, data_len) }
    }

    /// Queues deferred insertion of host-constructed `T::default()` for `component` onto `entity`.
    #[inline]
    pub fn insert_default(&mut self, entity: FfiEntity, component: BamComponent) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if entity.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        if !component.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        let Some(insert_default_fn) = self.host.api().cmd_insert_default else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is active and `entity`/`component` were checked non-zero.
        unsafe { insert_default_fn(self.raw, entity, component) }
    }

    /// Queues deferred removal of `component` from `entity`.
    #[inline]
    pub fn remove(&mut self, entity: FfiEntity, component: BamComponent) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if entity.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        if !component.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        let Some(remove_fn) = self.host.api().cmd_remove else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is active and `entity`/`component` were checked non-zero.
        unsafe { remove_fn(self.raw, entity, component) }
    }

    /// Queues deferred `world.trigger(...)` or `world.trigger_dynamic*` for pre-resolved `event`.
    ///
    /// Pass [`FfiEntity::NULL`] for global event triggers or a valid target [`FfiEntity`] for
    /// entity-targeted triggers.
    ///
    /// # Safety
    /// If `data_len > 0`, `data_ptr` must be non-null and point to `data_len` initialized readable bytes.
    pub unsafe fn trigger_raw(
        &mut self,
        event: BamComponent,
        target: FfiEntity,
        data_ptr: *const u8,
        data_len: usize,
    ) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if !event.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        if data_len > 0 && data_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, cmd_trigger),
            core::mem::size_of::<Option<BamCmdTriggerFn>>(),
        ) {
            return FfiResult::UNSUPPORTED;
        }
        let Some(trigger_fn) = self.host.api().cmd_trigger else {
            return FfiResult::UNSUPPORTED;
        };
        // SAFETY: `self.raw` is active and caller guarantees `data_ptr` is readable for `data_len` bytes.
        unsafe { trigger_fn(self.raw, event, target, data_ptr, data_len) }
    }

    /// Queues a deferred event trigger of `value` (`T: AbiPod`) under pre-resolved handle `event`.
    #[inline]
    pub fn trigger<T: AbiPod>(
        &mut self,
        event: BamComponent,
        target: FfiEntity,
        value: &T,
    ) -> FfiResult {
        let data_ptr = if core::mem::size_of::<T>() == 0 {
            core::ptr::NonNull::<u8>::dangling().as_ptr().cast_const()
        } else {
            core::ptr::from_ref(value).cast::<u8>()
        };
        // SAFETY: `value` is `T: AbiPod` and valid for `size_of::<T>()` initialized bytes.
        unsafe { self.trigger_raw(event, target, data_ptr, core::mem::size_of::<T>()) }
    }

    /// Reserves an entity that survives mod unload (`BamHostApi::cmd_spawn_persistent`) and
    /// batch-inserts `entries`.
    pub fn spawn_persistent(&mut self, entries: &[SpawnEntry<'_>]) -> Result<FfiEntity, FfiResult> {
        if entries.len() <= INLINE_SPAWN_CAP {
            let mut comps = [BamComponent::INVALID; INLINE_SPAWN_CAP];
            let mut ptrs = [core::ptr::null::<u8>(); INLINE_SPAWN_CAP];
            for (i, entry) in entries.iter().enumerate() {
                if !entry.component.is_valid() {
                    return Err(FfiResult::INVALID_COMPONENT);
                }
                comps[i] = entry.component;
                ptrs[i] = entry.data_ptr;
            }
            // SAFETY: Each `SpawnEntry` holds a valid payload for the duration of `entries`.
            unsafe { self.spawn_persistent_raw(&comps[..entries.len()], &ptrs[..entries.len()]) }
        } else {
            let mut comps = Vec::with_capacity(entries.len());
            let mut ptrs = Vec::with_capacity(entries.len());
            for entry in entries {
                if !entry.component.is_valid() {
                    return Err(FfiResult::INVALID_COMPONENT);
                }
                comps.push(entry.component);
                ptrs.push(entry.data_ptr);
            }
            // SAFETY: `comps` and `ptrs` have identical length and point to live `SpawnEntry` payloads.
            unsafe { self.spawn_persistent_raw(&comps, &ptrs) }
        }
    }

    /// Raw persistent batched entity spawn forwarding parallel `components` and `data_ptrs` slices
    /// to `BamHostApi::cmd_spawn_persistent`.
    ///
    /// # Safety
    /// For every `i` in `0..components.len()`, if the registered byte size of `components[i]` is
    /// non-zero, `data_ptrs[i]` must be non-null and readable for that registered byte size.
    pub unsafe fn spawn_persistent_raw(
        &mut self,
        components: &[BamComponent],
        data_ptrs: &[*const u8],
    ) -> Result<FfiEntity, FfiResult> {
        if self.raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        if components.len() != data_ptrs.len() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, cmd_spawn_persistent),
            core::mem::size_of::<Option<BamCmdSpawnFn>>(),
        ) {
            return Err(FfiResult::UNSUPPORTED);
        }
        let Some(spawn_fn) = self.host.api().cmd_spawn_persistent else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let count = components.len();
        let comps_ptr = if count == 0 {
            core::ptr::null()
        } else {
            components.as_ptr()
        };
        let ptrs_ptr = if count == 0 {
            core::ptr::null()
        } else {
            data_ptrs.as_ptr()
        };
        let mut out = FfiEntity::NULL;
        // SAFETY: `self.raw` is valid, `comps_ptr`/`ptrs_ptr` cover `count` elements, and caller
        // guarantees each `data_ptrs[i]` matches `components[i]`'s registered size.
        let status = unsafe { spawn_fn(self.raw, comps_ptr, ptrs_ptr, count, &mut out) };
        if status.is_ok() { Ok(out) } else { Err(status) }
    }

    /// Spawns `entity_count` entities in a single columnar batch via `BamHostApi::cmd_spawn_batch`,
    /// optionally writing the reserved [`FfiEntity`] handles into `out_entities`.
    pub fn spawn_batch_columns(
        &mut self,
        columns: &[SpawnBatchColumn<'_>],
        entity_count: usize,
        out_entities: Option<&mut [FfiEntity]>,
    ) -> FfiResult {
        if let Some(ref out_slice) = out_entities
            && out_slice.len() < entity_count
        {
            return FfiResult::INVALID_ARGUMENT;
        }
        for col in columns {
            if !col.component.is_valid() {
                return FfiResult::INVALID_COMPONENT;
            }
            if col.entity_count < entity_count {
                return FfiResult::INVALID_ARGUMENT;
            }
        }
        let out_ptr = match out_entities {
            Some(slice) if entity_count > 0 => slice.as_mut_ptr(),
            _ => core::ptr::null_mut(),
        };
        if columns.len() <= INLINE_SPAWN_CAP {
            let mut comps = [BamComponent::INVALID; INLINE_SPAWN_CAP];
            let mut ptrs = [core::ptr::null::<u8>(); INLINE_SPAWN_CAP];
            for (i, col) in columns.iter().enumerate() {
                comps[i] = col.component;
                ptrs[i] = col.column_ptr;
            }
            // SAFETY: Each `SpawnBatchColumn` verified `col.entity_count >= entity_count` and valid pointers.
            unsafe {
                self.spawn_batch_raw(
                    &comps[..columns.len()],
                    &ptrs[..columns.len()],
                    entity_count,
                    out_ptr,
                )
            }
        } else {
            let mut comps = Vec::with_capacity(columns.len());
            let mut ptrs = Vec::with_capacity(columns.len());
            for col in columns {
                comps.push(col.component);
                ptrs.push(col.column_ptr);
            }
            // SAFETY: `comps` and `ptrs` match `columns` and cover `entity_count` rows.
            unsafe { self.spawn_batch_raw(&comps, &ptrs, entity_count, out_ptr) }
        }
    }

    /// Raw columnar multi-entity spawn forwarding directly to `BamHostApi::cmd_spawn_batch`.
    ///
    /// # Safety
    /// - For every `i` in `0..components.len()`, if `entity_count > 0` and the registered size `S`
    ///   of `components[i]` is non-zero, `column_ptrs[i]` must be non-null and readable for
    ///   `entity_count * S` bytes.
    /// - If `out_entities` is non-null and `entity_count > 0`, it must be writable for
    ///   `entity_count` [`FfiEntity`] elements.
    pub unsafe fn spawn_batch_raw(
        &mut self,
        components: &[BamComponent],
        column_ptrs: &[*const u8],
        entity_count: usize,
        out_entities: *mut FfiEntity,
    ) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if components.len() != column_ptrs.len() {
            return FfiResult::INVALID_ARGUMENT;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, cmd_spawn_batch),
            core::mem::size_of::<Option<BamCmdSpawnBatchFn>>(),
        ) {
            return FfiResult::UNSUPPORTED;
        }
        let Some(batch_fn) = self.host.api().cmd_spawn_batch else {
            return FfiResult::UNSUPPORTED;
        };
        let comp_count = components.len();
        let comps_ptr = if comp_count == 0 {
            core::ptr::null()
        } else {
            components.as_ptr()
        };
        let cols_ptr = if comp_count == 0 {
            core::ptr::null()
        } else {
            column_ptrs.as_ptr()
        };
        // SAFETY: `self.raw` is valid and caller guarantees `column_ptrs` and `out_entities` bounds.
        unsafe {
            batch_fn(
                self.raw,
                comps_ptr,
                cols_ptr,
                comp_count,
                entity_count,
                out_entities,
            )
        }
    }

    /// Queues deferred bulk despawning of all entities matching filter-only `terms`
    /// (`FILTER_ONLY` / `WITHOUT` / `ALLOW`) via `BamHostApi::cmd_despawn_all`.
    pub fn despawn_all(&mut self, terms: &[BamTermDesc]) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if terms.is_empty() {
            return FfiResult::INVALID_ARGUMENT;
        }
        for term in terms {
            let status = term.validate();
            if !status.is_ok() {
                return status;
            }
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, cmd_despawn_all),
            core::mem::size_of::<Option<BamCmdDespawnAllFn>>(),
        ) {
            return FfiResult::UNSUPPORTED;
        }
        let Some(despawn_all_fn) = self.host.api().cmd_despawn_all else {
            return FfiResult::UNSUPPORTED;
        };
        // SAFETY: `self.raw` is active and `terms` is a valid slice of validated `BamTermDesc`s.
        unsafe { despawn_all_fn(self.raw, terms.as_ptr(), terms.len()) }
    }

    /// Queues deferred conditional insertion of `value` (`T: AbiPod`) onto `entity` only if
    /// `component` is not already present (`BamHostApi::cmd_insert_if_new`).
    #[inline]
    pub fn insert_if_new<T: AbiPod>(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        value: &T,
    ) -> FfiResult {
        let data_ptr = core::ptr::from_ref(value).cast::<u8>();
        let data_len = core::mem::size_of::<T>();
        // SAFETY: `value` is a live `&T` (`T: AbiPod`) readable for `size_of::<T>()` bytes.
        unsafe { self.insert_if_new_raw(entity, component, data_ptr, data_len) }
    }

    /// Raw deferred conditional insertion (`cmd_insert_if_new`).
    ///
    /// # Safety
    /// If `data_len > 0`, `data_ptr` must be non-null and readable for `data_len` bytes matching
    /// the registered component layout.
    pub unsafe fn insert_if_new_raw(
        &mut self,
        entity: FfiEntity,
        component: BamComponent,
        data_ptr: *const u8,
        data_len: usize,
    ) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if entity.is_null() {
            return FfiResult::INVALID_ENTITY;
        }
        if !component.is_valid() {
            return FfiResult::INVALID_COMPONENT;
        }
        if data_len > 0 && data_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, cmd_insert_if_new),
            core::mem::size_of::<Option<BamCmdInsertFn>>(),
        ) {
            return FfiResult::UNSUPPORTED;
        }
        let Some(insert_fn) = self.host.api().cmd_insert_if_new else {
            return FfiResult::UNSUPPORTED;
        };
        // SAFETY: `self.raw` is active and caller guarantees `data_ptr` covers `data_len` bytes.
        unsafe { insert_fn(self.raw, entity, component, data_ptr, data_len) }
    }

    /// Raw extensible trigger dispatch forwarding `desc` to `BamHostApi::cmd_trigger_ex`.
    ///
    /// # Safety
    /// `desc` must satisfy [`BamTriggerDesc::validate`] and its pointers (`data_ptr`,
    /// `watched_components`) must remain valid for the duration of this call.
    pub unsafe fn trigger_ex_raw(&mut self, desc: &BamTriggerDesc) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let validation = desc.validate();
        if !validation.is_ok() {
            return validation;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, cmd_trigger_ex),
            core::mem::size_of::<Option<BamCmdTriggerExFn>>(),
        ) {
            return FfiResult::UNSUPPORTED;
        }
        let Some(trigger_ex_fn) = self.host.api().cmd_trigger_ex else {
            return FfiResult::UNSUPPORTED;
        };
        // SAFETY: `self.raw` is active and `desc` passed validation per caller contract.
        unsafe { trigger_ex_fn(self.raw, desc) }
    }

    /// Queues an extended custom event trigger (`value: &T`) with optional `flags` (such as
    /// `BAM_TRIGGER_FLAG_PROPAGATE`) and `propagate_relationship`.
    pub fn trigger_ex<T: AbiPod>(
        &mut self,
        event: BamComponent,
        target: FfiEntity,
        value: &T,
        flags: u32,
        propagate_relationship: BamComponent,
    ) -> FfiResult {
        let mut copy = *value;
        let data_ptr = if core::mem::size_of::<T>() == 0 {
            core::ptr::null_mut()
        } else {
            core::ptr::from_mut(&mut copy).cast::<u8>()
        };
        let mut desc = BamTriggerDesc::new(event, data_ptr, core::mem::size_of::<T>())
            .with_entity(target)
            .with_flags(flags);
        if propagate_relationship.is_valid() {
            desc = desc.with_propagate(propagate_relationship);
        }
        // SAFETY: `copy` lives on the stack across `trigger_ex_raw`, which copies payload bytes
        // into the host command arena before returning.
        unsafe { self.trigger_ex_raw(&desc) }
    }

    /// Queues a synthetic lifecycle or `Ready` trigger via `BamHostApi::cmd_trigger_ex`.
    pub fn trigger_lifecycle(
        &mut self,
        kind: BamEventKind,
        target: FfiEntity,
        watched_components: &[BamComponent],
    ) -> FfiResult {
        let comps_ptr = if watched_components.is_empty() {
            core::ptr::null()
        } else {
            watched_components.as_ptr()
        };
        let desc = BamTriggerDesc::new(BamComponent::INVALID, core::ptr::null_mut(), 0)
            .with_event_kind(kind)
            .with_entity(target)
            .with_watched_components(comps_ptr, watched_components.len());
        // SAFETY: `watched_components` slice stays live on the stack for the duration of `trigger_ex_raw`.
        unsafe { self.trigger_ex_raw(&desc) }
    }

    /// Raw runtime observer spawn forwarding `desc` to `BamHostApi::cmd_observe` and returning
    /// the spawned observer [`FfiEntity`].
    ///
    /// # Safety
    /// `desc` must satisfy [`BamObserverDesc::validate`], and its callbacks, `user_data`, `name`,
    /// and `watched_components` must remain valid until the observer entity is despawned or the
    /// owning mod shuts down.
    pub unsafe fn observe_raw(&mut self, desc: &BamObserverDesc) -> Result<FfiEntity, FfiResult> {
        if self.raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        let validation = desc.validate();
        if !validation.is_ok() {
            return Err(validation);
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, cmd_observe),
            core::mem::size_of::<Option<BamCmdObserveFn>>(),
        ) {
            return Err(FfiResult::UNSUPPORTED);
        }
        let Some(observe_fn) = self.host.api().cmd_observe else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let mut out = FfiEntity::NULL;
        // SAFETY: `self.raw` is active, `desc` passed validation, and `out` is writable stack storage.
        let status = unsafe { observe_fn(self.raw, desc, &mut out) };
        if status.is_ok() { Ok(out) } else { Err(status) }
    }

    /// Spawns a runtime read-only custom event observer via `BamHostApi::cmd_observe`, tracking
    /// its closure state inside the owning [`crate::GuestInstance`].
    #[allow(clippy::too_many_arguments)]
    pub fn observe<E, F>(
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
        F: FnMut(FfiEntity, &E, &mut ValidatedCommands<'_>) -> FfiResult + Send + 'static,
    {
        let desc = allocate_runtime_observer_desc::<E, F>(
            self.host,
            name,
            event,
            target,
            flags,
            propagate_relationship,
            callback,
        )?;
        // SAFETY: `desc`'s closure state and optional `name` were pinned in `GuestInstance` via
        // `Host::track_runtime_allocation`.
        unsafe { self.observe_raw(&desc) }
    }

    /// Spawns a runtime mutable custom event observer (`&mut E` + `&mut bool` propagate) via
    /// `BamHostApi::cmd_observe`, tracking its closure state inside the owning [`crate::GuestInstance`].
    #[allow(clippy::too_many_arguments)]
    pub fn observe_mut<E, F>(
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
        F: FnMut(FfiEntity, &mut E, &mut bool, &mut ValidatedCommands<'_>) -> FfiResult
            + Send
            + 'static,
    {
        let desc = allocate_runtime_observer_mut_desc::<E, F>(
            self.host,
            name,
            event,
            target,
            flags,
            propagate_relationship,
            callback,
        )?;
        // SAFETY: `desc`'s closure state and optional `name` were pinned in `GuestInstance`.
        unsafe { self.observe_raw(&desc) }
    }

    /// Spawns a runtime lifecycle or `Ready` observer via `BamHostApi::cmd_observe`, tracking its
    /// closure state and watched component list inside the owning [`crate::GuestInstance`].
    #[allow(clippy::too_many_arguments)]
    pub fn observe_lifecycle<F>(
        &mut self,
        name: Option<&str>,
        event_kind: BamEventKind,
        watched_components: &[BamComponent],
        target: FfiEntity,
        flags: u32,
        propagate_relationship: BamComponent,
        callback: F,
    ) -> Result<FfiEntity, FfiResult>
    where
        F: FnMut(FfiEntity, &mut ValidatedCommands<'_>) -> FfiResult + Send + 'static,
    {
        let desc = allocate_runtime_lifecycle_observer_desc(
            self.host,
            name,
            event_kind,
            watched_components,
            target,
            flags,
            propagate_relationship,
            callback,
        )?;
        // SAFETY: `desc`'s closure state, `name`, and `watched_components` were pinned in `GuestInstance`.
        unsafe { self.observe_raw(&desc) }
    }

    /// Spawns a BSN or dynamic scene instance from `scene_handle` (optionally parented under
    /// `parent` when `!parent.is_null()`) via `BamHostApi::cmd_spawn_scene`, returning the
    /// reserved scene root [`FfiEntity`].
    pub fn spawn_scene(
        &mut self,
        scene_handle: &FfiAssetHandle,
        parent: FfiEntity,
    ) -> Result<FfiEntity, FfiResult> {
        if self.raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        let handle_status = scene_handle.validate();
        if !handle_status.is_ok() {
            return Err(handle_status);
        }
        if scene_handle.is_none() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, cmd_spawn_scene),
            core::mem::size_of::<Option<BamCmdSpawnSceneFn>>(),
        ) {
            return Err(FfiResult::UNSUPPORTED);
        }
        let Some(spawn_scene_fn) = self.host.api().cmd_spawn_scene else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let mut out = FfiEntity::NULL;
        // SAFETY: `self.raw` is active and `scene_handle` / `&mut out` are aligned stack references.
        let status = unsafe { spawn_scene_fn(self.raw, scene_handle, parent, &mut out) };
        if status.is_ok() { Ok(out) } else { Err(status) }
    }
}
