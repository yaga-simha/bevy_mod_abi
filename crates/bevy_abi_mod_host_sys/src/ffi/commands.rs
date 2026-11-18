//! Per-system bump-allocated deferred command queue (`HostCommands`, `CommandArena`)
//!.
//!
//! # Intent
//! During system execution, [`UnsafeWorldCell`](bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell)
//! cannot perform structural ECS mutations (spawning, despawning, or inserting/removing
//! components) without invalidating active table/archetype borrows and `HostIter` column
//! pointers. Buffering mutations into [`HostCommands`] and flushing them during Bevy's
//! `apply_deferred` phase preserves parallel scheduler guarantees, avoids cross-allocator
//! `Box`/`Vec` transfers, and eliminates steady-state heap allocations across frames.
//!
//! # How It Works
//! - Component byte payloads are copied as `MaybeUninit<u8>` (never `&[u8]`) into reusable aligned
//!   slabs inside [`CommandArena`].
//! - Commands (`SpawnEmpty`, `SpawnBatch`, `Despawn`, `Insert`, `InsertDefault`, `Remove`) are
//!   recorded in flat `Vec` buffers reused every frame.
//! - `cmd_spawn` applies all `K` components in a single archetype transition via
//!   [`bevy_ecs::world::EntityWorldMut::insert_by_ids`].

use crate::drop_trampoline::{drop_slot_generation, run_uncommitted_slot_drop};
use crate::handle_header::{COMMANDS_MAGIC, HandleHeader, validate_handle_mut};
use crate::loader::{ModId, ModScoped};
use crate::registry::{InsertDefaultFn, ModRegistry, RegistrySnapshot};
use bevy_abi_mod_sys::{BamCommands, BamComponent, FfiAssetHandle, FfiEntity, FfiResult};
use bevy_ecs::component::ComponentId;
use bevy_ecs::entity::{Entity, EntityAllocator};
use bevy_ecs::event::{EntityTrigger, GlobalTrigger};
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::ptr::{OwningPtr, PtrMut};
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::num::NonZeroUsize;
use core::sync::atomic::AtomicBool;
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::sync::Arc;

/// Maximum number of components accepted in a single [`host_cmd_spawn`] batch (`FfiResult::CAPACITY_EXCEEDED`).
pub const MAX_SPAWN_BATCH_COMPONENTS: usize = 64;

const DEFAULT_SLAB_BYTES: usize = 64 * 1024;
const SLAB_BASE_ALIGN: usize = 64;

struct ArenaSlab {
    ptr: *mut MaybeUninit<u8>,
    layout: Layout,
    cursor: usize,
}

impl ArenaSlab {
    fn new(min_bytes: usize, min_align: usize) -> Self {
        let align = min_align.max(SLAB_BASE_ALIGN).next_power_of_two();
        let cap = min_bytes.max(DEFAULT_SLAB_BYTES).next_multiple_of(align);
        let layout = Layout::from_size_align(cap, align).expect("valid CommandArena slab layout");
        // SAFETY: `cap >= DEFAULT_SLAB_BYTES > 0` and `align` is a power of two.
        let ptr = unsafe { alloc(layout) }.cast::<MaybeUninit<u8>>();
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Self {
            ptr,
            layout,
            cursor: 0,
        }
    }

    #[inline]
    fn try_alloc(&mut self, size: usize, align: usize) -> Option<*mut MaybeUninit<u8>> {
        if align > self.layout.align() {
            return None;
        }
        let aligned_offset = self.cursor.checked_add(align - 1)? & !(align - 1);
        let end = aligned_offset.checked_add(size)?;
        if end > self.layout.size() {
            return None;
        }
        self.cursor = end;
        // SAFETY: `aligned_offset + size <= self.layout.size()`, and `self.ptr` is aligned to
        // `self.layout.align() >= align`, so `self.ptr.add(aligned_offset)` is aligned to `align`.
        Some(unsafe { self.ptr.add(aligned_offset) })
    }
}

impl Drop for ArenaSlab {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was allocated with `std::alloc::alloc(self.layout)`.
        unsafe {
            dealloc(self.ptr.cast::<u8>(), self.layout);
        }
    }
}

// SAFETY: `ArenaSlab` uniquely owns a raw heap byte slab containing Pod/raw component payloads.
unsafe impl Send for ArenaSlab {}
// SAFETY: Shared references to `ArenaSlab` expose no unsynchronized interior mutability.
unsafe impl Sync for ArenaSlab {}

#[derive(Clone, Copy, Debug)]
struct ArenaSlice {
    slab_idx: u32,
    offset: usize,
    size: usize,
    align: usize,
}

/// Reusable multi-slab aligned bump arena storing `MaybeUninit<u8>` component payloads.
#[derive(Default)]
pub struct CommandArena {
    slabs: Vec<ArenaSlab>,
    active_slab: usize,
}

impl CommandArena {
    /// Creates an empty [`CommandArena`] (allocates its first slab lazily on first non-ZST write).
    #[inline]
    pub const fn new() -> Self {
        Self {
            slabs: Vec::new(),
            active_slab: 0,
        }
    }

    /// Copies `size` raw `MaybeUninit<u8>` bytes from `src` into `align`-aligned arena storage.
    ///
    /// # Safety
    /// If `size > 0`, `src` must be non-null and readable for `size` `MaybeUninit<u8>` bytes.
    unsafe fn alloc_copy_raw(
        &mut self,
        src: *const u8,
        size: usize,
        align: usize,
    ) -> Result<ArenaSlice, FfiResult> {
        let align = align.max(1);
        if !align.is_power_of_two() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        if size == 0 {
            return Ok(ArenaSlice {
                slab_idx: u32::MAX,
                offset: 0,
                size: 0,
                align,
            });
        }
        if src.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }

        while self.active_slab < self.slabs.len() {
            let slab_idx = self.active_slab;
            let base = self.slabs[slab_idx].ptr as usize;
            if let Some(dst) = self.slabs[slab_idx].try_alloc(size, align) {
                // SAFETY: Caller guarantees `src` is readable for `size` `MaybeUninit<u8>` bytes;
                // `dst` is freshly bump-allocated inside `slabs[slab_idx]` and non-overlapping.
                unsafe {
                    core::ptr::copy_nonoverlapping(src.cast::<MaybeUninit<u8>>(), dst, size);
                }
                return Ok(ArenaSlice {
                    slab_idx: slab_idx as u32,
                    offset: (dst as usize) - base,
                    size,
                    align,
                });
            }
            self.active_slab += 1;
        }

        let mut slab = ArenaSlab::new(size, align);
        let base = slab.ptr as usize;
        let dst = slab
            .try_alloc(size, align)
            .expect("freshly allocated ArenaSlab must fit requested payload");
        // SAFETY: Same as above; copying as `MaybeUninit<u8>` avoids forming `&[u8]` over padding.
        unsafe {
            core::ptr::copy_nonoverlapping(src.cast::<MaybeUninit<u8>>(), dst, size);
        }
        let slab_idx = self.slabs.len();
        self.slabs.push(slab);
        self.active_slab = slab_idx;

        Ok(ArenaSlice {
            slab_idx: slab_idx as u32,
            offset: (dst as usize) - base,
            size,
            align,
        })
    }

    #[inline]
    fn ptr_mut(&mut self, slice: ArenaSlice) -> core::ptr::NonNull<u8> {
        if slice.size == 0 {
            let nz = NonZeroUsize::new(slice.align.max(1)).expect("non-zero alignment");
            return core::ptr::NonNull::<u8>::without_provenance(nz);
        }
        let slab = &mut self.slabs[slice.slab_idx as usize];
        // SAFETY: `slice` was produced by `alloc_copy_raw` from `slabs[slice.slab_idx]` with
        // `offset + size <= slab.layout.size()` and `slab.ptr` is non-null.
        let raw = unsafe { slab.ptr.add(slice.offset).cast::<u8>() };
        // SAFETY: `slab.ptr` is non-null, so `slab.ptr.add(slice.offset)` is non-null.
        unsafe { core::ptr::NonNull::new_unchecked(raw) }
    }

    /// Resets all slabs to cursor `0` while retaining up to 2 slabs of allocated capacity for
    /// zero-allocation reuse on subsequent frames.
    #[inline]
    pub fn reset(&mut self) {
        if self.slabs.len() > 2 {
            self.slabs.truncate(2);
        }
        for slab in &mut self.slabs {
            slab.cursor = 0;
        }
        self.active_slab = 0;
    }
}

#[derive(Clone, Copy)]
enum RecordedCommand {
    SpawnEmpty {
        entity: Entity,
        owner_mod_id: Option<ModId>,
    },
    SpawnBatch {
        entity: Entity,
        batch_start: usize,
        batch_len: usize,
        owner_mod_id: Option<ModId>,
    },
    Despawn {
        entity: Entity,
    },
    Insert {
        entity: Entity,
        component: BamComponent,
        slice: ArenaSlice,
        drop_slot: Option<(usize, u64)>,
    },
    InsertDefault {
        entity: Entity,
        insert_fn: InsertDefaultFn,
    },
    Remove {
        entity: Entity,
        component_id: ComponentId,
    },
    Trigger {
        component: BamComponent,
        target: Option<Entity>,
        slice: ArenaSlice,
    },
    SpawnScene {
        entity: Entity,
        parent: Option<Entity>,
        handle: FfiAssetHandle,
    },
    Custom {
        idx: usize,
    },
}

type CustomCommandFn = Box<dyn FnOnce(&mut World, &RegistrySnapshot) + Send + Sync>;

#[derive(Clone, Copy)]
struct BatchItem {
    component: BamComponent,
    component_id: ComponentId,
    slice: ArenaSlice,
    drop_slot: Option<(usize, u64)>,
    validates_entity_payload: bool,
}

/// Persistent per-system command buffer storage reused across frames with zero steady-state heap
/// allocations.
#[derive(Default)]
pub struct HostCommandsStorage {
    arena: CommandArena,
    commands: Vec<RecordedCommand>,
    batch_items: Vec<BatchItem>,
    scratch_ids: Vec<ComponentId>,
    custom_fns: Vec<Option<CustomCommandFn>>,
}

impl HostCommandsStorage {
    /// Creates empty persistent command storage.
    #[inline]
    pub const fn new() -> Self {
        Self {
            arena: CommandArena::new(),
            commands: Vec::new(),
            batch_items: Vec::new(),
            scratch_ids: Vec::new(),
            custom_fns: Vec::new(),
        }
    }

    /// Returns `true` if no commands are currently queued.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Returns the number of queued commands awaiting application.
    #[inline]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Invokes foreign destructors on any uncommitted dynamic `drop_fn` payloads still held in
    /// `CommandArena` without decrementing `SLOT_LIFECYCLE`.
    fn drop_uncommitted_payloads(&mut self) {
        if self.commands.is_empty() && self.batch_items.is_empty() {
            return;
        }
        for i in 0..self.commands.len() {
            match self.commands[i] {
                RecordedCommand::SpawnBatch {
                    batch_start,
                    batch_len,
                    ..
                } => {
                    for item in &mut self.batch_items[batch_start..(batch_start + batch_len)] {
                        if let Some((slot, expected_gen)) = item.drop_slot.take() {
                            let nn = self.arena.ptr_mut(item.slice);
                            // SAFETY: `nn` holds an uncommitted guest payload in `self.arena` whose
                            // ownership never transferred into Bevy ECS.
                            unsafe {
                                run_uncommitted_slot_drop(slot, expected_gen, nn.as_ptr());
                            }
                        }
                    }
                }
                RecordedCommand::Insert {
                    slice,
                    ref mut drop_slot,
                    ..
                } => {
                    if let Some((slot, expected_gen)) = drop_slot.take() {
                        let nn = self.arena.ptr_mut(slice);
                        // SAFETY: `nn` holds an uncommitted guest payload in `self.arena` whose
                        // ownership never transferred into Bevy ECS.
                        unsafe {
                            run_uncommitted_slot_drop(slot, expected_gen, nn.as_ptr());
                        }
                    }
                }
                _ => {}
            }
        }
        // Catch any orphaned `batch_items` not yet linked to a `SpawnBatch` command.
        for item in &mut self.batch_items {
            if let Some((slot, expected_gen)) = item.drop_slot.take() {
                let nn = self.arena.ptr_mut(item.slice);
                // SAFETY: Same as above.
                unsafe {
                    run_uncommitted_slot_drop(slot, expected_gen, nn.as_ptr());
                }
            }
        }
    }

    /// Discards all queued commands without applying them to the `World` (used when a guest system
    /// faults mid-invocation), dropping any uncommitted foreign destructor payloads while preserving
    /// allocated slab and vector capacities.
    #[inline]
    pub fn clear(&mut self) {
        self.drop_uncommitted_payloads();
        self.commands.clear();
        self.batch_items.clear();
        self.scratch_ids.clear();
        self.custom_fns.clear();
        self.arena.reset();
    }

    /// Takes ownership of all queued commands into a fresh `HostCommandsStorage` (used only by
    /// the cold `System::queue_deferred` combinator path).
    pub fn take_for_deferred_queue(&mut self) -> Self {
        core::mem::take(self)
    }

    /// Applies all queued commands to `world` in FIFO order and resets all internal buffers for reuse.
    pub fn apply_to_world(&mut self, world: &mut World, snapshot: &RegistrySnapshot) {
        if self.commands.is_empty() {
            return;
        }
        // Prefer the latest `ModRegistry` snapshot in `world` when available so commands queued
        // before an intermediate `unload_mod` observe evicted `drop_fn == None` state.
        let live_registry = world.get_resource::<ModRegistry>().cloned();
        let active_snapshot = live_registry
            .as_ref()
            .map(ModRegistry::snapshot)
            .unwrap_or(snapshot);

        for i in 0..self.commands.len() {
            match self.commands[i] {
                RecordedCommand::SpawnEmpty {
                    entity,
                    owner_mod_id,
                } => {
                    if let Ok(mut entity_mut) = world.spawn_empty_at(entity)
                        && let Some(mod_id) = owner_mod_id
                    {
                        entity_mut.insert(ModScoped(mod_id));
                    }
                }
                RecordedCommand::SpawnBatch {
                    entity,
                    batch_start,
                    batch_len,
                    owner_mod_id,
                } => {
                    let mut include = [true; MAX_SPAWN_BATCH_COMPONENTS];
                    for (idx, item) in self.batch_items[batch_start..(batch_start + batch_len)]
                        .iter_mut()
                        .enumerate()
                    {
                        let stale_unloaded = match active_snapshot.get_by_handle(item.component) {
                            None => true,
                            Some(meta) => {
                                meta.is_dynamic
                                    && meta.drop_slot.is_some()
                                    && meta.drop_fn.is_none()
                            }
                        };
                        if stale_unloaded {
                            include[idx] = false;
                            if let Some((slot, expected_gen)) = item.drop_slot.take() {
                                let nn = self.arena.ptr_mut(item.slice);
                                // SAFETY: Component was unloaded before apply; `expected_gen` prevents
                                // invoking a recycled slot's new destructor.
                                unsafe {
                                    run_uncommitted_slot_drop(slot, expected_gen, nn.as_ptr());
                                }
                            }
                            continue;
                        }
                        if item.validates_entity_payload {
                            let nn = self.arena.ptr_mut(item.slice);
                            // SAFETY: `validates_entity_payload` entries hold an 8-byte `FfiEntity`.
                            let raw_bits =
                                unsafe { core::ptr::read_unaligned(nn.as_ptr().cast::<u64>()) };
                            let valid_parent = Entity::try_from_bits(raw_bits)
                                .is_some_and(|p| p != entity && world.entities().contains(p));
                            if !valid_parent {
                                include[idx] = false;
                                tracing::warn!(
                                    child = ?entity,
                                    parent_bits = raw_bits,
                                    "skipping ChildOf during spawn because parent entity is invalid or equals child"
                                );
                            }
                        }
                    }
                    if let Ok(mut entity_mut) = world.spawn_empty_at(entity) {
                        if let Some(mod_id) = owner_mod_id {
                            entity_mut.insert(ModScoped(mod_id));
                        }
                        self.scratch_ids.clear();
                        let batch_slice =
                            &mut self.batch_items[batch_start..(batch_start + batch_len)];
                        for (idx, item) in batch_slice.iter_mut().enumerate() {
                            // Ownership of each payload transfers into Bevy ECS below (or is skipped).
                            item.drop_slot = None;
                            if include[idx] {
                                self.scratch_ids.push(item.component_id);
                            }
                        }
                        if !self.scratch_ids.is_empty() {
                            let arena = &mut self.arena;
                            let ptrs_iter = batch_slice
                                .iter()
                                .enumerate()
                                .filter(|(idx, _)| include[*idx])
                                .map(|(_, item)| {
                                    let nn = arena.ptr_mut(item.slice);
                                    // SAFETY: Each `nn` points to disjoint aligned arena storage matching
                                    // the registered `(size, align)` of `scratch_ids[k]`, transferring ownership into Bevy ECS.
                                    unsafe { OwningPtr::new(nn) }
                                });
                            // SAFETY: `scratch_ids` has no duplicate `ComponentId`s (verified during
                            // `host_cmd_spawn`), matches `ptrs_iter` length, and each `ComponentId`
                            // was registered in `world` with the exact payload layout.
                            unsafe {
                                entity_mut.insert_by_ids(&self.scratch_ids, ptrs_iter);
                            }
                        }
                    } else {
                        for item in &mut self.batch_items[batch_start..(batch_start + batch_len)] {
                            if let Some((slot, expected_gen)) = item.drop_slot.take() {
                                let nn = self.arena.ptr_mut(item.slice);
                                // SAFETY: `spawn_empty_at` failed, so ownership remained in `self.arena`.
                                unsafe {
                                    run_uncommitted_slot_drop(slot, expected_gen, nn.as_ptr());
                                }
                            }
                        }
                    }
                }
                RecordedCommand::Despawn { entity } => {
                    if let Ok(e) = world.get_entity_mut(entity) {
                        e.despawn();
                    }
                }
                RecordedCommand::Insert {
                    entity,
                    component,
                    slice,
                    drop_slot,
                } => {
                    if let RecordedCommand::Insert {
                        drop_slot: ref mut stored_slot,
                        ..
                    } = self.commands[i]
                    {
                        *stored_slot = None;
                    }
                    let Some(meta) = active_snapshot.get_by_handle(component) else {
                        if let Some((slot, expected_gen)) = drop_slot {
                            let nn = self.arena.ptr_mut(slice);
                            // SAFETY: Component handle vanished before apply; drop uncommitted payload.
                            unsafe {
                                run_uncommitted_slot_drop(slot, expected_gen, nn.as_ptr());
                            }
                        }
                        continue;
                    };
                    if meta.is_dynamic && meta.drop_slot.is_some() && meta.drop_fn.is_none() {
                        if let Some((slot, expected_gen)) = drop_slot {
                            let nn = self.arena.ptr_mut(slice);
                            // SAFETY: Owning mod unloaded before apply; `expected_gen` guards against
                            // invoking a recycled slot's new destructor.
                            unsafe {
                                run_uncommitted_slot_drop(slot, expected_gen, nn.as_ptr());
                            }
                        }
                        continue;
                    }
                    let nn = self.arena.ptr_mut(slice);
                    if meta.validates_entity_payload {
                        // SAFETY: `validates_entity_payload` components hold an 8-byte `FfiEntity`.
                        let raw_bits =
                            unsafe { core::ptr::read_unaligned(nn.as_ptr().cast::<u64>()) };
                        let valid_parent = Entity::try_from_bits(raw_bits)
                            .is_some_and(|p| p != entity && world.entities().contains(p));
                        if !valid_parent {
                            tracing::warn!(
                                child = ?entity,
                                parent_bits = raw_bits,
                                "skipping ChildOf insert because parent entity is invalid or equals child"
                            );
                            continue;
                        }
                    }
                    if !meta.is_dynamic {
                        if let Some(insert_fn) = meta.insert_raw_fn {
                            // SAFETY: `nn` points to `meta.size` bytes aligned to `meta.align`
                            // inside `self.arena`; `insert_fn` reads `T` via `ptr::read_unaligned`.
                            unsafe {
                                insert_fn(world, entity, nn.as_ptr().cast_const());
                            }
                        }
                    } else if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
                        // SAFETY: `nn` holds `meta.size` aligned bytes for dynamic `meta.component_id`;
                        // ownership transfers into Bevy ECS storage (firing any `on_insert` hook).
                        let owning = unsafe { OwningPtr::new(nn) };
                        // SAFETY: `meta.component_id` was registered in `world` with `(meta.size, meta.align)`.
                        unsafe {
                            entity_mut.insert_by_id(meta.component_id, owning);
                        }
                    } else if let Some((slot, expected_gen)) = drop_slot {
                        // SAFETY: Target `entity` did not exist (`get_entity_mut` failed), so
                        // ownership remained in `self.arena`; run foreign destructor without
                        // decrementing `SLOT_LIFECYCLE`.
                        unsafe {
                            run_uncommitted_slot_drop(slot, expected_gen, nn.as_ptr());
                        }
                    }
                }
                RecordedCommand::InsertDefault { entity, insert_fn } => {
                    insert_fn(world, entity);
                }
                RecordedCommand::Remove {
                    entity,
                    component_id,
                } => {
                    if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
                        entity_mut.remove_by_id(component_id);
                    }
                }
                RecordedCommand::Trigger {
                    component,
                    target,
                    slice,
                } => {
                    let Some(meta) = snapshot.get_by_handle(component) else {
                        continue;
                    };
                    if let Some(t) = target
                        && !world.entities().contains(t)
                    {
                        continue;
                    }
                    let nn = self.arena.ptr_mut(slice);
                    if let Some(trigger_fn) = meta.trigger_event_fn {
                        // SAFETY: `nn` holds `meta.size` bytes for `meta`'s registered event type.
                        unsafe {
                            let _ = trigger_fn(world, target, nn.as_ptr().cast_const());
                        }
                    } else if let Some(event_key) = meta.event_key {
                        // SAFETY: `nn` is aligned to `meta.align` and valid for `meta.size` bytes
                        // inside `self.arena` for the duration of this synchronous `trigger_dynamic*`.
                        let event_mut = unsafe { PtrMut::new(nn) };
                        if let Some(t) = target {
                            let mut trigger = EntityTrigger;
                            let trigger_mut = PtrMut::from(&mut trigger);
                            // SAFETY: `event_key` was registered from `ComponentId` with `(meta.size, meta.align)`
                            // and `trigger_mut` points to ZST `EntityTrigger`.
                            unsafe {
                                world.trigger_dynamic_targets(event_key, t, event_mut, trigger_mut);
                            }
                        } else {
                            let mut trigger = GlobalTrigger;
                            let trigger_mut = PtrMut::from(&mut trigger);
                            // SAFETY: `event_key` was registered from `ComponentId` with `(meta.size, meta.align)`
                            // and `trigger_mut` points to ZST `GlobalTrigger`.
                            unsafe {
                                world.trigger_dynamic(event_key, event_mut, trigger_mut);
                            }
                        }
                    }
                }
                RecordedCommand::SpawnScene {
                    entity,
                    parent,
                    handle,
                } => {
                    let valid_parent =
                        parent.filter(|&p| p != entity && world.entities().contains(p));
                    if let Ok(mut entity_mut) = world.spawn_empty_at(entity) {
                        if let Some(p) = valid_parent {
                            entity_mut.insert(ChildOf(p));
                        }
                        if let Some(backend) = snapshot.scene_backend() {
                            let _ =
                                backend.spawn_scene(world, snapshot, entity, valid_parent, &handle);
                        }
                    }
                }
                RecordedCommand::Custom { idx } => {
                    if let Some(custom_fn) = self.custom_fns.get_mut(idx).and_then(Option::take) {
                        custom_fn(world, snapshot);
                    }
                }
            }
        }

        self.clear();
    }
}

impl Drop for HostCommandsStorage {
    fn drop(&mut self) {
        self.drop_uncommitted_payloads();
    }
}

/// Concrete `#[repr(C)]` command context backing opaque [`BamCommands`].
#[repr(C)]
pub struct HostCommands<'a> {
    /// Leading handle validation header (`COMMANDS_MAGIC`) at byte offset `0`.
    pub header: HandleHeader,
    storage: &'a mut HostCommandsStorage,
    allocator: &'a EntityAllocator,
    snapshot: &'a RegistrySnapshot,
    mod_id: Option<ModId>,
    is_active: Option<Arc<AtomicBool>>,
    registry: Option<&'a ModRegistry>,
}

const _: () = assert!(core::mem::offset_of!(HostCommands<'_>, header) == 0);

impl<'a> HostCommands<'a> {
    /// Creates a stack-scoped [`HostCommands`] handle over `storage`, `allocator`, and `snapshot`.
    #[inline]
    pub fn new(
        storage: &'a mut HostCommandsStorage,
        allocator: &'a EntityAllocator,
        snapshot: &'a RegistrySnapshot,
    ) -> Self {
        Self {
            header: HandleHeader::new(COMMANDS_MAGIC),
            storage,
            allocator,
            snapshot,
            mod_id: None,
            is_active: None,
            registry: None,
        }
    }

    /// Creates a stack-scoped [`HostCommands`] handle using a caller-supplied non-zero `generation`.
    #[inline]
    pub const fn with_generation(
        generation: u32,
        storage: &'a mut HostCommandsStorage,
        allocator: &'a EntityAllocator,
        snapshot: &'a RegistrySnapshot,
    ) -> Self {
        Self {
            header: HandleHeader::with_generation(COMMANDS_MAGIC, generation),
            storage,
            allocator,
            snapshot,
            mod_id: None,
            is_active: None,
            registry: None,
        }
    }

    /// Attaches the owning [`ModId`] and shared module liveness flag (`is_active`) to this
    /// command context.
    #[inline]
    pub fn with_mod_context(
        mut self,
        mod_id: Option<ModId>,
        is_active: Option<Arc<AtomicBool>>,
    ) -> Self {
        self.mod_id = mod_id;
        self.is_active = is_active;
        self
    }

    /// Attaches the live [`ModRegistry`] handle to this command context (used by runtime observer
    /// spawning).
    #[inline]
    pub const fn with_registry(mut self, registry: &'a ModRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Returns the owning [`ModId`] associated with this command buffer, if scoped to a loaded mod.
    #[inline]
    pub const fn owner_mod_id(&self) -> Option<ModId> {
        self.mod_id
    }

    /// Returns the shared module liveness flag (`is_active`) associated with this command buffer, if any.
    #[inline]
    pub const fn is_active(&self) -> Option<&Arc<AtomicBool>> {
        self.is_active.as_ref()
    }

    /// Returns the attached [`ModRegistry`] handle, if any.
    #[inline]
    pub const fn registry(&self) -> Option<&'a ModRegistry> {
        self.registry
    }

    /// Returns the active [`RegistrySnapshot`] bound to this command buffer.
    #[inline]
    pub const fn snapshot(&self) -> &'a RegistrySnapshot {
        self.snapshot
    }

    /// Reserves a fresh unique [`Entity`] via Bevy's lock-free [`EntityAllocator`].
    #[inline]
    pub fn reserve_entity(&self) -> Entity {
        self.allocator.alloc()
    }

    /// Enqueues a custom deferred closure executed in FIFO order alongside standard commands during
    /// [`HostCommandsStorage::apply_to_world`] (and dropped without running on [`HostCommandsStorage::clear`]).
    #[inline]
    pub fn queue_custom(
        &mut self,
        f: impl FnOnce(&mut World, &RegistrySnapshot) + Send + Sync + 'static,
    ) {
        let idx = self.storage.custom_fns.len();
        self.storage.custom_fns.push(Some(Box::new(f)));
        self.storage.commands.push(RecordedCommand::Custom { idx });
    }

    /// Casts `&mut self` to an opaque `*mut BamCommands` FFI pointer.
    #[inline]
    pub fn as_ffi_ptr(&mut self) -> *mut BamCommands {
        (self as *mut Self).cast::<BamCommands>()
    }

    pub(crate) fn queue_spawn_scene(
        &mut self,
        handle: FfiAssetHandle,
        parent: Option<Entity>,
    ) -> Result<Entity, FfiResult> {
        let Some(backend) = self.snapshot.scene_backend() else {
            return Err(FfiResult::UNSUPPORTED);
        };
        if let Err((code, _msg)) = backend.validate_scene_handle(self.snapshot, &handle) {
            return Err(code);
        }
        let entity = self.allocator.alloc();
        self.storage.commands.push(RecordedCommand::SpawnScene {
            entity,
            parent,
            handle,
        });
        Ok(entity)
    }
}

impl Drop for HostCommands<'_> {
    #[inline]
    fn drop(&mut self) {
        self.header.poison();
    }
}

#[inline]
fn validate_command_entity(entity: FfiEntity) -> Result<Entity, FfiResult> {
    if entity.is_null() {
        return Err(FfiResult::INVALID_ENTITY);
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return Err(FfiResult::INVALID_ENTITY);
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return Err(FfiResult::INVALID_ENTITY);
    }
    Ok(bevy_entity)
}

/// Reserves a fresh empty entity atomically and writes its [`FfiEntity`] identifier into `*out_entity`.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `out_entity` must point to writable [`FfiEntity`] storage, or null.
pub unsafe extern "C" fn host_cmd_spawn_empty(
    commands: *mut BamCommands,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    let entity = cmds.allocator.alloc();
    cmds.storage.commands.push(RecordedCommand::SpawnEmpty {
        entity,
        owner_mod_id: cmds.mod_id,
    });
    // SAFETY: `out_entity` was checked non-null above and caller guarantees writable `FfiEntity`.
    unsafe {
        *out_entity = FfiEntity::from_bits(entity.to_bits());
    }
    FfiResult::OK
}

/// Shared batched spawn helper supporting both mod-scoped (`persistent == false`) and
/// persistent (`persistent == true`, skipping `ModScoped`) entity creation.
///
/// # Safety
/// - `out_entity` must point to writable [`FfiEntity`] storage (non-null).
/// - If `count > 0`, `components` must point to `count` readable [`BamComponent`] handles and
///   `data_ptrs` must point to `count` pointers readable for each component's byte size.
pub(crate) unsafe fn spawn_batch_with_scope(
    cmds: &mut HostCommands<'_>,
    components: *const BamComponent,
    data_ptrs: *const *const u8,
    count: usize,
    out_entity: *mut FfiEntity,
    persistent: bool,
) -> FfiResult {
    let owner_mod_id = if persistent { None } else { cmds.mod_id };

    if count > MAX_SPAWN_BATCH_COMPONENTS {
        return FfiResult::CAPACITY_EXCEEDED;
    }
    if count == 0 {
        let entity = cmds.allocator.alloc();
        cmds.storage.commands.push(RecordedCommand::SpawnEmpty {
            entity,
            owner_mod_id,
        });
        // SAFETY: `out_entity` is non-null and writable.
        unsafe {
            *out_entity = FfiEntity::from_bits(entity.to_bits());
        }
        return FfiResult::OK;
    }
    if components.is_null() || data_ptrs.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // Pass 1: Validate every component handle, raw-insert permission, non-null payload, and
    // absence of duplicate `ComponentId`s BEFORE allocating an entity or mutating batch buffers.
    #[derive(Clone, Copy)]
    struct ValidatedBatchEntry {
        handle: BamComponent,
        component_id: ComponentId,
        size: usize,
        align: usize,
        ptr: *const u8,
        drop_slot: Option<(usize, u64)>,
        validates_entity_payload: bool,
    }

    let mut validated: [ValidatedBatchEntry; MAX_SPAWN_BATCH_COMPONENTS] = [ValidatedBatchEntry {
        handle: BamComponent::INVALID,
        component_id: ComponentId::new(0),
        size: 0,
        align: 1,
        ptr: core::ptr::null(),
        drop_slot: None,
        validates_entity_payload: false,
    };
        MAX_SPAWN_BATCH_COMPONENTS];

    for (i, slot) in validated.iter_mut().take(count).enumerate() {
        // SAFETY: `components` and `data_ptrs` are non-null and caller guarantees `count` elements.
        let handle = unsafe { *components.add(i) };
        // SAFETY: Same bounds guarantee for `data_ptrs[0..count]`.
        let ptr = unsafe { *data_ptrs.add(i) };

        let Some(meta) = cmds.snapshot.get_by_handle(handle) else {
            return FfiResult::INVALID_COMPONENT;
        };
        if meta.is_resource() {
            return FfiResult::INVALID_COMPONENT;
        };
        if meta.is_dynamic && meta.drop_slot.is_some() && meta.drop_fn.is_none() {
            return FfiResult::INVALID_STATE;
        }
        if !meta.is_dynamic && meta.insert_raw_fn.is_none() {
            return FfiResult::PERMISSION_DENIED;
        }
        if meta.size > 0 && ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if meta.validates_entity_payload {
            if meta.size != core::mem::size_of::<FfiEntity>() {
                return FfiResult::INVALID_ARGUMENT;
            }
            // SAFETY: `ptr` is non-null and points to 8 bytes (`FfiEntity`) above.
            let raw_bits = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) };
            let target = FfiEntity::from_bits(raw_bits);
            if validate_command_entity(target).is_err() {
                return FfiResult::INVALID_ENTITY;
            }
        }
        let drop_slot = if meta.is_dynamic {
            meta.drop_slot
                .and_then(|s| drop_slot_generation(s).map(|slot_gen| (s, slot_gen)))
        } else {
            None
        };
        *slot = ValidatedBatchEntry {
            handle,
            component_id: meta.component_id,
            size: meta.size,
            align: meta.align,
            ptr,
            drop_slot,
            validates_entity_payload: meta.validates_entity_payload,
        };
    }
    for i in 0..count {
        for prev in &validated[..i] {
            if prev.component_id == validated[i].component_id {
                return FfiResult::INVALID_ARGUMENT;
            }
        }
    }

    // Pass 2: Copy payloads into `CommandArena` and record single `SpawnBatch`.
    let batch_start = cmds.storage.batch_items.len();
    for entry in &validated[..count] {
        // SAFETY: `entry.ptr` was verified non-null when `entry.size > 0`, and caller guarantees `entry.size` readable bytes.
        let slice = match unsafe {
            cmds.storage
                .arena
                .alloc_copy_raw(entry.ptr, entry.size, entry.align)
        } {
            Ok(s) => s,
            Err(err) => {
                for item in &mut cmds.storage.batch_items[batch_start..] {
                    if let Some((slot, expected_gen)) = item.drop_slot.take() {
                        let nn = cmds.storage.arena.ptr_mut(item.slice);
                        // SAFETY: Partial batch failed before commit; drop already-copied payloads.
                        unsafe {
                            run_uncommitted_slot_drop(slot, expected_gen, nn.as_ptr());
                        }
                    }
                }
                cmds.storage.batch_items.truncate(batch_start);
                return err;
            }
        };
        cmds.storage.batch_items.push(BatchItem {
            component: entry.handle,
            component_id: entry.component_id,
            slice,
            drop_slot: entry.drop_slot,
            validates_entity_payload: entry.validates_entity_payload,
        });
    }

    let entity = cmds.allocator.alloc();
    cmds.storage.commands.push(RecordedCommand::SpawnBatch {
        entity,
        batch_start,
        batch_len: count,
        owner_mod_id,
    });
    // SAFETY: `out_entity` is non-null and writable.
    unsafe {
        *out_entity = FfiEntity::from_bits(entity.to_bits());
    }
    FfiResult::OK
}

/// Reserves a fresh entity and queues a batched `count`-component spawn.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `out_entity` must point to writable [`FfiEntity`] storage, or null.
/// - If `count > 0`, `components` must point to `count` readable [`BamComponent`] handles and
///   `data_ptrs` must point to `count` pointers readable for each component's byte size.
pub unsafe extern "C" fn host_cmd_spawn(
    commands: *mut BamCommands,
    components: *const BamComponent,
    data_ptrs: *const *const u8,
    count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: `cmds` is validated, `out_entity` is non-null and writable, and caller guarantees `components`/`data_ptrs`.
    unsafe { spawn_batch_with_scope(cmds, components, data_ptrs, count, out_entity, false) }
}

/// Queues deferred despawning of `entity`.
///
/// # Safety
/// `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
pub unsafe extern "C" fn host_cmd_despawn(
    commands: *mut BamCommands,
    entity: FfiEntity,
) -> FfiResult {
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    let bevy_entity = match validate_command_entity(entity) {
        Ok(e) => e,
        Err(err) => return err,
    };
    cmds.storage.commands.push(RecordedCommand::Despawn {
        entity: bevy_entity,
    });
    FfiResult::OK
}

/// Queues deferred raw byte insertion of pre-resolved `component` onto `entity`.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - If `data_len > 0`, `data_ptr` must point to at least `data_len` readable `MaybeUninit<u8>` bytes.
pub unsafe extern "C" fn host_cmd_insert(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if data_len > 0 && data_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    let Some(meta) = cmds.snapshot.get_by_handle(component) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if meta.is_resource() {
        return FfiResult::INVALID_COMPONENT;
    }
    if meta.is_dynamic && meta.drop_slot.is_some() && meta.drop_fn.is_none() {
        return FfiResult::INVALID_STATE;
    }
    if !meta.is_dynamic && meta.insert_raw_fn.is_none() {
        return FfiResult::PERMISSION_DENIED;
    }
    if meta.size != data_len {
        return FfiResult::INVALID_ARGUMENT;
    }
    let bevy_entity = match validate_command_entity(entity) {
        Ok(e) => e,
        Err(err) => return err,
    };
    if meta.validates_entity_payload {
        if data_len != core::mem::size_of::<FfiEntity>() {
            return FfiResult::INVALID_ARGUMENT;
        }
        // SAFETY: `data_ptr` is non-null (`data_len == 8`) and readable for 8 bytes.
        let raw_bits = unsafe { core::ptr::read_unaligned(data_ptr.cast::<u64>()) };
        let parent_ffi = FfiEntity::from_bits(raw_bits);
        let Ok(parent) = validate_command_entity(parent_ffi) else {
            return FfiResult::INVALID_ENTITY;
        };
        if parent == bevy_entity {
            return FfiResult::INVALID_ARGUMENT;
        }
    }

    // SAFETY: `data_ptr` is non-null when `data_len > 0` and readable for `meta.size` bytes.
    let slice = match unsafe {
        cmds.storage
            .arena
            .alloc_copy_raw(data_ptr, meta.size, meta.align)
    } {
        Ok(s) => s,
        Err(err) => return err,
    };

    let drop_slot = if meta.is_dynamic {
        meta.drop_slot
            .and_then(|slot| drop_slot_generation(slot).map(|slot_gen| (slot, slot_gen)))
    } else {
        None
    };
    cmds.storage.commands.push(RecordedCommand::Insert {
        entity: bevy_entity,
        component,
        slice,
        drop_slot,
    });
    FfiResult::OK
}

/// Queues deferred insertion of host-constructed `T::default()` for `component` on `entity`.
///
/// # Safety
/// `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
pub unsafe extern "C" fn host_cmd_insert_default(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
) -> FfiResult {
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    let Some(meta) = cmds.snapshot.get_by_handle(component) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if meta.is_resource() {
        return FfiResult::INVALID_COMPONENT;
    }
    let Some(insert_fn) = meta.insert_default_fn else {
        return FfiResult::PERMISSION_DENIED;
    };
    let bevy_entity = match validate_command_entity(entity) {
        Ok(e) => e,
        Err(err) => return err,
    };

    cmds.storage.commands.push(RecordedCommand::InsertDefault {
        entity: bevy_entity,
        insert_fn,
    });
    FfiResult::OK
}

/// Queues deferred removal of pre-resolved `component` from `entity`.
///
/// # Safety
/// `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
pub unsafe extern "C" fn host_cmd_remove(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
) -> FfiResult {
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    let Some(meta) = cmds.snapshot.get_by_handle(component) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if meta.is_resource() {
        return FfiResult::INVALID_COMPONENT;
    }
    let bevy_entity = match validate_command_entity(entity) {
        Ok(e) => e,
        Err(err) => return err,
    };

    cmds.storage.commands.push(RecordedCommand::Remove {
        entity: bevy_entity,
        component_id: meta.component_id,
    });
    FfiResult::OK
}

/// Queues deferred `world.trigger(...)` or `world.trigger_dynamic*` for pre-resolved `event`.
///
/// Pass `FfiEntity::NONE` as `target` for global events, or a valid non-null entity handle for
/// `EntityEvent` triggers.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - When `data_len > 0`, `data_ptr` must point to at least `data_len` initialized readable bytes.
pub unsafe extern "C" fn host_cmd_trigger(
    commands: *mut BamCommands,
    event: BamComponent,
    target: FfiEntity,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if data_len > 0 && data_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    let Some(meta) = cmds.snapshot.get_by_handle(event) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if meta.is_resource()
        || meta.is_message()
        || meta.event_key.is_none()
        || meta.drop_fn.is_some()
        || meta.drop_slot.is_some()
    {
        return FfiResult::INVALID_COMPONENT;
    }
    if meta.size != data_len {
        return FfiResult::INVALID_ARGUMENT;
    }

    let mut target_entity = if target.is_null() {
        None
    } else {
        Some(match validate_command_entity(target) {
            Ok(e) => e,
            Err(err) => return err,
        })
    };

    if meta.is_entity_event() && target_entity.is_none() {
        if let Some(extract_fn) = meta.extract_event_target_fn {
            // SAFETY: `data_ptr` has `meta.size` bytes matching the `EntityEvent` layout.
            let Some(extracted) = (unsafe { extract_fn(data_ptr) }) else {
                return FfiResult::INVALID_ENTITY;
            };
            if extracted == Entity::PLACEHOLDER {
                return FfiResult::INVALID_ENTITY;
            }
            target_entity = Some(extracted);
        } else {
            return FfiResult::INVALID_ENTITY;
        }
    }

    // SAFETY: `data_ptr` is non-null when `meta.size > 0` and readable for `meta.size` bytes.
    let slice = match unsafe {
        cmds.storage
            .arena
            .alloc_copy_raw(data_ptr, meta.size, meta.align)
    } {
        Ok(s) => s,
        Err(err) => return err,
    };

    cmds.storage.commands.push(RecordedCommand::Trigger {
        component: event,
        target: target_entity,
        slice,
    });
    FfiResult::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;
    use std::sync::Mutex;

    #[test]
    fn queue_custom_runs_in_fifo_order_and_clear_skips_execution() {
        let mut world = World::new();
        let registry = ModRegistry::new();
        let snap = registry.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let log = Arc::new(Mutex::new(Vec::<u32>::new()));
        let active = Arc::new(AtomicBool::new(true));
        let mid = ModId(7);

        let reserved = {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
                .with_mod_context(Some(mid), Some(Arc::clone(&active)))
                .with_registry(&registry);
            assert_eq!(cmds.owner_mod_id(), Some(mid));
            assert!(cmds.is_active().unwrap().load(Ordering::Relaxed));
            assert!(cmds.registry().is_some());
            let _ = cmds.snapshot();

            let e = cmds.reserve_entity();
            let log1 = Arc::clone(&log);
            cmds.queue_custom(move |w, _| {
                let _ = w.spawn_empty_at(e);
                log1.lock().unwrap().push(1);
            });
            let log2 = Arc::clone(&log);
            cmds.queue_custom(move |w, _| {
                assert!(w.entities().contains(e));
                log2.lock().unwrap().push(2);
            });
            e
        };

        storage.apply_to_world(&mut world, &snap);
        assert!(world.entities().contains(reserved));
        assert_eq!(*log.lock().unwrap(), vec![1, 2]);

        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let log3 = Arc::clone(&log);
            cmds.queue_custom(move |_, _| {
                log3.lock().unwrap().push(99);
            });
        }
        storage.clear();
        storage.apply_to_world(&mut world, &snap);
        assert_eq!(*log.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn stale_unloaded_drop_handle_rejected_and_queued_commands_skip_recycled_drop_slot() {
        use crate::drop_trampoline::{
            WorldDropGuard, available_drop_trampoline_slots, prune_drop_trampolines_for_guard,
        };
        use crate::ffi::commands_ext::host_cmd_spawn_persistent;
        use crate::ffi::register::{LoadContext, host_register_component};
        use bevy_abi_mod_sys::{ComponentDesc, ComponentUuid};
        use core::sync::atomic::AtomicUsize;

        static DROP_A_COUNT: AtomicUsize = AtomicUsize::new(0);
        static DROP_B_COUNT: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn drop_a(_ptr: *mut c_void) {
            DROP_A_COUNT.fetch_add(1, Ordering::SeqCst);
        }
        unsafe extern "C" fn drop_b(ptr: *mut c_void) {
            // SAFETY: Validating Mod B's 24-byte magic sentinel to catch any cross-mod type confusion.
            let sentinel = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) };
            assert_eq!(sentinel, 0xBBBB_BBBB_BBBB_BBBB);
            DROP_B_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        DROP_A_COUNT.store(0, Ordering::SeqCst);
        DROP_B_COUNT.store(0, Ordering::SeqCst);

        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModRegistry::new());
        let mod_a = ModId(101);
        let mod_b = ModId(102);

        // 1. Mod A registers `CompA` (`size = 8`, `drop_a`).
        let active_a = Arc::new(AtomicBool::new(true));
        let mut comp_a_h = BamComponent::INVALID;
        {
            let reg = world.resource::<ModRegistry>().clone();
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active_a),
                &mut world as *mut World,
                Some(mod_a),
            );
            let desc_a = ComponentDesc::new(
                c"stale_drop::CompA".as_ptr(),
                ComponentUuid::from_parts("stale_drop::CompA", 8, 8, 1),
                8,
                8,
            )
            .with_drop(drop_a);
            // SAFETY: Valid active `ctx`, `desc_a`, and writable `comp_a_h`.
            let st = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_a, &mut comp_a_h) };
            assert_eq!(st, FfiResult::OK);
            let updated = ctx.registry.clone();
            ctx.finish_init();
            world.insert_resource(updated);
        }
        let comp_a_id = world
            .resource::<ModRegistry>()
            .get_by_handle(comp_a_h)
            .unwrap()
            .component_id;
        let slot_a = world
            .resource::<ModRegistry>()
            .get_by_handle(comp_a_h)
            .unwrap()
            .drop_slot
            .unwrap();

        // 2. Queue `cmd_spawn` and `cmd_insert` of `CompA` BEFORE Mod A unloads.
        let target_ent = world.spawn_empty().id();
        let target_ffi = FfiEntity::from_bits(target_ent.to_bits());
        let pre_unload_snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut pre_unload_storage = HostCommandsStorage::new();
        let mut pre_unload_clear_storage = HostCommandsStorage::new();
        let payload_a: u64 = 0xAAAA_AAAA_AAAA_AAAA;
        let mut queued_spawn_ent = FfiEntity::NULL;
        {
            let mut cmds = HostCommands::new(
                &mut pre_unload_storage,
                world.entity_allocator(),
                &pre_unload_snap,
            )
            .with_mod_context(Some(mod_a), Some(Arc::clone(&active_a)));
            let comps = [comp_a_h];
            let ptrs = [(&raw const payload_a).cast::<u8>()];
            // SAFETY: Valid `cmds`, `comps`, `ptrs`, and `payload_a`.
            unsafe {
                assert_eq!(
                    host_cmd_spawn(
                        cmds.as_ffi_ptr(),
                        comps.as_ptr(),
                        ptrs.as_ptr(),
                        1,
                        &mut queued_spawn_ent,
                    ),
                    FfiResult::OK
                );
                assert_eq!(
                    host_cmd_insert(
                        cmds.as_ffi_ptr(),
                        target_ffi,
                        comp_a_h,
                        (&raw const payload_a).cast::<u8>(),
                        8,
                    ),
                    FfiResult::OK
                );
            }
        }
        {
            let mut cmds = HostCommands::new(
                &mut pre_unload_clear_storage,
                world.entity_allocator(),
                &pre_unload_snap,
            )
            .with_mod_context(Some(mod_a), Some(Arc::clone(&active_a)));
            // SAFETY: Valid `cmds` and `payload_a`.
            unsafe {
                assert_eq!(
                    host_cmd_insert(
                        cmds.as_ffi_ptr(),
                        target_ffi,
                        comp_a_h,
                        (&raw const payload_a).cast::<u8>(),
                        8,
                    ),
                    FfiResult::OK
                );
            }
        }

        // 3. Unload Mod A (`live_values == 0`): disarms `active_a`, recycles `slot_a` (`gen + 1`),
        //    and evicts Mod A from `ModRegistry` (`drop_fn = None`, `owner = None`).
        prune_drop_trampolines_for_guard(&active_a);
        let recycled = world
            .resource_mut::<WorldDropGuard>()
            .recycle_slots_for_guard(&active_a);
        assert_eq!(recycled, 1);
        world.resource_mut::<ModRegistry>().evict_mod(mod_a);

        // 4. Mod B loads and registers `CompB` (`size = 24`, `drop_b`) — reuses `slot_a`!
        let active_b = Arc::new(AtomicBool::new(true));
        let mut comp_b_h = BamComponent::INVALID;
        {
            let reg = world.resource::<ModRegistry>().clone();
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active_b),
                &mut world as *mut World,
                Some(mod_b),
            );
            let desc_b = ComponentDesc::new(
                c"stale_drop::CompB".as_ptr(),
                ComponentUuid::from_parts("stale_drop::CompB", 24, 8, 2),
                24,
                8,
            )
            .with_drop(drop_b);
            // SAFETY: Valid active `ctx`, `desc_b`, and writable `comp_b_h`.
            let st = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_b, &mut comp_b_h) };
            assert_eq!(st, FfiResult::OK);
            let updated = ctx.registry.clone();
            ctx.finish_init();
            world.insert_resource(updated);
        }
        let slot_b = world
            .resource::<ModRegistry>()
            .get_by_handle(comp_b_h)
            .unwrap()
            .drop_slot
            .unwrap();
        assert_eq!(slot_b, slot_a);

        // 5. Post-unload calls with stale `comp_a_h` MUST fail with `INVALID_STATE`.
        let post_unload_snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut post_storage = HostCommandsStorage::new();
        let mut ignored_ent = FfiEntity::NULL;
        {
            let mut cmds = HostCommands::new(
                &mut post_storage,
                world.entity_allocator(),
                &post_unload_snap,
            );
            let comps = [comp_a_h];
            let ptrs = [(&raw const payload_a).cast::<u8>()];
            // SAFETY: Valid `cmds` and `payload_a`; verifying stale handle rejection.
            unsafe {
                assert_eq!(
                    host_cmd_spawn(
                        cmds.as_ffi_ptr(),
                        comps.as_ptr(),
                        ptrs.as_ptr(),
                        1,
                        &mut ignored_ent,
                    ),
                    FfiResult::INVALID_STATE
                );
                assert_eq!(
                    host_cmd_spawn_persistent(
                        cmds.as_ffi_ptr(),
                        comps.as_ptr(),
                        ptrs.as_ptr(),
                        1,
                        &mut ignored_ent,
                    ),
                    FfiResult::INVALID_STATE
                );
                assert_eq!(
                    host_cmd_insert(
                        cmds.as_ffi_ptr(),
                        target_ffi,
                        comp_a_h,
                        (&raw const payload_a).cast::<u8>(),
                        8,
                    ),
                    FfiResult::INVALID_STATE
                );
            }
        }
        assert!(post_storage.is_empty());

        // 6. Applying & clearing commands queued BEFORE unload MUST NOT insert `comp_a_id` or invoke `drop_b`!
        pre_unload_storage.apply_to_world(&mut world, &pre_unload_snap);
        pre_unload_clear_storage.clear();
        assert!(!world.entity(target_ent).contains_id(comp_a_id));
        let spawned_bevy = Entity::from_bits(queued_spawn_ent.to_bits());
        assert!(!world.entity(spawned_bevy).contains_id(comp_a_id));
        assert_eq!(DROP_A_COUNT.load(Ordering::SeqCst), 0);
        assert_eq!(DROP_B_COUNT.load(Ordering::SeqCst), 0);

        let _ = available_drop_trampoline_slots();
    }
}
