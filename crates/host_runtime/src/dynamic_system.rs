#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Dynamic C-ABI System Adapter
//!
//! This module adapts foreign C-ABI function callbacks into native Bevy ECS [`System`]s.
//!
//! ## Core Architectural Invariants
//! 1. **Dynamic `FilteredAccessSet` Borrow Checking**:
//!    - During [`DynamicCSystem::initialize`], foreign query descriptors ([`ComponentAccessDescOwned`])
//!      are parsed into Bevy's native [`FilteredAccess<ComponentId>`].
//!    - Access modes ([`ComponentAccessMode::READ`], [`ComponentAccessMode::WRITE`],
//!      [`ComponentAccessMode::FILTER_ONLY`], [`ComponentAccessMode::WITHOUT`]) are recorded.
//!    - An access set is returned to Bevy via [`SystemAccess::Shared`].
//!    - Bevy's scheduler uses this access set to guarantee data race freedom: systems accessing
//!      disjoint components run concurrently in parallel, while conflicting systems are serialized.
//! 2. **Intra-System Conflict Detection**:
//!    - If a single system specifies mutually conflicting queries (e.g. Query 0 writes Component A
//!      and Query 1 reads or writes Component A), `access_set.is_compatible(&candidate_set)` returns `false`.
//!    - The system detects this self-conflict, flags `self_conflict = true; self.enabled = false;`,
//!      and safely disables itself without causing undefined behavior.
//! 3. **Steady-State Zero Allocation via `ArchetypeGeneration`**:
//!    - Archetypes are scanned and cached in [`CachedQueryGroup`].
//!    - In steady state, `world.archetypes().generation()` matches the cached generation,
//!      so archetype scanning is bypassed and zero heap allocations occur per tick.
//! 4. **Panic Containment & Fault Isolation**:
//!    - The foreign callback invocation is wrapped in [`std::panic::catch_unwind`].
//!    - If a guest mod panics or returns an error status ([`FfiResult::PANIC_CAUGHT`] or `is_err()`),
//!      the system sets `self.enabled = false` and quarantines itself.
//!    - The host Bevy engine and all other healthy systems continue running uninterrupted.
//! 5. **Atomic Tombstone Guard**:
//!    - Checks `self.is_active.load(Ordering::Acquire)` before every execution.
//!    - When a mod is unloaded, this flag becomes `false`, immediately rendering the system an inert no-op.

use crate::chunk_iter::{HostChunkIter, QueryColumnDesc};
use crate::commands::CommandQueueBridge;
use crate::registry::ModRegistry;
use abi_contract::{
    ComponentAccessMode, DynamicSystemCallback, FfiResult, MultiQuerySystemCallback,
    MultiQuerySystemDescriptor, RawChunkIter, RawCommandQueue, SystemDescriptor,
};
use bevy_ecs::archetype::ArchetypeGeneration;
use bevy_ecs::change_detection::{CheckChangeTicks, Tick};
use bevy_ecs::query::{FilteredAccess, FilteredAccessSet};
use bevy_ecs::storage::TableId;
use bevy_ecs::system::{RunSystemError, System, SystemAccess, SystemIn, SystemStateFlags};
use bevy_ecs::utils::DebugName;
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy_ecs::world::{CommandQueue, DeferredWorld, World};
use core::ffi::{c_void, CStr};
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Maximum number of concurrent/nested queries supported per multi-query dynamic system.
pub const MAX_SYSTEM_QUERIES: usize = 8;

/// Owned representation of component access mode and component name for a dynamic query clause.
#[derive(Clone, Debug)]
pub struct ComponentAccessDescOwned {
    /// Canonical component name.
    pub name: String,
    /// Requested component access mode (`READ`, `WRITE`, `FILTER_ONLY`, `WITHOUT`).
    pub mode: ComponentAccessMode,
}

#[derive(Clone, Debug)]
struct CachedQueryGroup {
    descs: Vec<ComponentAccessDescOwned>,
    columns: Vec<QueryColumnDesc>,
    matched_tables: Vec<TableId>,
    matched_table_bits: Vec<bool>,
}

impl CachedQueryGroup {
    fn new(descs: Vec<ComponentAccessDescOwned>) -> Self {
        Self {
            descs,
            columns: Vec::new(),
            matched_tables: Vec::new(),
            matched_table_bits: Vec::new(),
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

    archetype_generation: ArchetypeGeneration,
    access: SystemAccess,
    self_conflict: bool,
    last_run: Tick,
    this_run: Tick,

    callback: SystemCallbackKind,
    user_data: *mut c_void,

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
            archetype_generation: ArchetypeGeneration::initial(),
            access: SystemAccess::None,
            self_conflict: false,
            last_run: Tick::new(0),
            this_run: Tick::new(0),
            callback: SystemCallbackKind::Single(callback),
            user_data,
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
            archetype_generation: ArchetypeGeneration::initial(),
            access: SystemAccess::None,
            self_conflict: false,
            last_run: Tick::new(0),
            this_run: Tick::new(0),
            callback: SystemCallbackKind::Multi(callback),
            user_data,
            enabled: true,
            is_active: Arc::new(AtomicBool::new(true)),
            command_queue: CommandQueue::default(),
            registry,
        }
    }

    /// Returns `true` if the system is currently enabled to execute.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enables or disables execution of this system.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Returns `true` if the system was disabled due to internal query borrow conflicts.
    pub fn has_self_conflict(&self) -> bool {
        self.self_conflict
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

    fn update_archetypes(&mut self, world: UnsafeWorldCell, current_gen: ArchetypeGeneration) {
        self.archetype_generation = current_gen;
        let archetypes = world.archetypes();

        for group in &mut self.query_groups {
            group.matched_tables.clear();
            group.matched_table_bits.clear();

            for archetype in archetypes.iter() {
                let matches = group.columns.iter().all(|col| {
                    if col.mode == ComponentAccessMode::WITHOUT {
                        !archetype.contains(col.component_id)
                    } else {
                        archetype.contains(col.component_id)
                    }
                });
                if matches {
                    let table_id = archetype.table_id();
                    let idx = table_id.as_usize();
                    if idx >= group.matched_table_bits.len() {
                        group.matched_table_bits.resize(idx + 1, false);
                    }
                    if !group.matched_table_bits[idx] {
                        group.matched_table_bits[idx] = true;
                        group.matched_tables.push(table_id);
                    }
                }
            }
        }
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

    fn initialize(&mut self, world: &mut World) -> SystemAccess {
        self.this_run = world.change_tick();
        self.last_run = self.this_run;
        self.self_conflict = false;

        let mut access_set = FilteredAccessSet::new();

        for group in &mut self.query_groups {
            group.columns.clear();
            let mut filtered_access = FilteredAccess::default();

            for desc in &group.descs {
                if let Some(reg) = self.registry.get_by_name(&desc.name) {
                    let comp_id = reg.component_id;
                    let stride = reg.size;

                    group.columns.push(QueryColumnDesc {
                        component_id: comp_id,
                        mode: desc.mode,
                        stride,
                    });

                    match desc.mode {
                        ComponentAccessMode::READ => {
                            filtered_access.add_read(comp_id);
                        }
                        ComponentAccessMode::WRITE => {
                            filtered_access.add_write(comp_id);
                        }
                        ComponentAccessMode::FILTER_ONLY => {
                            filtered_access.and_with(comp_id);
                        }
                        ComponentAccessMode::WITHOUT => {
                            filtered_access.and_without(comp_id);
                        }
                        _ => {}
                    }
                }
            }

            let mut candidate_set = FilteredAccessSet::new();
            candidate_set.add(filtered_access.clone());
            if !access_set.is_compatible(&candidate_set) {
                self.self_conflict = true;
                self.enabled = false;
            }

            access_set.add(filtered_access);
        }

        self.access = SystemAccess::Shared(access_set);

        // Scan initial archetypes
        let cell = world.as_unsafe_world_cell();
        let current_gen = cell.archetypes().generation();
        self.update_archetypes(cell, current_gen);

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

        // Advance change detection ticks
        self.last_run = self.this_run;
        self.this_run = world.change_tick();

        // Archetype caching: zero heap allocations in steady-state
        let current_gen = world.archetypes().generation();
        if self.archetype_generation != current_gen {
            self.update_archetypes(world, current_gen);
        }

        // SAFETY: Scheduler verified access via FilteredAccessSet; no intra-system conflict exists.
        let storages = unsafe { world.storages() };

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
                let mut host_iter = HostChunkIter::new(
                    &storages.tables,
                    &group.matched_tables,
                    &group.columns,
                    self.this_run,
                );
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
                    host_iters[i] = Some(HostChunkIter::new(
                        &storages.tables,
                        &group.matched_tables,
                        &group.columns,
                        self.this_run,
                    ));
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

        match res {
            Ok(status) => {
                if status == FfiResult::PANIC_CAUGHT || status.is_err() {
                    self.enabled = false;
                }
            }
            Err(_) => {
                self.enabled = false;
            }
        }

        Ok(())
    }

    fn apply_deferred(&mut self, world: &mut World) {
        self.command_queue.apply(world);
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
}

impl ModInitContext {
    /// Creates a new initialization context with a fresh active flag.
    pub fn new(registry: Arc<ModRegistry>) -> Self {
        Self {
            registry,
            registered_systems: Vec::new(),
            is_active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Creates an initialization context sharing a specific active flag.
    pub fn with_is_active(registry: Arc<ModRegistry>, is_active: Arc<AtomicBool>) -> Self {
        Self {
            registry,
            registered_systems: Vec::new(),
            is_active,
        }
    }
}

/// C-ABI implementation of `register_system`.
///
/// # Safety
/// `host_context` must point to a valid [`ModInitContext`]. `descriptor` must point to a valid [`SystemDescriptor`].
pub unsafe extern "C" fn host_register_system(
    host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
) -> FfiResult {
    if host_context.is_null() || descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: caller passes a valid pointer to SystemDescriptor.
    let desc = unsafe { &*descriptor };
    if (desc.struct_size as usize) < core::mem::size_of::<SystemDescriptor>() {
        return FfiResult::INTERNAL_ERROR;
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
    .with_is_active(Arc::clone(&init_ctx.is_active));
    init_ctx.registered_systems.push(system);

    FfiResult::OK
}

/// C-ABI implementation of `register_multi_query_system` for nested / multi-query ECS systems.
///
/// # Safety
/// `host_context` must point to a valid [`ModInitContext`]. `descriptor` must point to a valid [`MultiQuerySystemDescriptor`].
pub unsafe extern "C" fn host_register_multi_query_system(
    host_context: *mut c_void,
    descriptor: *const MultiQuerySystemDescriptor,
) -> FfiResult {
    if host_context.is_null() || descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: caller passes a valid pointer to MultiQuerySystemDescriptor.
    let desc = unsafe { &*descriptor };
    if (desc.struct_size as usize) < core::mem::size_of::<MultiQuerySystemDescriptor>() {
        return FfiResult::INTERNAL_ERROR;
    }
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
    .with_is_active(Arc::clone(&init_ctx.is_active));
    init_ctx.registered_systems.push(system);

    FfiResult::OK
}
