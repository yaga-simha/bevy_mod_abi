//! `DynamicCSystem` implementation of Bevy's `System` trait over C-ABI callbacks.

use crate::ffi::commands::CommandQueueBridge;
use crate::ffi::iter::HostChunkIter;
use crate::loader::ModId;
use crate::registry::ModRegistry;
use crate::schedule::{ModSchedule, ModSystemSet};
use crate::sync_ptr::{GuestFnPtr, GuestUserData};
use crate::system::access_builder::initialize_query_groups_in_world;
use crate::system::error_policy::{
    ModErrorPolicy, ModErrorPolicyConfig, ModSystemError, ModSystemFaulted, SystemInitError,
};
use crate::system::query_group::{CachedQueryGroup, ComponentAccessDescOwned, MAX_SYSTEM_QUERIES};
use bevy_abi_mod_sys::{
    DynamicSystemCallback, FfiResult, MultiQuerySystemCallback, RawChunkIter, RawCommandQueue,
    RunConditionCallback, ffi_bool_to_rust,
};
use bevy_ecs::change_detection::{CheckChangeTicks, Tick};
use bevy_ecs::message::Messages;
use bevy_ecs::schedule::{InternedSystemSet, SystemSet};
use bevy_ecs::system::{RunSystemError, System, SystemAccess, SystemIn, SystemStateFlags};
use bevy_ecs::utils::DebugName;
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy_ecs::world::{CommandQueue, DeferredWorld, World};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum SystemCallbackKind {
    Single(GuestFnPtr<DynamicSystemCallback>),
    Multi(GuestFnPtr<MultiQuerySystemCallback>),
}

/// Host-side Bevy [`System`] wrapping an opaque guest C-ABI system callback (single or multi-query).
pub struct DynamicCSystem {
    name: String,
    debug_name: DebugName,
    query_groups: Vec<CachedQueryGroup>,

    access: SystemAccess,
    self_conflict: bool,
    missing_components: Vec<String>,
    init_error: Option<SystemInitError>,
    last_run: Tick,
    this_run: Tick,

    callback: SystemCallbackKind,
    user_data: GuestUserData,
    run_if: Option<(GuestFnPtr<RunConditionCallback>, GuestUserData)>,

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

impl DynamicCSystem {
    /// Creates a single-query dynamic C-ABI system.
    pub fn new(
        name: String,
        query_descs: Vec<ComponentAccessDescOwned>,
        callback: DynamicSystemCallback,
        user_data: *mut c_void,
        registry: Arc<ModRegistry>,
    ) -> Self {
        let debug_name = DebugName::owned(name.clone());
        Self {
            name,
            debug_name,
            query_groups: vec![CachedQueryGroup::new(query_descs)],
            access: SystemAccess::None,
            self_conflict: false,
            missing_components: Vec::new(),
            init_error: None,
            last_run: Tick::new(0),
            this_run: Tick::new(0),
            callback: SystemCallbackKind::Single(GuestFnPtr(callback)),
            user_data: GuestUserData::new(user_data),
            run_if: None,
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
        let debug_name = DebugName::owned(name.clone());
        let query_groups = query_specs.into_iter().map(CachedQueryGroup::new).collect();
        Self {
            name,
            debug_name,
            query_groups,
            access: SystemAccess::None,
            self_conflict: false,
            missing_components: Vec::new(),
            init_error: None,
            last_run: Tick::new(0),
            this_run: Tick::new(0),
            callback: SystemCallbackKind::Multi(GuestFnPtr(callback)),
            user_data: GuestUserData::new(user_data),
            run_if: None,
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

    /// Returns the structured [`SystemInitError`] recorded if `System::initialize` failed.
    pub fn init_error(&self) -> Option<&SystemInitError> {
        self.init_error.as_ref()
    }

    /// Returns any component names that failed to resolve during `initialize` or validation.
    pub fn missing_components(&self) -> &[String] {
        &self.missing_components
    }

    /// Validates that all queried component names exist in the given [`ModRegistry`].
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

    /// Configures an optional C-ABI run condition callback evaluated before each system run.
    pub fn set_run_if(&mut self, run_if: Option<(RunConditionCallback, *mut c_void)>) {
        self.run_if = run_if.map(|(cb, ud)| (GuestFnPtr(cb), GuestUserData::new(ud)));
    }

    /// Fluent builder method to attach an optional C-ABI run condition callback.
    pub fn with_run_if(mut self, run_if: Option<(RunConditionCallback, *mut c_void)>) -> Self {
        self.set_run_if(run_if);
        self
    }

    /// Returns the configured run condition callback and context pointer, if any.
    pub fn run_if(&self) -> Option<(RunConditionCallback, *mut c_void)> {
        self.run_if.map(|(GuestFnPtr(cb), ud)| (cb, ud.as_ptr()))
    }

    fn record_fault_and_apply_policy(
        &mut self,
        status: FfiResult,
        panicked: bool,
    ) -> Result<(), RunSystemError> {
        self.enabled = false;
        let fault_event = ModSystemFaulted {
            mod_id: self.mod_id,
            system_name: self.name.clone(),
            status,
            panicked,
            init_error: None,
        };
        self.command_queue.push(move |world: &mut World| {
            if let Some(mut msgs) = world.get_resource_mut::<Messages<ModSystemFaulted>>() {
                msgs.write(fault_event.clone());
            }
            world.trigger(fault_event);
        });

        match self.error_policy {
            ModErrorPolicy::Quarantine => Ok(()),
            ModErrorPolicy::Log => {
                eprintln!(
                    "[bevy_abi_mod] guest system '{}' (mod_id={:?}) faulted: status={:?}, panicked={}",
                    self.name, self.mod_id, status, panicked
                );
                Ok(())
            }
            ModErrorPolicy::ReportToBevy => Err(RunSystemError::from(ModSystemError {
                mod_id: self.mod_id,
                system_name: self.name.clone(),
                status,
                panicked,
            })),
        }
    }
}

impl System for DynamicCSystem {
    type In = ();
    type Out = ();

    fn name(&self) -> DebugName {
        self.debug_name.clone()
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
        self.this_run = world.change_tick();
        self.last_run = Tick::new(world.change_tick().get().wrapping_sub(Tick::MAX.get()));
        self.self_conflict = false;
        self.missing_components.clear();
        self.init_error = None;

        if !self.error_policy_explicit
            && let Some(cfg) = world.get_resource::<ModErrorPolicyConfig>()
        {
            self.error_policy = cfg.0;
        }

        if let Some(world_reg) = world.get_resource::<ModRegistry>() {
            self.registry = Arc::new(world_reg.clone());
        }

        match initialize_query_groups_in_world(world, &self.registry, &mut self.query_groups) {
            Ok(access_set) => {
                self.access = SystemAccess::Shared(access_set);
                let cell = world.as_unsafe_world_cell();
                for group in &mut self.query_groups {
                    group.update_archetypes(cell);
                }
                self.access.clone()
            }
            Err(err) => {
                self.enabled = false;
                self.access = SystemAccess::None;
                if matches!(err, SystemInitError::SelfConflict { .. }) {
                    self.self_conflict = true;
                }
                if let SystemInitError::UnknownComponent(ref missing_name) = err {
                    self.missing_components.push(missing_name.clone());
                }
                let status = err.to_ffi_result();
                self.init_error = Some(err.clone());

                let fault_event = ModSystemFaulted {
                    mod_id: self.mod_id,
                    system_name: self.name.clone(),
                    status,
                    panicked: false,
                    init_error: Some(err),
                };
                if let Some(mut msgs) = world.get_resource_mut::<Messages<ModSystemFaulted>>() {
                    msgs.write(fault_event.clone());
                }
                world.trigger(fault_event);

                self.access.clone()
            }
        }
    }

    unsafe fn run_unsafe(
        &mut self,
        _input: SystemIn<'_, Self>,
        world: UnsafeWorldCell,
    ) -> Result<Self::Out, RunSystemError> {
        if !self.enabled || self.self_conflict || !self.is_active.load(Ordering::Acquire) {
            return Ok(());
        }

        let _reg_guard = self.registry.make_current_internal();

        if let Some((GuestFnPtr(cond_fn), cond_ud)) = self.run_if {
            // SAFETY: `cond_fn` is a foreign `extern "C"` predicate supplied by an active loaded mod.
            let flag = unsafe { cond_fn(cond_ud.as_ptr()) };
            if !ffi_bool_to_rust(flag) {
                return Ok(());
            }
        }

        let change_tick = world.increment_change_tick();
        self.this_run = change_tick;
        let last_run = self.last_run;

        for group in &mut self.query_groups {
            group.update_archetypes(world);
        }

        // SAFETY: Scheduler verified access via `FilteredAccessSet`; no intra-system conflict exists.
        let storages = unsafe { world.storages() };
        let archetypes = world.archetypes();

        let mut bridge = CommandQueueBridge::new(
            &mut self.command_queue,
            world.entity_allocator(),
            &self.registry,
        );
        let commands_ptr = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
        let user_data = self.user_data.as_ptr();
        let registry_ref: &ModRegistry = &self.registry;

        let status = match self.callback {
            SystemCallbackKind::Single(GuestFnPtr(callback)) => {
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
                }
                .with_last_run(last_run)
                .with_resources(world, registry_ref, &group.declared_resources);
                let iter_ptr = (&mut host_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();
                // SAFETY: Calling foreign `extern "C"` callback with valid stack-allocated iterator and command bridge.
                unsafe { callback(iter_ptr, commands_ptr, user_data) }
            }
            SystemCallbackKind::Multi(GuestFnPtr(callback)) => {
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
                    }
                    .with_last_run(last_run)
                    .with_resources(
                        world,
                        registry_ref,
                        &group.declared_resources,
                    );
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
                unsafe { callback(raw_ptrs.as_ptr(), count, commands_ptr, user_data) }
            }
        };

        self.last_run = change_tick;

        if status == FfiResult::PANIC_CAUGHT || status.is_err() {
            return self.record_fault_and_apply_policy(status, status == FfiResult::PANIC_CAUGHT);
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
