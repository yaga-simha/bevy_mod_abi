#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use crate::chunk_iter::{HostChunkIter, QueryColumnDesc};
use crate::commands::CommandQueueBridge;
use crate::registry::ModRegistry;
use abi_contract::{
    ComponentAccessMode, DynamicSystemCallback, FfiResult, RawChunkIter, RawCommandQueue,
    SystemDescriptor,
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

#[derive(Clone, Debug)]
pub struct ComponentAccessDescOwned {
    pub name: String,
    pub mode: ComponentAccessMode,
}

/// Host-side Bevy System wrapping an opaque guest C-ABI system callback.
pub struct DynamicCSystem {
    name: String,
    query_descs: Vec<ComponentAccessDescOwned>,
    query_columns: Vec<QueryColumnDesc>,

    archetype_generation: ArchetypeGeneration,
    matched_tables: Vec<TableId>,
    matched_table_bits: Vec<bool>,

    access: SystemAccess,
    last_run: Tick,
    this_run: Tick,

    callback: DynamicSystemCallback,
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
    pub fn new(
        name: String,
        query_descs: Vec<ComponentAccessDescOwned>,
        callback: DynamicSystemCallback,
        user_data: *mut c_void,
        registry: Arc<ModRegistry>,
    ) -> Self {
        Self {
            name,
            query_descs,
            query_columns: Vec::new(),
            archetype_generation: ArchetypeGeneration::initial(),
            matched_tables: Vec::new(),
            matched_table_bits: Vec::new(),
            access: SystemAccess::None,
            last_run: Tick::new(0),
            this_run: Tick::new(0),
            callback,
            user_data,
            enabled: true,
            is_active: Arc::new(AtomicBool::new(true)),
            command_queue: CommandQueue::default(),
            registry,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_active(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.is_active)
    }

    pub fn set_is_active(&mut self, is_active: Arc<AtomicBool>) {
        self.is_active = is_active;
    }

    pub fn with_is_active(mut self, is_active: Arc<AtomicBool>) -> Self {
        self.is_active = is_active;
        self
    }

    fn update_archetypes(&mut self, world: UnsafeWorldCell, current_gen: ArchetypeGeneration) {
        self.archetype_generation = current_gen;
        self.matched_tables.clear();
        self.matched_table_bits.clear();

        let archetypes = world.archetypes();
        for archetype in archetypes.iter() {
            let matches = self
                .query_columns
                .iter()
                .all(|col| archetype.contains(col.component_id));
            if matches {
                let table_id = archetype.table_id();
                let idx = table_id.as_usize();
                if idx >= self.matched_table_bits.len() {
                    self.matched_table_bits.resize(idx + 1, false);
                }
                if !self.matched_table_bits[idx] {
                    self.matched_table_bits[idx] = true;
                    self.matched_tables.push(table_id);
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

        self.query_columns.clear();
        let mut filtered_access = FilteredAccess::default();

        for desc in &self.query_descs {
            if let Some(reg) = self.registry.get_by_name(&desc.name) {
                let comp_id = reg.component_id;
                let stride = reg.size;

                self.query_columns.push(QueryColumnDesc {
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
                    _ => {}
                }
            }
        }

        let mut access_set = FilteredAccessSet::new();
        access_set.add(filtered_access);
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
        if !self.enabled || !self.is_active.load(Ordering::Acquire) {
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

        let storages = unsafe { world.storages() };
        let mut host_iter = HostChunkIter::new(
            &storages.tables,
            &self.matched_tables,
            &self.query_columns,
            self.this_run,
        );
        let iter_ptr = (&mut host_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();

        let mut bridge = CommandQueueBridge::new(
            &mut self.command_queue,
            world.entity_allocator(),
            &self.registry,
        );
        let commands_ptr = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        // Fault-isolated execution
        let callback = self.callback;
        let user_data = self.user_data;
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            callback(iter_ptr, commands_ptr, user_data)
        }));

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
    pub registry: Arc<ModRegistry>,
    pub registered_systems: Vec<DynamicCSystem>,
    pub is_active: Arc<AtomicBool>,
}

impl ModInitContext {
    pub fn new(registry: Arc<ModRegistry>) -> Self {
        Self {
            registry,
            registered_systems: Vec::new(),
            is_active: Arc::new(AtomicBool::new(true)),
        }
    }

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
/// `host_context` must point to a valid `ModInitContext`. `descriptor` must point to a valid `SystemDescriptor`.
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
