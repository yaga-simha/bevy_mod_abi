//! Opaque deferred command buffer handle, trigger descriptors, and handle-based/batched command FFI function types.

use core::marker::{PhantomData, PhantomPinned};

use crate::{
    access::BamTermDesc,
    extensible::ExtensibleStruct,
    primitives::{BamComponent, FfiAssetHandle, FfiEntity, FfiResult},
    system::{BamEventKind, BamObserverDesc},
};

/// Opaque C handle representing the per-system deferred command buffer (`typedef struct BamCommands BamCommands;`).
#[repr(C)]
pub struct BamCommands {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Enable hierarchical/relationship upward propagation (`PropagateEntityTrigger`) on [`BamTriggerDesc::flags`].
pub const BAM_TRIGGER_FLAG_PROPAGATE: u32 = 1 << 0;

/// Bitmask of recognized [`BamTriggerDesc::flags`] bits in ABI `0.2`.
pub const BAM_TRIGGER_KNOWN_FLAGS: u32 = BAM_TRIGGER_FLAG_PROPAGATE;

/// Extensible deferred trigger descriptor passed to [`BamCmdTriggerExFn`]
/// (`typedef struct BamTriggerDesc BamTriggerDesc;`).
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 64` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `MIN_V0_2_SIZE = 48` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamTriggerDesc {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Bitmask of `BAM_TRIGGER_FLAG_*` (`offset 4`).
    pub flags: u32,
    /// Pre-resolved [`BamComponent`] handle of the triggered event (`offset 8`; may be `INVALID` for lifecycle kinds).
    pub event: BamComponent,
    /// Event kind (`CUSTOM` or lifecycle `ADD`..=`READY`, `offset 12`).
    pub event_kind: BamEventKind,
    /// Lower 32 bits of optional target [`FfiEntity`] (`0` when global, `offset 16`).
    pub entity_lo: u32,
    /// Upper 32 bits of optional target [`FfiEntity`] (`0` when global, `offset 20`).
    pub entity_hi: u32,
    /// Pointer to mutable or read-only event payload bytes (`offset 24`; null allowed iff `data_len == 0`).
    pub data_ptr: *mut u8,
    /// Byte length of `data_ptr` (`offset 32` on 64-bit / `28` on 32-bit).
    pub data_len: usize,
    /// Optional watched/target [`BamComponent`] handles for `EntityComponentsTrigger` (`offset 40` / `32`).
    pub watched_components: *const BamComponent,
    /// Number of entries in `watched_components` (`offset 48` / `36`).
    pub watched_component_count: usize,
    /// Relationship component handle for propagation when `BAM_TRIGGER_FLAG_PROPAGATE` is set
    /// (`BamComponent::INVALID` defaults to `ChildOf`, `offset 56` / `40`).
    pub propagate_relationship: BamComponent,
    /// Explicit zero-reserved alignment field (`must be 0`, `offset 60` / `44`).
    pub reserved0: u32,
}

impl BamTriggerDesc {
    /// Frozen minimum valid `BamTriggerDesc` size in ABI `0.2` (`64` on 64-bit, `48` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, reserved0) + core::mem::size_of::<u32>();

    /// Constructs a custom event [`BamTriggerDesc`] over `data_ptr[0..data_len]`.
    #[inline]
    pub const fn new(event: BamComponent, data_ptr: *mut u8, data_len: usize) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            event,
            event_kind: BamEventKind::CUSTOM,
            entity_lo: 0,
            entity_hi: 0,
            data_ptr,
            data_len,
            watched_components: core::ptr::null(),
            watched_component_count: 0,
            propagate_relationship: BamComponent::INVALID,
            reserved0: 0,
        }
    }

    /// Returns `true` if `self.struct_size` covers a field of `slot_size` bytes at `slot_offset`.
    #[inline]
    pub const fn covers_slot(&self, slot_offset: usize, slot_size: usize) -> bool {
        match slot_offset.checked_add(slot_size) {
            Some(end) => (self.struct_size as usize) >= end,
            None => false,
        }
    }

    /// Sets `BAM_TRIGGER_FLAG_*` bitflags on this descriptor.
    #[inline]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Sets [`BamEventKind`] on this trigger descriptor.
    #[inline]
    pub const fn with_event_kind(mut self, event_kind: BamEventKind) -> Self {
        self.event_kind = event_kind;
        self
    }

    /// Targets a specific entity (`FfiEntity::NULL` restores global triggering).
    #[inline]
    pub const fn with_entity(mut self, entity: FfiEntity) -> Self {
        let bits = entity.to_bits();
        self.entity_lo = bits as u32;
        self.entity_hi = (bits >> 32) as u32;
        self
    }

    /// Reconstructs the configured target [`FfiEntity`] (`FfiEntity::NULL` when global).
    #[inline]
    pub const fn entity(&self) -> FfiEntity {
        FfiEntity::from_bits(((self.entity_hi as u64) << 32) | (self.entity_lo as u64))
    }

    /// Attaches component handles for `EntityComponentsTrigger` lifecycle triggers.
    #[inline]
    pub const fn with_watched_components(
        mut self,
        watched_components: *const BamComponent,
        watched_component_count: usize,
    ) -> Self {
        self.watched_components = watched_components;
        self.watched_component_count = watched_component_count;
        self
    }

    /// Enables relationship propagation (`BAM_TRIGGER_FLAG_PROPAGATE`) over `relationship`
    /// (`BamComponent::INVALID` defaults to Bevy's `ChildOf` hierarchy).
    #[inline]
    pub const fn with_propagate(mut self, relationship: BamComponent) -> Self {
        self.flags |= BAM_TRIGGER_FLAG_PROPAGATE;
        self.propagate_relationship = relationship;
        self
    }

    /// Returns `true` if `BAM_TRIGGER_FLAG_PROPAGATE` is set.
    #[inline]
    pub const fn is_propagating(&self) -> bool {
        (self.flags & BAM_TRIGGER_FLAG_PROPAGATE) != 0
    }

    /// Validates `struct_size`, known `flags`, `reserved0 == 0`, `event_kind`, pointer slices,
    /// and event/component handle presence.
    #[inline]
    pub const fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_2_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if (self.flags & !BAM_TRIGGER_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if self.reserved0 != 0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        if !self.event_kind.is_valid() {
            return FfiResult::INVALID_ARGUMENT;
        }
        if self.data_len > 0 && self.data_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if self.watched_component_count > 0 && self.watched_components.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if self.event_kind.is_custom() {
            if !self.event.is_valid() {
                return FfiResult::INVALID_COMPONENT;
            }
        } else if self.event_kind.is_component_lifecycle()
            && !self.event.is_valid()
            && self.watched_component_count == 0
        {
            return FfiResult::INVALID_COMPONENT;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(BamTriggerDesc::MIN_V0_2_SIZE <= core::mem::size_of::<BamTriggerDesc>());
    #[cfg(target_pointer_width = "64")]
    {
        assert!(BamTriggerDesc::MIN_V0_2_SIZE == 64);
        assert!(core::mem::size_of::<BamTriggerDesc>() == 64);
        assert!(core::mem::align_of::<BamTriggerDesc>() == 8);
        assert!(core::mem::offset_of!(BamTriggerDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamTriggerDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamTriggerDesc, event) == 8);
        assert!(core::mem::offset_of!(BamTriggerDesc, event_kind) == 12);
        assert!(core::mem::offset_of!(BamTriggerDesc, entity_lo) == 16);
        assert!(core::mem::offset_of!(BamTriggerDesc, entity_hi) == 20);
        assert!(core::mem::offset_of!(BamTriggerDesc, data_ptr) == 24);
        assert!(core::mem::offset_of!(BamTriggerDesc, data_len) == 32);
        assert!(core::mem::offset_of!(BamTriggerDesc, watched_components) == 40);
        assert!(core::mem::offset_of!(BamTriggerDesc, watched_component_count) == 48);
        assert!(core::mem::offset_of!(BamTriggerDesc, propagate_relationship) == 56);
        assert!(core::mem::offset_of!(BamTriggerDesc, reserved0) == 60);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(BamTriggerDesc::MIN_V0_2_SIZE == 48);
        assert!(core::mem::size_of::<BamTriggerDesc>() == 48);
        assert!(core::mem::align_of::<BamTriggerDesc>() == 4);
        assert!(core::mem::offset_of!(BamTriggerDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamTriggerDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamTriggerDesc, event) == 8);
        assert!(core::mem::offset_of!(BamTriggerDesc, event_kind) == 12);
        assert!(core::mem::offset_of!(BamTriggerDesc, entity_lo) == 16);
        assert!(core::mem::offset_of!(BamTriggerDesc, entity_hi) == 20);
        assert!(core::mem::offset_of!(BamTriggerDesc, data_ptr) == 24);
        assert!(core::mem::offset_of!(BamTriggerDesc, data_len) == 28);
        assert!(core::mem::offset_of!(BamTriggerDesc, watched_components) == 32);
        assert!(core::mem::offset_of!(BamTriggerDesc, watched_component_count) == 36);
        assert!(core::mem::offset_of!(BamTriggerDesc, propagate_relationship) == 40);
        assert!(core::mem::offset_of!(BamTriggerDesc, reserved0) == 44);
    }
};

// SAFETY: `BamTriggerDesc` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for BamTriggerDesc {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_2_SIZE;
}

/// Reserves a fresh empty entity atomically and writes its [`FfiEntity`] identifier into `*out_entity`.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - `out_entity` must point to writable [`FfiEntity`] storage.
pub type BamCmdSpawnEmptyFn =
    unsafe extern "C" fn(commands: *mut BamCommands, out_entity: *mut FfiEntity) -> FfiResult;

/// Reserves a fresh entity and queues a batched multi-component spawn (`components[0..count]` with
/// byte payloads `data_ptrs[0..count]`), placing all components in a single archetype transition
/// during `apply_deferred`.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - `out_entity` must point to writable [`FfiEntity`] storage.
/// - If `count > 0`, `components` must point to `count` readable [`BamComponent`] handles, and
///   `data_ptrs` must point to `count` pointers where each `data_ptrs[i]` is readable for the
///   registered byte size of `components[i]` (when `size > 0`).
pub type BamCmdSpawnFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    components: *const BamComponent,
    data_ptrs: *const *const u8,
    count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult;

/// Reserves `entity_count` fresh entities and queues a bulk SoA spawn of `components[0..component_count]`
/// across `column_ptrs[0..component_count]` (where each `column_ptrs[c]` points to
/// `entity_count * elem_size` contiguous component bytes), optionally writing the reserved
/// [`FfiEntity`] handles into `out_entities[0..entity_count]` when `out_entities` is non-null.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - If `component_count > 0`, `components` and `column_ptrs` must each point to `component_count`
///   readable entries, and when `entity_count > 0` and `size_of(components[c]) > 0`, `column_ptrs[c]`
///   must be readable for `entity_count * size_of(components[c])` bytes.
/// - If `out_entities` is non-null and `entity_count > 0`, `out_entities` must point to at least
///   `entity_count` writable [`FfiEntity`] slots.
pub type BamCmdSpawnBatchFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    components: *const BamComponent,
    column_ptrs: *const *const u8,
    component_count: usize,
    entity_count: usize,
    out_entities: *mut FfiEntity,
) -> FfiResult;

/// Queues deferred despawning of `entity`.
///
/// # Safety
/// `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
pub type BamCmdDespawnFn =
    unsafe extern "C" fn(commands: *mut BamCommands, entity: FfiEntity) -> FfiResult;

/// Queues deferred bulk despawning (`World::despawn_all`) of all non-resource entities matching
/// the filter terms `terms[0..term_count]`.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - If `term_count > 0`, `terms` must point to `term_count` readable [`BamTermDesc`] entries.
pub type BamCmdDespawnAllFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    terms: *const BamTermDesc,
    term_count: usize,
) -> FfiResult;

/// Queues deferred raw byte insertion of pre-resolved `component` (`data_len` bytes at `data_ptr`)
/// onto `entity`.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - If `data_len > 0`, `data_ptr` must point to at least `data_len` readable bytes matching the
///   registered component layout (including any struct padding bytes).
pub type BamCmdInsertFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult;

/// Queues deferred insertion of host-constructed `T::default()` for pre-resolved `component` on `entity`.
///
/// Returns [`FfiResult::PERMISSION_DENIED`] if `component` was not registered with a `Default` constructor.
///
/// # Safety
/// `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
pub type BamCmdInsertDefaultFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
) -> FfiResult;

/// Queues deferred removal of pre-resolved `component` from `entity`.
///
/// # Safety
/// `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
pub type BamCmdRemoveFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
) -> FfiResult;

/// Queues deferred observer trigger (`World::trigger`) for pre-resolved `event` (`data_len` bytes
/// at `data_ptr`), optionally targeting `target` (`FfiEntity::NULL` for global `Event` triggers
/// or non-null target for `EntityEvent` triggers).
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - If `data_len > 0`, `data_ptr` must point to at least `data_len` readable bytes matching `event`.
pub type BamCmdTriggerFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    event: BamComponent,
    target: FfiEntity,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult;

/// Queues an extended deferred trigger described by [`BamTriggerDesc`] (supporting lifecycle
/// triggers, watched component sets, mutable payload triggers, and relationship propagation).
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - `desc` must point to a valid [`BamTriggerDesc`].
pub type BamCmdTriggerExFn =
    unsafe extern "C" fn(commands: *mut BamCommands, desc: *const BamTriggerDesc) -> FfiResult;

/// Queues deferred spawning of a runtime dynamic observer described by [`BamObserverDesc`],
/// optionally writing the reserved observer [`FfiEntity`] into `*out_entity` when non-null.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - `desc` must point to a valid [`BamObserverDesc`] whose callbacks and `user_data` outlive the observer.
/// - If `out_entity` is non-null, it must point to writable [`FfiEntity`] storage.
pub type BamCmdObserveFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    desc: *const BamObserverDesc,
    out_entity: *mut FfiEntity,
) -> FfiResult;

/// Reserves a fresh root entity and queues deferred spawning of `scene_handle` (optionally
/// parented under `parent` when `parent != FfiEntity::NULL`), writing the reserved root entity
/// into `*out_entity`.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - `scene_handle` must point to a readable [`FfiAssetHandle`].
/// - `out_entity` must point to writable [`FfiEntity`] storage.
pub type BamCmdSpawnSceneFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    scene_handle: *const FfiAssetHandle,
    parent: FfiEntity,
    out_entity: *mut FfiEntity,
) -> FfiResult;
