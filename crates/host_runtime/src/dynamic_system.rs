#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Dynamic C-ABI System Adapter
//!
//! This module adapts foreign C-ABI function callbacks into native Bevy ECS [`System`]s.
//!
//! ## Core Architectural Invariants
//! 1. **Idiomatic `QueryBuilder` / `QueryState` & `FilteredAccessSet` Borrow Checking**:
//!    - During [`DynamicCSystem::initialize`], each foreign query group is constructed via Bevy's
//!      native [`QueryBuilder`] (`ref_id`, `mut_id`, `with_id`, `without_id`) into a [`QueryState`],
//!      automatically inheriting [`DefaultQueryFilters`](bevy_ecs::query::DefaultQueryFilters) and
//!      exact dense vs. sparse storage classification.
//!    - When all accessed and filtered components use [`StorageType::Table`], dense table iteration
//!      is used. When any accessed or filtered component uses [`StorageType::SparseSet`], archetype-aware
//!      iteration (`HostChunkIter::new_archetypal`) visits only matching archetypes and supports both
//!      `Table` and `SparseSet` data columns without data races or null columns.
//! 2. **Intra-System Conflict Detection**:
//!    - If a single system specifies mutually conflicting queries (e.g. Query 0 writes Component A
//!      and Query 1 reads or writes Component A), `access_set.is_compatible(&candidate_set)` returns `false`.
//!    - The system detects this self-conflict, flags `self_conflict = true; self.enabled = false;`,
//!      and safely disables itself without causing undefined behavior.
//! 3. **Incremental `ArchetypeGeneration` Caching**:
//!    - Only newly created archetypes (`&archetypes[old_gen..]`) are scanned when
//!      `world.archetypes().generation()` advances, achieving zero heap allocations in steady state.
//! 4. **Change Tick Advancement & Thread-Local Registry Isolation**:
//!    - Matches Bevy's `FunctionSystem`: advances `world.increment_change_tick()` on every run so
//!      downstream `Changed<T>` queries observe exact per-system tick boundaries.
//!    - Binds [`ModRegistry::make_current`] on the worker thread inside `run_unsafe` so multi-threaded
//!      executor worker threads resolve symbols strictly against that `World`'s registry.
//! 5. **Lock-Free Per-World Drop Trampolines**:
//!    - Supports up to [`MAX_DROP_TRAMPOLINES`] distinct `(WorldId, ComponentUuid)` dynamic drop
//!      registrations with lock-free atomic reads on the drop hot path and full multi-World isolation.

use crate::chunk_iter::{HostChunkIter, QueryColumnDesc, MAX_QUERY_COMPONENTS};
use crate::commands::CommandQueueBridge;
use crate::host::ModId;
use crate::registry::{ModRegistry, RegisteredComponent};
use abi_contract::{
    read_descriptor, ComponentAccessMode, ComponentDropFn, DynamicSystemCallback, FfiResult,
    MultiQuerySystemCallback, MultiQuerySystemDescriptor, RawChunkIter, RawCommandQueue,
    ScheduleLabelId, ScheduledMultiQuerySystemDescriptor, ScheduledSystemDescriptor,
    SystemDescriptor,
};
use bevy_ecs::archetype::{ArchetypeGeneration, ArchetypeId};
use bevy_ecs::change_detection::{CheckChangeTicks, Tick};
use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, StorageType};
use bevy_ecs::event::Event;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::query::{FilteredAccessSet, QueryBuilder, QueryState};
use bevy_ecs::schedule::{InternedSystemSet, SystemSet};
use bevy_ecs::storage::TableId;
use bevy_ecs::system::{RunSystemError, System, SystemAccess, SystemIn, SystemStateFlags};
use bevy_ecs::utils::DebugName;
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy_ecs::world::{CommandQueue, DeferredWorld, World, WorldId};
use core::ffi::{c_void, CStr};
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// Maximum number of concurrent/nested queries supported per multi-query dynamic system.
pub const MAX_SYSTEM_QUERIES: usize = 8;

/// Maximum number of distinct `(WorldId, ComponentUuid)` foreign drop trampolines supported per process.
pub const MAX_DROP_TRAMPOLINES: usize = 1024;

type DropGuardPair = (ComponentDropFn, Arc<AtomicBool>);

struct DropSlotSnapshot {
    entries: Box<[DropGuardPair]>,
}

static DROP_SLOT_SNAPSHOTS: [AtomicPtr<DropSlotSnapshot>; MAX_DROP_TRAMPOLINES] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_DROP_TRAMPOLINES];

struct DropSlotMeta {
    world_id: WorldId,
    uuid: abi_contract::ComponentUuid,
    entries: Vec<DropGuardPair>,
}

struct DropRegistryState {
    slots: Vec<DropSlotMeta>,
    /// Retains retired `DropSlotSnapshot` allocations across rare hot-reloads or test resets so
    /// lock-free readers on `DROP_SLOT_SNAPSHOTS` never dereference freed memory.
    retired: Vec<usize>,
}

static DROP_REGISTRY: LazyLock<Mutex<DropRegistryState>> = LazyLock::new(|| {
    Mutex::new(DropRegistryState {
        slots: Vec::new(),
        retired: Vec::new(),
    })
});

unsafe fn drop_trampoline<const SLOT: usize>(ptr: OwningPtr<'_>) {
    let snap_ptr = DROP_SLOT_SNAPSHOTS[SLOT].load(Ordering::Acquire);
    if !snap_ptr.is_null() {
        // SAFETY: `snap_ptr` points to a valid `DropSlotSnapshot` whose lifetime is retained
        // for the lifetime of the process (or in `DROP_REGISTRY.retired` across updates).
        let snap = unsafe { &*snap_ptr };
        if let Some(&(drop_fn, _)) = snap
            .entries
            .iter()
            .rev()
            .find(|(_, guard)| guard.load(Ordering::Acquire))
        {
            let raw = ptr.as_ptr() as *mut c_void;
            // SAFETY: Foreign drop function invoked on valid component instance while its own
            // paired mod instance (`guard == true`) remains mapped in memory.
            unsafe { drop_fn(raw) };
        }
    }
}

macro_rules! tramp_16 {
    ($b:expr) => {
        [
            drop_trampoline::<{ $b }>,
            drop_trampoline::<{ $b + 1 }>,
            drop_trampoline::<{ $b + 2 }>,
            drop_trampoline::<{ $b + 3 }>,
            drop_trampoline::<{ $b + 4 }>,
            drop_trampoline::<{ $b + 5 }>,
            drop_trampoline::<{ $b + 6 }>,
            drop_trampoline::<{ $b + 7 }>,
            drop_trampoline::<{ $b + 8 }>,
            drop_trampoline::<{ $b + 9 }>,
            drop_trampoline::<{ $b + 10 }>,
            drop_trampoline::<{ $b + 11 }>,
            drop_trampoline::<{ $b + 12 }>,
            drop_trampoline::<{ $b + 13 }>,
            drop_trampoline::<{ $b + 14 }>,
            drop_trampoline::<{ $b + 15 }>,
        ]
    };
}

macro_rules! tramp_256 {
    ($b:expr) => {
        [
            tramp_16!($b),
            tramp_16!($b + 16),
            tramp_16!($b + 32),
            tramp_16!($b + 48),
            tramp_16!($b + 64),
            tramp_16!($b + 80),
            tramp_16!($b + 96),
            tramp_16!($b + 112),
            tramp_16!($b + 128),
            tramp_16!($b + 144),
            tramp_16!($b + 160),
            tramp_16!($b + 176),
            tramp_16!($b + 192),
            tramp_16!($b + 208),
            tramp_16!($b + 224),
            tramp_16!($b + 240),
        ]
    };
}

type DropTrampolineFn = for<'a> unsafe fn(OwningPtr<'a>);
type DropTrampolineBlock = [[DropTrampolineFn; 16]; 16];

static DROP_TRAMPOLINE_BLOCKS: [DropTrampolineBlock; 4] = [
    tramp_256!(0),
    tramp_256!(256),
    tramp_256!(512),
    tramp_256!(768),
];

#[inline]
fn drop_trampoline_fn_for_slot(slot: usize) -> DropTrampolineFn {
    let b256 = slot / 256;
    let rem = slot % 256;
    let b16 = rem / 16;
    let idx = rem % 16;
    DROP_TRAMPOLINE_BLOCKS[b256][b16][idx]
}

fn allocate_drop_trampoline(
    world_id: WorldId,
    uuid: abi_contract::ComponentUuid,
    drop_fn: unsafe extern "C" fn(ptr: *mut c_void),
    is_active: Arc<AtomicBool>,
) -> Option<for<'a> unsafe fn(OwningPtr<'a>)> {
    let mut reg = DROP_REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // 1. Same-(WorldId, ComponentUuid) hot-reload / multi-mod binding: isolated per World, and
    //    each `drop_fn` stays strictly paired with its own `is_active` guard so unloading one mod
    //    never invokes its unmapped `drop_fn` via another still-loaded mod's guard!
    if let Some((slot_idx, meta)) = reg
        .slots
        .iter_mut()
        .enumerate()
        .find(|(_, m)| m.world_id == world_id && m.uuid == uuid)
    {
        meta.entries.retain(|(_, g)| g.load(Ordering::Acquire));
        if let Some(existing) = meta
            .entries
            .iter_mut()
            .find(|(_, g)| Arc::ptr_eq(g, &is_active))
        {
            existing.0 = drop_fn;
        } else {
            meta.entries.push((drop_fn, is_active));
        }
        let new_snap = Box::into_raw(Box::new(DropSlotSnapshot {
            entries: meta.entries.clone().into_boxed_slice(),
        }));
        let old_snap = DROP_SLOT_SNAPSHOTS[slot_idx].swap(new_snap, Ordering::AcqRel);
        if !old_snap.is_null() {
            reg.retired.push(old_snap as usize);
        }
        return Some(drop_trampoline_fn_for_slot(slot_idx));
    }

    // 2. Allocate a fresh slot for this (WorldId, ComponentUuid)
    let slot_idx = reg.slots.len();
    if slot_idx >= MAX_DROP_TRAMPOLINES {
        return None;
    }

    let entries = vec![(drop_fn, is_active)];
    let new_snap = Box::into_raw(Box::new(DropSlotSnapshot {
        entries: entries.clone().into_boxed_slice(),
    }));
    let old_snap = DROP_SLOT_SNAPSHOTS[slot_idx].swap(new_snap, Ordering::AcqRel);
    if !old_snap.is_null() {
        reg.retired.push(old_snap as usize);
    }
    reg.slots.push(DropSlotMeta {
        world_id,
        uuid,
        entries,
    });
    Some(drop_trampoline_fn_for_slot(slot_idx))
}

/// Resets all drop trampoline slots to unassigned.
///
/// # Safety / Warning
/// Intended strictly for resetting global trampoline state between isolated integration tests.
#[doc(hidden)]
pub fn reset_drop_trampolines_for_testing() {
    let mut reg = DROP_REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let used = reg.slots.len();
    for slot in DROP_SLOT_SNAPSHOTS.iter().take(used) {
        let old = slot.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !old.is_null() {
            reg.retired.push(old as usize);
        }
    }
    reg.slots.clear();
}

/// Bevy [`SystemSet`] labels automatically attached to dynamic guest systems.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModSystemSet {
    /// Contains all dynamic guest systems across all loaded mods.
    All,
    /// Contains all dynamic guest systems belonging to a specific [`ModId`].
    Mod(ModId),
}

/// Target Bevy schedule for a dynamic guest system (matches `ScheduleLabelId` discriminant values).
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ModSchedule {
    /// Main per-frame `Update` schedule (`0`).
    #[default]
    Update = 0,
    /// One-time `Startup` schedule (`1`).
    Startup = 1,
    /// `PreUpdate` schedule (`2`).
    PreUpdate = 2,
    /// `PostUpdate` schedule (`3`).
    PostUpdate = 3,
    /// Fixed-timestep `FixedUpdate` schedule (`4`).
    FixedUpdate = 4,
    /// End-of-frame `Last` schedule (`5`).
    Last = 5,
}

impl ModSchedule {
    /// Converts a raw `u32` discriminant (`ScheduleLabelId`) into [`ModSchedule`].
    #[inline]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Update),
            1 => Some(Self::Startup),
            2 => Some(Self::PreUpdate),
            3 => Some(Self::PostUpdate),
            4 => Some(Self::FixedUpdate),
            5 => Some(Self::Last),
            _ => None,
        }
    }

    /// Converts an [`abi_contract::ScheduleLabelId`] into [`ModSchedule`].
    #[inline]
    pub const fn from_label_id(id: ScheduleLabelId) -> Option<Self> {
        Self::from_raw(id.0)
    }

    /// Converts this [`ModSchedule`] into an [`abi_contract::ScheduleLabelId`].
    #[inline]
    pub const fn to_label_id(self) -> ScheduleLabelId {
        ScheduleLabelId(self as u32)
    }
}

/// Error-handling policy governing how [`DynamicCSystem`] responds when a guest callback
/// returns an error [`FfiResult`] or unwinds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModErrorPolicy {
    /// Quarantines the faulting guest system (`enabled = false`) and returns `Ok(())` (default).
    #[default]
    Quarantine,
    /// Quarantines the faulting guest system, logs diagnostic details to stderr, and returns `Ok(())`.
    Log,
    /// Quarantines the faulting guest system and propagates [`ModSystemError`] via
    /// `Err(RunSystemError::Failed(...))` to Bevy's error handler.
    ReportToBevy,
}

/// World resource configuring the default [`ModErrorPolicy`] for dynamic guest systems.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModErrorPolicyConfig(pub ModErrorPolicy);

/// Typed error reported to Bevy when [`ModErrorPolicy::ReportToBevy`] is active and a guest system faults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModSystemError {
    /// Optional [`ModId`] owning the faulting system.
    pub mod_id: Option<ModId>,
    /// Registered name of the faulting guest system.
    pub system_name: String,
    /// Error status returned by the callback (or `FfiResult::PANIC_CAUGHT` on unwind).
    pub status: FfiResult,
    /// Whether an unwind was caught at the callback boundary.
    pub panicked: bool,
}

impl fmt::Display for ModSystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "guest mod system '{}' (mod_id={:?}) faulted with {:?} (panicked={})",
            self.system_name, self.mod_id, self.status, self.panicked
        )
    }
}

impl std::error::Error for ModSystemError {}

/// Bevy [`Event`] and buffered [`Message`] emitted whenever a guest system faults and is quarantined.
#[derive(Event, Message, Debug, Clone, PartialEq, Eq)]
pub struct ModSystemFaulted {
    /// [`ModId`] owning the faulting system, if tracked.
    pub mod_id: Option<ModId>,
    /// Name of the faulting guest system.
    pub system_name: String,
    /// Fault [`FfiResult`] status code.
    pub status: FfiResult,
    /// `true` if a panic/unwind was caught.
    pub panicked: bool,
}

/// Owned representation of component access mode and component name for a dynamic query clause.
#[derive(Clone, Debug)]
pub struct ComponentAccessDescOwned {
    /// Canonical component name.
    pub name: String,
    /// Requested component access mode (`READ`, `WRITE`, `FILTER_ONLY`, `WITHOUT`).
    pub mode: ComponentAccessMode,
}

struct CachedQueryGroup {
    descs: Vec<ComponentAccessDescOwned>,
    columns: Vec<QueryColumnDesc>,
    query_state: Option<QueryState<(), ()>>,
    is_dense: bool,
    sparse_column_mask: u32,
    archetype_generation: ArchetypeGeneration,
    matched_tables: Vec<TableId>,
    matched_table_bits: Vec<bool>,
    matched_archetypes: Vec<ArchetypeId>,
}

impl CachedQueryGroup {
    fn new(descs: Vec<ComponentAccessDescOwned>) -> Self {
        Self {
            descs,
            columns: Vec::new(),
            query_state: None,
            is_dense: true,
            sparse_column_mask: 0,
            archetype_generation: ArchetypeGeneration::initial(),
            matched_tables: Vec::new(),
            matched_table_bits: Vec::new(),
            matched_archetypes: Vec::new(),
        }
    }

    fn reset_cache(&mut self) {
        self.archetype_generation = ArchetypeGeneration::initial();
        self.matched_tables.clear();
        self.matched_table_bits.clear();
        self.matched_archetypes.clear();
    }

    fn update_archetypes(&mut self, world: UnsafeWorldCell<'_>) {
        let archetypes = world.archetypes();
        let new_gen = archetypes.generation();
        if self.archetype_generation == new_gen {
            return;
        }
        let old_gen = core::mem::replace(&mut self.archetype_generation, new_gen);

        let Some(ref mut state) = self.query_state else {
            return;
        };
        state.update_archetypes_unsafe_world_cell(world);

        // Incremental scan: inspect ONLY newly created archetypes in `old_gen..new_gen`!
        for archetype in &archetypes[old_gen..] {
            if state.matches_component_set(&|id| archetype.contains(id)) {
                if self.is_dense {
                    let table_id = archetype.table_id();
                    let idx = table_id.as_usize();
                    if idx >= self.matched_table_bits.len() {
                        self.matched_table_bits.resize(idx + 1, false);
                    }
                    if !self.matched_table_bits[idx] {
                        self.matched_table_bits[idx] = true;
                        self.matched_tables.push(table_id);
                    }
                } else {
                    self.matched_archetypes.push(archetype.id());
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum SystemCallbackKind {
    Single(DynamicSystemCallback),
    Multi(MultiQuerySystemCallback),
}

/// Host-side Bevy System wrapping an opaque guest C-ABI system callback (single or multi-query).
pub struct DynamicCSystem {
    name: String,
    query_groups: Vec<CachedQueryGroup>,

    access: SystemAccess,
    self_conflict: bool,
    missing_components: Vec<String>,
    last_run: Tick,
    this_run: Tick,

    callback: SystemCallbackKind,
    user_data: *mut c_void,

    mod_id: Option<ModId>,
    schedule: ModSchedule,
    after_systems: Vec<String>,
    before_systems: Vec<String>,
    error_policy: ModErrorPolicy,
    error_policy_explicit: bool,

    enabled: bool,
    is_active: Arc<AtomicBool>,
    command_queue: CommandQueue,
    registry: Arc<ModRegistry>,
}

// SAFETY: DynamicCSystem access is governed by Bevy scheduler through FilteredAccessSet.
// user_data is guaranteed Send + Sync by the mod contract.
unsafe impl Send for DynamicCSystem {}
unsafe impl Sync for DynamicCSystem {}

impl DynamicCSystem {
    /// Creates a single-query dynamic C-ABI system.
    pub fn new(
        name: String,
        query_descs: Vec<ComponentAccessDescOwned>,
        callback: DynamicSystemCallback,
        user_data: *mut c_void,
        registry: Arc<ModRegistry>,
    ) -> Self {
        Self {
            name,
            query_groups: vec![CachedQueryGroup::new(query_descs)],
            access: SystemAccess::None,
            self_conflict: false,
            missing_components: Vec::new(),
            last_run: Tick::new(0),
            this_run: Tick::new(0),
            callback: SystemCallbackKind::Single(callback),
            user_data,
            mod_id: None,
            schedule: ModSchedule::Update,
            after_systems: Vec::new(),
            before_systems: Vec::new(),
            error_policy: ModErrorPolicy::Quarantine,
            error_policy_explicit: false,
            enabled: true,
            is_active: Arc::new(AtomicBool::new(true)),
            command_queue: CommandQueue::default(),
            registry,
        }
    }

    /// Creates a multi-query dynamic C-ABI system supporting up to [`MAX_SYSTEM_QUERIES`] nested queries.
    pub fn new_multi_query(
        name: String,
        query_specs: Vec<Vec<ComponentAccessDescOwned>>,
        callback: MultiQuerySystemCallback,
        user_data: *mut c_void,
        registry: Arc<ModRegistry>,
    ) -> Self {
        let query_groups = query_specs.into_iter().map(CachedQueryGroup::new).collect();
        Self {
            name,
            query_groups,
            access: SystemAccess::None,
            self_conflict: false,
            missing_components: Vec::new(),
            last_run: Tick::new(0),
            this_run: Tick::new(0),
            callback: SystemCallbackKind::Multi(callback),
            user_data,
            mod_id: None,
            schedule: ModSchedule::Update,
            after_systems: Vec::new(),
            before_systems: Vec::new(),
            error_policy: ModErrorPolicy::Quarantine,
            error_policy_explicit: false,
            enabled: true,
            is_active: Arc::new(AtomicBool::new(true)),
            command_queue: CommandQueue::default(),
            registry,
        }
    }

    /// Returns the guest system's registered name.
    pub fn system_name(&self) -> &str {
        &self.name
    }

    /// Returns `true` if the system is currently enabled to execute.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enables or disables execution of this system.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Replaces the component registry snapshot used by this system.
    pub(crate) fn set_registry(&mut self, registry: Arc<ModRegistry>) {
        self.registry = registry;
    }

    /// Returns `true` if the system was disabled due to internal query borrow conflicts.
    pub fn has_self_conflict(&self) -> bool {
        self.self_conflict
    }

    /// Returns any component names that failed to resolve during `initialize` or validation.
    pub fn missing_components(&self) -> &[String] {
        &self.missing_components
    }

    /// Validates that all queried component names exist in the given [`ModRegistry`].
    /// Returns `Err(missing_component_names)` if one or more components are unregistered.
    pub fn validate_against_registry(&self, registry: &ModRegistry) -> Result<(), Vec<String>> {
        let mut missing = Vec::new();
        for group in &self.query_groups {
            for desc in &group.descs {
                if registry.get_by_name(&desc.name).is_none() && !missing.contains(&desc.name) {
                    missing.push(desc.name.clone());
                }
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        }
    }

    /// Returns the [`ModId`] associated with this system, if any.
    pub fn mod_id(&self) -> Option<ModId> {
        self.mod_id
    }

    /// Associates this system with a [`ModId`] so it joins [`ModSystemSet::Mod`].
    pub fn set_mod_id(&mut self, mod_id: ModId) {
        self.mod_id = Some(mod_id);
    }

    /// Builder helper to associate this system with a [`ModId`].
    pub fn with_mod_id(mut self, mod_id: ModId) -> Self {
        self.mod_id = Some(mod_id);
        self
    }

    /// Returns the target [`ModSchedule`] requested for this system.
    pub fn schedule(&self) -> ModSchedule {
        self.schedule
    }

    /// Sets the target [`ModSchedule`] for this system.
    pub fn set_schedule(&mut self, schedule: ModSchedule) {
        self.schedule = schedule;
    }

    /// Builder helper to set the target [`ModSchedule`] for this system.
    pub fn with_schedule(mut self, schedule: ModSchedule) -> Self {
        self.schedule = schedule;
        self
    }

    /// Builder helper to configure explicit `after` and `before` system-name ordering dependencies.
    pub fn with_ordering(mut self, after: Vec<String>, before: Vec<String>) -> Self {
        self.after_systems = after;
        self.before_systems = before;
        self
    }

    /// System names declared in `ScheduledSystemDescriptor::after`.
    pub fn after_systems(&self) -> &[String] {
        &self.after_systems
    }

    /// System names declared in `ScheduledSystemDescriptor::before`.
    pub fn before_systems(&self) -> &[String] {
        &self.before_systems
    }

    /// Returns the [`ModErrorPolicy`] governing faults in this system.
    pub fn error_policy(&self) -> ModErrorPolicy {
        self.error_policy
    }

    /// Overrides the [`ModErrorPolicy`] for this system.
    pub fn set_error_policy(&mut self, policy: ModErrorPolicy) {
        self.error_policy = policy;
        self.error_policy_explicit = true;
    }

    /// Builder helper to override the [`ModErrorPolicy`] for this system.
    pub fn with_error_policy(mut self, policy: ModErrorPolicy) -> Self {
        self.error_policy = policy;
        self.error_policy_explicit = true;
        self
    }

    /// Obtains a clone of the atomic tombstone flag.
    pub fn is_active(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.is_active)
    }

    /// Sets the atomic tombstone flag for this system.
    pub fn set_is_active(&mut self, is_active: Arc<AtomicBool>) {
        self.is_active = is_active;
    }

    /// Fluent builder method to attach an atomic tombstone flag.
    pub fn with_is_active(mut self, is_active: Arc<AtomicBool>) -> Self {
        self.is_active = is_active;
        self
    }
}

impl System for DynamicCSystem {
    type In = ();
    type Out = ();

    fn name(&self) -> DebugName {
        DebugName::from(self.name.clone())
    }

    fn flags(&self) -> SystemStateFlags {
        SystemStateFlags::DEFERRED
    }

    fn default_system_sets(&self) -> Vec<InternedSystemSet> {
        let mut sets = vec![SystemSet::intern(&ModSystemSet::All)];
        if let Some(id) = self.mod_id {
            sets.push(SystemSet::intern(&ModSystemSet::Mod(id)));
        }
        sets
    }

    fn initialize(&mut self, world: &mut World) -> SystemAccess {
        // Match Bevy's FunctionSystem::initialize tick semantics (`change_tick.relative_to(Tick::MAX)`)
        self.this_run = world.change_tick();
        self.last_run = Tick::new(world.change_tick().get().wrapping_sub(Tick::MAX.get()));
        self.self_conflict = false;
        self.missing_components.clear();

        if !self.error_policy_explicit {
            if let Some(cfg) = world.get_resource::<ModErrorPolicyConfig>() {
                self.error_policy = cfg.0;
            }
        }

        // Synchronize with world's ModRegistry to ensure dynamically registered components are available
        if let Some(world_reg) = world.get_resource::<ModRegistry>() {
            self.registry = Arc::new(world_reg.clone());
        }

        let mut access_set = FilteredAccessSet::new();

        for group in &mut self.query_groups {
            group.columns.clear();
            group.query_state = None;
            group.sparse_column_mask = 0;
            group.reset_cache();

            if group.descs.len() > MAX_QUERY_COMPONENTS {
                self.enabled = false;
                self.access = SystemAccess::None;
                return self.access.clone();
            }

            let mut read_components = std::collections::HashSet::new();
            let mut write_components = std::collections::HashSet::new();
            let mut resolved = Vec::with_capacity(group.descs.len());

            for (col_idx, desc) in group.descs.iter().enumerate() {
                if !desc.mode.is_valid() {
                    self.enabled = false;
                    self.access = SystemAccess::None;
                    return self.access.clone();
                }

                let Some(reg) = self.registry.get_by_name(&desc.name) else {
                    self.missing_components.push(desc.name.clone());
                    self.enabled = false;
                    self.access = SystemAccess::None;
                    return self.access.clone();
                };

                let comp_id = reg.component_id;
                let stride = reg.size;

                match desc.mode {
                    ComponentAccessMode::READ => {
                        if write_components.contains(&comp_id) {
                            self.self_conflict = true;
                            self.enabled = false;
                            self.access = SystemAccess::None;
                            return self.access.clone();
                        }
                        read_components.insert(comp_id);
                    }
                    ComponentAccessMode::WRITE => {
                        if read_components.contains(&comp_id) || write_components.contains(&comp_id)
                        {
                            self.self_conflict = true;
                            self.enabled = false;
                            self.access = SystemAccess::None;
                            return self.access.clone();
                        }
                        write_components.insert(comp_id);
                    }
                    ComponentAccessMode::FILTER_ONLY | ComponentAccessMode::WITHOUT => {}
                    _ => {
                        self.enabled = false;
                        self.access = SystemAccess::None;
                        return self.access.clone();
                    }
                }

                let is_sparse_storage = world
                    .components()
                    .get_info(comp_id)
                    .is_none_or(|info| info.storage_type() == StorageType::SparseSet);

                if is_sparse_storage
                    && (desc.mode == ComponentAccessMode::READ
                        || desc.mode == ComponentAccessMode::WRITE)
                {
                    group.sparse_column_mask |= 1u32 << col_idx;
                }

                resolved.push((comp_id, desc.mode, stride));
            }

            let mut builder = QueryBuilder::<()>::new(world);
            for &(comp_id, mode, stride) in &resolved {
                match mode {
                    ComponentAccessMode::READ => {
                        builder.ref_id(comp_id);
                    }
                    ComponentAccessMode::WRITE => {
                        builder.mut_id(comp_id);
                    }
                    ComponentAccessMode::FILTER_ONLY => {
                        builder.with_id(comp_id);
                    }
                    ComponentAccessMode::WITHOUT => {
                        builder.without_id(comp_id);
                    }
                    _ => {}
                }
                group.columns.push(QueryColumnDesc {
                    component_id: comp_id,
                    mode,
                    stride,
                });
            }

            let state = builder.build();
            let is_comp_dense = |id| {
                world
                    .components()
                    .get_info(id)
                    .is_some_and(|info| info.storage_type() == StorageType::Table)
            };
            group.is_dense = group.sparse_column_mask == 0
                && state.component_access().with_filters().all(is_comp_dense)
                && state
                    .component_access()
                    .without_filters()
                    .all(is_comp_dense);

            let filtered_access = state.component_access().clone();
            group.query_state = Some(state);

            let mut candidate_set = FilteredAccessSet::new();
            candidate_set.add(filtered_access.clone());
            if !access_set.is_compatible(&candidate_set) {
                self.self_conflict = true;
                self.enabled = false;
            }

            access_set.add(filtered_access);
        }

        if self.self_conflict || !self.enabled {
            self.access = SystemAccess::None;
            return self.access.clone();
        }

        self.access = SystemAccess::Shared(access_set);

        // Scan initial archetypes incrementally from ArchetypeGeneration::initial()
        let cell = world.as_unsafe_world_cell();
        for group in &mut self.query_groups {
            group.update_archetypes(cell);
        }

        self.access.clone()
    }

    unsafe fn run_unsafe(
        &mut self,
        _input: SystemIn<'_, Self>,
        world: UnsafeWorldCell,
    ) -> Result<Self::Out, RunSystemError> {
        if !self.enabled || self.self_conflict || !self.is_active.load(Ordering::Acquire) {
            return Ok(());
        }

        // Match Bevy's FunctionSystem::run_unsafe change tick progression
        let change_tick = world.increment_change_tick();
        self.this_run = change_tick;

        // Bind this World's ModRegistry on the executing worker thread so any runtime
        // `get_proc_address` call inside the guest resolves strictly against this World.
        let _reg_guard = self.registry.make_current();

        // Incremental archetype update: zero heap allocations in steady-state
        for group in &mut self.query_groups {
            group.update_archetypes(world);
        }

        // SAFETY: Scheduler verified access via FilteredAccessSet; no intra-system conflict exists.
        let storages = unsafe { world.storages() };
        let archetypes = world.archetypes();

        let mut bridge = CommandQueueBridge::new(
            &mut self.command_queue,
            world.entity_allocator(),
            &self.registry,
        );
        let commands_ptr = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
        let user_data = self.user_data;

        let res = match self.callback {
            SystemCallbackKind::Single(callback) => {
                let group = &self.query_groups[0];
                let mut host_iter = if group.is_dense {
                    HostChunkIter::new(
                        &storages.tables,
                        &group.matched_tables,
                        &group.columns,
                        change_tick,
                    )
                } else {
                    HostChunkIter::new_archetypal(
                        &storages.tables,
                        archetypes,
                        &storages.sparse_sets,
                        &group.matched_archetypes,
                        &group.columns,
                        group.sparse_column_mask,
                        change_tick,
                    )
                };
                let iter_ptr = (&mut host_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();
                // SAFETY: Calling foreign C-ABI callback with valid stack-allocated iterator and command bridge.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    callback(iter_ptr, commands_ptr, user_data)
                }))
            }
            SystemCallbackKind::Multi(callback) => {
                let count = self.query_groups.len().min(MAX_SYSTEM_QUERIES);
                let mut host_iters: [Option<HostChunkIter<'_>>; MAX_SYSTEM_QUERIES] =
                    [const { None }; MAX_SYSTEM_QUERIES];
                for (i, group) in self.query_groups.iter().take(count).enumerate() {
                    let iter = if group.is_dense {
                        HostChunkIter::new(
                            &storages.tables,
                            &group.matched_tables,
                            &group.columns,
                            change_tick,
                        )
                    } else {
                        HostChunkIter::new_archetypal(
                            &storages.tables,
                            archetypes,
                            &storages.sparse_sets,
                            &group.matched_archetypes,
                            &group.columns,
                            group.sparse_column_mask,
                            change_tick,
                        )
                    };
                    host_iters[i] = Some(iter);
                }

                let mut raw_ptrs: [*mut RawChunkIter; MAX_SYSTEM_QUERIES] =
                    [core::ptr::null_mut(); MAX_SYSTEM_QUERIES];
                for i in 0..count {
                    if let Some(ref mut it) = host_iters[i] {
                        raw_ptrs[i] = (it as *mut HostChunkIter<'_>).cast::<RawChunkIter>();
                    }
                }

                // SAFETY: All `raw_ptrs[0..count]` point to disjoint stack-allocated `HostChunkIter`s
                // whose borrow compatibility was verified by `FilteredAccessSet` in `initialize`.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    callback(raw_ptrs.as_ptr(), count, commands_ptr, user_data)
                }))
            }
        };

        self.last_run = change_tick;

        let fault = match res {
            Ok(status) => {
                if status == FfiResult::PANIC_CAUGHT || status.is_err() {
                    Some((status, status == FfiResult::PANIC_CAUGHT))
                } else {
                    None
                }
            }
            Err(_) => Some((FfiResult::PANIC_CAUGHT, true)),
        };

        if let Some((status, panicked)) = fault {
            self.enabled = false;
            let fault_event = ModSystemFaulted {
                mod_id: self.mod_id,
                system_name: self.name.clone(),
                status,
                panicked,
            };
            self.command_queue.push(move |world: &mut World| {
                if let Some(mut msgs) = world.get_resource_mut::<Messages<ModSystemFaulted>>() {
                    msgs.write(fault_event.clone());
                }
                world.trigger(fault_event);
            });

            match self.error_policy {
                ModErrorPolicy::Quarantine => return Ok(()),
                ModErrorPolicy::Log => {
                    eprintln!(
                        "[bevy_abi_mod] guest system '{}' (mod_id={:?}) faulted: status={:?}, panicked={}",
                        self.name, self.mod_id, status, panicked
                    );
                    return Ok(());
                }
                ModErrorPolicy::ReportToBevy => {
                    return Err(RunSystemError::from(ModSystemError {
                        mod_id: self.mod_id,
                        system_name: self.name.clone(),
                        status,
                        panicked,
                    }));
                }
            }
        }

        Ok(())
    }

    fn apply_deferred(&mut self, world: &mut World) {
        self.command_queue.apply(world);
    }

    fn run(
        &mut self,
        input: SystemIn<'_, Self>,
        world: &mut World,
    ) -> Result<Self::Out, RunSystemError> {
        let res = self.run_without_applying_deferred(input, world);
        self.apply_deferred(world);
        res
    }

    fn queue_deferred(&mut self, mut world: DeferredWorld) {
        world.commands().append(&mut self.command_queue);
    }

    fn check_change_tick(&mut self, check: CheckChangeTicks) {
        self.last_run.check_tick(check);
        self.this_run.check_tick(check);
    }

    fn get_last_run(&self) -> Tick {
        self.last_run
    }

    fn set_last_run(&mut self, last_run: Tick) {
        self.last_run = last_run;
    }
}

/// Context passed as `host_context` to `bevy_abi_mod_init`.
pub struct ModInitContext {
    /// Active component and symbol registry.
    pub registry: Arc<ModRegistry>,
    /// Accumulated dynamic systems registered during mod initialization.
    pub registered_systems: Vec<DynamicCSystem>,
    /// Shared atomic tombstone flag.
    pub is_active: Arc<AtomicBool>,
    /// Pointer to the Bevy World during mod initialization (null if not in World scope).
    pub world: *mut World,
    /// Dynamic components registered during initialization.
    pub registered_components: Vec<RegisteredComponent>,
}

impl ModInitContext {
    /// Creates a new initialization context with a fresh active flag.
    pub fn new(registry: Arc<ModRegistry>) -> Self {
        Self {
            registry,
            registered_systems: Vec::new(),
            is_active: Arc::new(AtomicBool::new(true)),
            world: core::ptr::null_mut(),
            registered_components: Vec::new(),
        }
    }

    /// Creates an initialization context sharing a specific active flag.
    pub fn with_is_active(registry: Arc<ModRegistry>, is_active: Arc<AtomicBool>) -> Self {
        Self {
            registry,
            registered_systems: Vec::new(),
            is_active,
            world: core::ptr::null_mut(),
            registered_components: Vec::new(),
        }
    }

    /// Creates an initialization context with a specific active flag and mutable World access.
    pub fn with_world(
        registry: Arc<ModRegistry>,
        is_active: Arc<AtomicBool>,
        world: *mut World,
    ) -> Self {
        Self {
            registry,
            registered_systems: Vec::new(),
            is_active,
            world,
            registered_components: Vec::new(),
        }
    }

    /// Marks this context as active on the current thread for the duration of the returned guard.
    pub fn make_active(&mut self) -> ActiveInitContextGuard {
        set_active_init_context(self as *mut ModInitContext)
    }
}

thread_local! {
    /// Active initialization context on the current thread.
    pub static ACTIVE_INIT_CONTEXT: std::cell::Cell<*mut ModInitContext> = const { std::cell::Cell::new(core::ptr::null_mut()) };
}

/// RAII guard restoring the previous thread-local [`ACTIVE_INIT_CONTEXT`] on drop.
pub struct ActiveInitContextGuard {
    prev: *mut ModInitContext,
}

impl Drop for ActiveInitContextGuard {
    fn drop(&mut self) {
        ACTIVE_INIT_CONTEXT.with(|c| c.set(self.prev));
    }
}

/// Sets the thread-local active [`ModInitContext`].
pub fn set_active_init_context(ctx: *mut ModInitContext) -> ActiveInitContextGuard {
    let prev = ACTIVE_INIT_CONTEXT.with(|c| c.replace(ctx));
    ActiveInitContextGuard { prev }
}

unsafe fn parse_cstr_name_array(
    ptr: *const *const core::ffi::c_char,
    count: usize,
) -> Result<Vec<String>, FfiResult> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    // SAFETY: Caller passes an array of `count` C-string pointers.
    let slice = unsafe { core::slice::from_raw_parts(ptr, count) };
    let mut out = Vec::with_capacity(count);
    for &entry in slice {
        if entry.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        // SAFETY: `entry` is checked non-null and points to a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(entry) }).to_str() else {
            return Err(FfiResult::INVALID_UTF8);
        };
        out.push(s.to_string());
    }
    Ok(out)
}

unsafe fn register_system_from_extensible_ptr(
    host_context: *mut c_void,
    descriptor_ptr: *const c_void,
    schedule_override: Option<ModSchedule>,
    truncated_err: FfiResult,
) -> FfiResult {
    if host_context.is_null() || descriptor_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Reads `struct_size` and copies at most `min(struct_size, size_of::<ScheduledSystemDescriptor>())`
    // bytes into a zero-filled `MaybeUninit`, validating non-null `callback` BEFORE `assume_init()`
    // to eliminate truncated-provenance and null-`fn`-niche UB!
    let sched_desc = match unsafe { read_descriptor::<ScheduledSystemDescriptor>(descriptor_ptr) } {
        Ok(d) => d,
        Err(FfiResult::INVALID_VERSION) => return truncated_err,
        Err(err) => return err,
    };

    let schedule = match schedule_override {
        Some(s) => s,
        None => {
            let Some(s) = ModSchedule::from_label_id(sched_desc.schedule) else {
                return FfiResult::INVALID_VERSION;
            };
            s
        }
    };

    // SAFETY: `sched_desc.after` / `before` are zero-filled (`count == 0`) for v1 descriptors,
    // or caller-provided arrays when `struct_size` covers `ScheduledSystemDescriptor`.
    let after = match unsafe { parse_cstr_name_array(sched_desc.after, sched_desc.after_count) } {
        Ok(v) => v,
        Err(err) => return err,
    };
    // SAFETY: Same contract for `before`.
    let before = match unsafe { parse_cstr_name_array(sched_desc.before, sched_desc.before_count) }
    {
        Ok(v) => v,
        Err(err) => return err,
    };

    let desc = sched_desc.base;

    // D8 fix: never silently truncate query columns beyond MAX_QUERY_COMPONENTS!
    if desc.query_count > MAX_QUERY_COMPONENTS {
        return FfiResult::INVALID_COMPONENT;
    }

    let sys_name = if !desc.name.is_null() {
        // SAFETY: desc.name is a NUL-terminated C string.
        unsafe { CStr::from_ptr(desc.name) }
            .to_str()
            .unwrap_or("unnamed_guest_system")
            .to_string()
    } else {
        "unnamed_guest_system".to_string()
    };

    let mut query_descs = Vec::with_capacity(desc.query_count);
    if desc.query_count > 0 {
        if desc.queries.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: desc.queries points to an array of length desc.query_count.
        let queries_slice = unsafe { core::slice::from_raw_parts(desc.queries, desc.query_count) };
        for q in queries_slice {
            if q.name.is_null() {
                return FfiResult::NULL_POINTER;
            }
            if !q.mode.is_valid() {
                return FfiResult::INVALID_COMPONENT;
            }
            // SAFETY: q.name is a NUL-terminated C string.
            let Ok(q_name) = (unsafe { CStr::from_ptr(q.name) }).to_str() else {
                return FfiResult::INVALID_UTF8;
            };
            query_descs.push(ComponentAccessDescOwned {
                name: q_name.to_string(),
                mode: q.mode,
            });
        }
    }

    // SAFETY: caller guarantees host_context points to a valid ModInitContext.
    let init_ctx = unsafe { &mut *(host_context as *mut ModInitContext) };
    let system = DynamicCSystem::new(
        sys_name,
        query_descs,
        desc.callback,
        desc.user_data,
        Arc::clone(&init_ctx.registry),
    )
    .with_schedule(schedule)
    .with_ordering(after, before)
    .with_is_active(Arc::clone(&init_ctx.is_active));
    init_ctx.registered_systems.push(system);

    FfiResult::OK
}

/// C-ABI implementation of `register_system` (defaults to [`ModSchedule::Update`] for v1
/// [`SystemDescriptor`]s, and transparently honors `schedule` / `after` / `before` when passed
/// an extended [`ScheduledSystemDescriptor`] via [`ScheduledSystemDescriptor::as_base_ptr`]).
///
/// # Safety
/// `host_context` must point to a valid [`ModInitContext`]. `descriptor` must point to a valid
/// [`SystemDescriptor`] or [`ScheduledSystemDescriptor`].
pub unsafe extern "C" fn host_register_system(
    host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
) -> FfiResult {
    // SAFETY: Delegating through `read_descriptor::<ScheduledSystemDescriptor>`.
    unsafe {
        register_system_from_extensible_ptr(
            host_context,
            descriptor.cast::<c_void>(),
            None,
            FfiResult::INTERNAL_ERROR,
        )
    }
}

/// C-ABI symbol `register_scheduled_system` matching [`abi_contract::RegisterScheduledSystemFn`].
///
/// # Safety
/// `host_context` must point to a valid [`ModInitContext`]. `descriptor` must point to a valid
/// [`ScheduledSystemDescriptor`].
pub unsafe extern "C" fn host_register_scheduled_system(
    host_context: *mut c_void,
    descriptor: *const ScheduledSystemDescriptor,
) -> FfiResult {
    // SAFETY: Delegating through `read_descriptor::<ScheduledSystemDescriptor>`.
    unsafe {
        register_system_from_extensible_ptr(
            host_context,
            descriptor.cast::<c_void>(),
            None,
            FfiResult::INVALID_VERSION,
        )
    }
}

/// C-ABI symbol `register_system_in_schedule` allowing a guest mod to target a specific
/// [`ModSchedule`] (`0 = Update`, `1 = Startup`, `2 = PreUpdate`, `3 = PostUpdate`, `4 = FixedUpdate`, `5 = Last`).
///
/// # Safety
/// `host_context` must point to a valid [`ModInitContext`]. `descriptor` must point to a valid [`SystemDescriptor`].
pub unsafe extern "C" fn host_register_system_in_schedule(
    host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
    schedule_raw: u32,
) -> FfiResult {
    let Some(schedule) = ModSchedule::from_raw(schedule_raw) else {
        return FfiResult::INTERNAL_ERROR;
    };
    // SAFETY: Delegating through `read_descriptor::<ScheduledSystemDescriptor>`.
    unsafe {
        register_system_from_extensible_ptr(
            host_context,
            descriptor.cast::<c_void>(),
            Some(schedule),
            FfiResult::INTERNAL_ERROR,
        )
    }
}

/// C-ABI implementation of `register_multi_query_system` for nested / multi-query ECS systems.
/// Transparently supports both v1 [`MultiQuerySystemDescriptor`] and extended
/// [`ScheduledMultiQuerySystemDescriptor`] via `struct_size` negotiation.
///
/// # Safety
/// `host_context` must point to a valid [`ModInitContext`]. `descriptor` must point to a valid
/// [`MultiQuerySystemDescriptor`] or [`ScheduledMultiQuerySystemDescriptor`].
pub unsafe extern "C" fn host_register_multi_query_system(
    host_context: *mut c_void,
    descriptor: *const MultiQuerySystemDescriptor,
) -> FfiResult {
    if host_context.is_null() || descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Safely copies `min(struct_size, size_of::<ScheduledMultiQuerySystemDescriptor>())`
    // and checks `callback != 0` before `assume_init()` (no provenance or `fn`-niche UB).
    let sched_desc = match unsafe {
        read_descriptor::<ScheduledMultiQuerySystemDescriptor>(descriptor.cast::<c_void>())
    } {
        Ok(d) => d,
        Err(FfiResult::INVALID_VERSION) => return FfiResult::INTERNAL_ERROR,
        Err(err) => return err,
    };

    let Some(schedule) = ModSchedule::from_label_id(sched_desc.schedule) else {
        return FfiResult::INVALID_VERSION;
    };

    // SAFETY: Zero-filled for v1 descriptors or caller-supplied pointer array for v2 descriptors.
    let after = match unsafe { parse_cstr_name_array(sched_desc.after, sched_desc.after_count) } {
        Ok(v) => v,
        Err(err) => return err,
    };
    // SAFETY: Same contract for `before`.
    let before = match unsafe { parse_cstr_name_array(sched_desc.before, sched_desc.before_count) }
    {
        Ok(v) => v,
        Err(err) => return err,
    };

    let desc = sched_desc.base;
    if desc.query_spec_count == 0 || desc.query_spec_count > MAX_SYSTEM_QUERIES {
        return FfiResult::INTERNAL_ERROR;
    }
    if desc.query_specs.is_null() {
        return FfiResult::NULL_POINTER;
    }

    let sys_name = if !desc.name.is_null() {
        // SAFETY: desc.name is a NUL-terminated C string.
        unsafe { CStr::from_ptr(desc.name) }
            .to_str()
            .unwrap_or("unnamed_multi_query_guest_system")
            .to_string()
    } else {
        "unnamed_multi_query_guest_system".to_string()
    };

    // SAFETY: desc.query_specs points to an array of length desc.query_spec_count.
    let specs_slice =
        unsafe { core::slice::from_raw_parts(desc.query_specs, desc.query_spec_count) };
    let mut parsed_specs = Vec::with_capacity(desc.query_spec_count);

    for spec in specs_slice {
        // D8 fix: reject > MAX_QUERY_COMPONENTS instead of silently truncating columns!
        if spec.query_count > MAX_QUERY_COMPONENTS {
            return FfiResult::INVALID_COMPONENT;
        }
        let mut query_descs = Vec::with_capacity(spec.query_count);
        if spec.query_count > 0 {
            if spec.queries.is_null() {
                return FfiResult::NULL_POINTER;
            }
            // SAFETY: spec.queries points to an array of length spec.query_count.
            let queries_slice =
                unsafe { core::slice::from_raw_parts(spec.queries, spec.query_count) };
            for q in queries_slice {
                if q.name.is_null() {
                    return FfiResult::NULL_POINTER;
                }
                if !q.mode.is_valid() {
                    return FfiResult::INVALID_COMPONENT;
                }
                // SAFETY: q.name is a NUL-terminated C string.
                let Ok(q_name) = (unsafe { CStr::from_ptr(q.name) }).to_str() else {
                    return FfiResult::INVALID_UTF8;
                };
                query_descs.push(ComponentAccessDescOwned {
                    name: q_name.to_string(),
                    mode: q.mode,
                });
            }
        }
        parsed_specs.push(query_descs);
    }

    // SAFETY: caller guarantees host_context points to a valid ModInitContext.
    let init_ctx = unsafe { &mut *(host_context as *mut ModInitContext) };
    let system = DynamicCSystem::new_multi_query(
        sys_name,
        parsed_specs,
        desc.callback,
        desc.user_data,
        Arc::clone(&init_ctx.registry),
    )
    .with_schedule(schedule)
    .with_ordering(after, before)
    .with_is_active(Arc::clone(&init_ctx.is_active));
    init_ctx.registered_systems.push(system);

    FfiResult::OK
}

/// C-ABI implementation of `register_component` for registering dynamic runtime components.
///
/// Performs comprehensive defensive C-ABI validation on the component descriptor and host context.
/// If called with an active `ModInitContext` backed by a Bevy `World`, dynamically registers the
/// component in the `World` and `ModRegistry`.
///
/// # Safety
/// - `host_context` must point to a valid host context (e.g. [`ModInitContext`]).
/// - `descriptor` must point to a valid [`abi_contract::ModComponentDescriptor`].
pub unsafe extern "C" fn host_register_component(
    host_context: *mut c_void,
    descriptor: *const abi_contract::ModComponentDescriptor,
) -> FfiResult {
    if host_context.is_null() || descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Safely reads `struct_size` first and copies `min(struct_size, size_of::<ModComponentDescriptor>())`
    // without forming `&ModComponentDescriptor` on truncated C buffers (preventing provenance UB).
    let desc = match unsafe {
        read_descriptor::<abi_contract::ModComponentDescriptor>(descriptor.cast::<c_void>())
    } {
        Ok(d) => d,
        Err(err) => return err,
    };
    if desc.name.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: desc.name is checked non-null and is assumed to point to a NUL-terminated C string.
    let Ok(name_str) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
        return FfiResult::INVALID_UTF8;
    };
    if desc.align == 0 || !desc.align.is_power_of_two() {
        return FfiResult::INTERNAL_ERROR;
    }
    if !desc.size.is_multiple_of(desc.align) {
        return FfiResult::INTERNAL_ERROR;
    }
    let Some(layout) = desc.layout() else {
        return FfiResult::INTERNAL_ERROR;
    };

    let active_ptr = ACTIVE_INIT_CONTEXT.with(|c| c.get());
    if !active_ptr.is_null() {
        if host_context != (active_ptr as *mut c_void) {
            return FfiResult::INTERNAL_ERROR;
        }
        // SAFETY: active_ptr was set from a valid reference to ModInitContext on this thread.
        let init_ctx = unsafe { &mut *active_ptr };
        if init_ctx.world.is_null() {
            return FfiResult::INTERNAL_ERROR;
        }
        // SAFETY: init_ctx.world was created from a valid &mut World.
        let world = unsafe { &mut *init_ctx.world };
        let world_id = world.id();

        // Check if component already exists with identical layout (idempotent registration / reload)
        if let Some(existing) = world
            .get_resource::<ModRegistry>()
            .and_then(|r| r.get_by_name(name_str))
        {
            if existing.size == desc.size
                && existing.align == desc.align
                && existing.uuid == desc.uuid
            {
                // Update drop trampoline slot for the reloaded mod instance in THIS World
                if let Some(drop_fn) = desc.drop_fn {
                    match allocate_drop_trampoline(
                        world_id,
                        desc.uuid,
                        drop_fn,
                        Arc::clone(&init_ctx.is_active),
                    ) {
                        Some(_) => {}
                        None => return FfiResult::INTERNAL_ERROR,
                    }
                }
                if !init_ctx
                    .registered_components
                    .iter()
                    .any(|c| c.component_id == existing.component_id)
                {
                    init_ctx.registered_components.push(existing.clone());
                }
                let mut reg_clone = (*init_ctx.registry).clone();
                reg_clone.register_dynamic_component(
                    existing.component_id,
                    &desc,
                    name_str.to_string(),
                );
                init_ctx.registry = Arc::new(reg_clone);
                crate::registry::CURRENT_REGISTRY.with(|cell| {
                    *cell.borrow_mut() = Some(Arc::clone(&init_ctx.registry));
                });
                return FfiResult::OK;
            } else {
                return FfiResult::INVALID_COMPONENT;
            }
        }

        let drop_trampoline = if let Some(drop_fn) = desc.drop_fn {
            match allocate_drop_trampoline(
                world_id,
                desc.uuid,
                drop_fn,
                Arc::clone(&init_ctx.is_active),
            ) {
                Some(t) => Some(t),
                None => return FfiResult::INTERNAL_ERROR,
            }
        } else {
            None
        };

        // D7 fix: never allow Bevy's EntityCloner to bit-copy foreign-drop-managed components!
        let clone_behavior = if desc.drop_fn.is_some() {
            ComponentCloneBehavior::Ignore
        } else {
            ComponentCloneBehavior::Default
        };

        // SAFETY: layout verified above; drop_trampoline satisfies OwningPtr signature; Table storage used.
        let bevy_desc = unsafe {
            ComponentDescriptor::new_with_layout(
                name_str.to_string(),
                StorageType::Table,
                layout,
                drop_trampoline,
                true,
                true,
                clone_behavior,
                None,
            )
        };
        let comp_id = world.register_component_with_descriptor(bevy_desc);

        if let Some(mut registry) = world.get_resource_mut::<ModRegistry>() {
            registry.register_dynamic_component(comp_id, &desc, name_str.to_string());
            if let Some(reg_comp) = registry.get_by_id(comp_id) {
                init_ctx.registered_components.push(reg_comp.clone());
            }
        }

        let mut reg_clone = (*init_ctx.registry).clone();
        reg_clone.register_dynamic_component(comp_id, &desc, name_str.to_string());
        init_ctx.registry = Arc::new(reg_clone);
        crate::registry::CURRENT_REGISTRY.with(|cell| {
            *cell.borrow_mut() = Some(Arc::clone(&init_ctx.registry));
        });
    }

    FfiResult::OK
}
