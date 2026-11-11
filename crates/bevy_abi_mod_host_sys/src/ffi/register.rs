//! `ModInitContext` validation (`magic` + `generation`) and C-ABI component/system registration.

use crate::drop_trampoline::allocate_drop_trampoline;
use crate::ffi::iter::MAX_QUERY_COMPONENTS;
use crate::loader::ModId;
use crate::registry::{ModRegistry, RegisteredComponent};
use crate::schedule::ModSchedule;
use crate::system::access_builder::validate_registry_independent_groups;
use crate::system::{ComponentAccessDescOwned, DynamicCSystem, MAX_SYSTEM_QUERIES};
use bevy_abi_mod_sys::{
    ConditionalMultiQuerySystemDescriptor, ConditionalSystemDescriptor, FfiResult,
    ModComponentDescriptor, MultiQuerySystemDescriptor, ScheduledMultiQuerySystemDescriptor,
    ScheduledSystemDescriptor, SystemDescriptor, read_descriptor,
};
use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, StorageType};
use bevy_ecs::world::World;
use core::cell::Cell;
use core::ffi::{CStr, c_char, c_void};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// Expected magic tag (`"BAM_INIT"`) at byte offset `0` of an active [`ModInitContext`].
pub const MOD_INIT_CONTEXT_MAGIC: u64 = 0x4241_4D5F_494E_4954;

static NEXT_INIT_GENERATION: AtomicU32 = AtomicU32::new(1);

/// Fixed `#[repr(C)]` validation prefix located at offset `0` of [`ModInitContext`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModInitContextHeader {
    /// Magic sentinel (`MOD_INIT_CONTEXT_MAGIC` while active, zeroed after `init` returns).
    pub magic: u64,
    /// Monotonic generation token matched against the active thread-local registration scope.
    pub generation: u32,
    _pad: u32,
}

/// Host initialization state passed as `host_context` to `bevy_abi_mod_init`.
///
/// Prefixed at byte offset `0` by [`ModInitContextHeader`] so every `host_register_*` entry point
/// verifies active scope, pointer identity, magic tag, and generation before mutating host state.
#[repr(C)]
pub struct ModInitContext {
    /// Validation header pinned at byte offset `0`.
    pub header: ModInitContextHeader,
    /// Active component and symbol registry snapshot.
    pub registry: Arc<ModRegistry>,
    /// Accumulated dynamic systems registered during mod initialization.
    pub registered_systems: Vec<DynamicCSystem>,
    /// Shared atomic liveness flag for this module load attempt.
    pub is_active: Arc<AtomicBool>,
    /// Pointer to the Bevy [`World`] during mod initialization (null if outside a `World` scope).
    pub world: *mut World,
    /// Dynamic components registered during initialization.
    pub registered_components: Vec<RegisteredComponent>,
    /// Optional [`ModId`] assigned to the module currently initializing.
    pub mod_id: Option<ModId>,
}

const _: () = assert!(core::mem::offset_of!(ModInitContext, header) == 0);

impl ModInitContext {
    /// Creates a new initialization context with a fresh active flag.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new(registry: Arc<ModRegistry>) -> Self {
        Self::with_world_and_mod_id(
            registry,
            Arc::new(AtomicBool::new(true)),
            core::ptr::null_mut(),
            None,
        )
    }

    /// Creates an initialization context sharing a specific active flag.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_is_active(registry: Arc<ModRegistry>, is_active: Arc<AtomicBool>) -> Self {
        Self::with_world_and_mod_id(registry, is_active, core::ptr::null_mut(), None)
    }

    /// Creates an initialization context with a specific active flag and mutable `World` access.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_world(
        registry: Arc<ModRegistry>,
        is_active: Arc<AtomicBool>,
        world: *mut World,
    ) -> Self {
        Self::with_world_and_mod_id(registry, is_active, world, None)
    }

    /// Creates an initialization context bound to a `World` and owning [`ModId`].
    pub fn with_world_and_mod_id(
        registry: Arc<ModRegistry>,
        is_active: Arc<AtomicBool>,
        world: *mut World,
        mod_id: Option<ModId>,
    ) -> Self {
        Self {
            header: ModInitContextHeader {
                magic: 0,
                generation: 0,
                _pad: 0,
            },
            registry,
            registered_systems: Vec::new(),
            is_active,
            world,
            registered_components: Vec::new(),
            mod_id,
        }
    }

    /// Marks this context as active on the current thread for the duration of the returned guard.
    pub fn make_active(&mut self) -> ActiveInitContextGuard {
        let mut generation = NEXT_INIT_GENERATION.fetch_add(1, Ordering::Relaxed);
        if generation == 0 {
            generation = NEXT_INIT_GENERATION.fetch_add(1, Ordering::Relaxed);
        }
        self.header = ModInitContextHeader {
            magic: MOD_INIT_CONTEXT_MAGIC,
            generation,
            _pad: 0,
        };
        let nn = NonNull::from(&mut *self);
        let prev = ACTIVE_INIT_CONTEXT.with(|c| c.replace(Some((nn, generation))));
        ActiveInitContextGuard { ctx_ptr: nn, prev }
    }

    /// Corrupts the context header in-place for white-box context validation tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn poison_header_for_testing(&mut self) {
        self.header.magic = 0xDEAD_BEEF_CAFE_BABE;
        self.header.generation = 0;
    }
}

type ActiveContextSlot = Option<(NonNull<ModInitContext>, u32)>;

thread_local! {
    pub(crate) static ACTIVE_INIT_CONTEXT: Cell<ActiveContextSlot> = const { Cell::new(None) };
}

/// RAII guard that invalidates [`ModInitContextHeader`] and restores the previous thread-local
/// active initialization context on drop.
pub struct ActiveInitContextGuard {
    ctx_ptr: NonNull<ModInitContext>,
    prev: ActiveContextSlot,
}

impl Drop for ActiveInitContextGuard {
    fn drop(&mut self) {
        // SAFETY: `ActiveInitContextGuard` is created from `&mut ModInitContext` and dropped before
        // `ModInitContext` leaves scope during `init`, so `self.ctx_ptr` remains valid to poison.
        unsafe {
            let hdr_ptr = core::ptr::addr_of_mut!((*self.ctx_ptr.as_ptr()).header);
            hdr_ptr.write(ModInitContextHeader {
                magic: 0,
                generation: 0,
                _pad: 0,
            });
            if let Some((prev_nn, prev_gen)) = self.prev {
                let prev_hdr = core::ptr::addr_of_mut!((*prev_nn.as_ptr()).header);
                prev_hdr.write(ModInitContextHeader {
                    magic: MOD_INIT_CONTEXT_MAGIC,
                    generation: prev_gen,
                    _pad: 0,
                });
            }
        }
        ACTIVE_INIT_CONTEXT.with(|c| c.set(self.prev));
    }
}

/// Validates that `host_context` matches the currently active `ModInitContext` on this thread and
/// carries an intact `(magic, generation)` header.
///
/// # Safety
/// Safe for any non-null `host_context` pointer because pointer equality against the live
/// thread-local `ACTIVE_INIT_CONTEXT` slot is verified *before* dereferencing `host_context`.
unsafe fn validate_active_init_context<'a>(
    host_context: *mut c_void,
) -> Result<&'a mut ModInitContext, FfiResult> {
    if host_context.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    let Some((active_nn, expected_gen)) = ACTIVE_INIT_CONTEXT.with(|c| c.get()) else {
        return Err(FfiResult::INVALID_STATE);
    };
    if host_context != active_nn.as_ptr().cast::<c_void>() {
        return Err(FfiResult::INVALID_STATE);
    }
    // SAFETY: `host_context` equals `active_nn.as_ptr()`, which points to the live `ModInitContext`
    // pinned on this thread's call stack by `ActiveInitContextGuard`.
    let hdr = unsafe { core::ptr::addr_of!((*active_nn.as_ptr()).header).read() };
    if hdr.magic != MOD_INIT_CONTEXT_MAGIC || hdr.generation != expected_gen || expected_gen == 0 {
        return Err(FfiResult::INVALID_STATE);
    }
    // SAFETY: Verified active pointer match, magic tag, and generation; exclusive access holds
    // inside this single-threaded C-ABI registration call.
    Ok(unsafe { &mut *active_nn.as_ptr() })
}

/// Parses an array of `count` NUL-terminated UTF-8 C strings.
///
/// # Safety
/// If `count > 0` and `ptr` is non-null, `ptr` must reference `count` valid `*const c_char` slots,
/// each either null or pointing to a NUL-terminated C string.
unsafe fn parse_cstr_name_array(
    ptr: *const *const c_char,
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
) -> FfiResult {
    if host_context.is_null() || descriptor_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Verifies active pointer identity, magic, and generation before any dereference.
    let init_ctx = match unsafe { validate_active_init_context(host_context) } {
        Ok(ctx) => ctx,
        Err(err) => return err,
    };

    // SAFETY: Copies `min(struct_size, size_of::<ConditionalSystemDescriptor>())` bytes into a
    // zero-filled `MaybeUninit` and validates non-null `callback` prior to `assume_init()`.
    let cond_desc = match unsafe { read_descriptor::<ConditionalSystemDescriptor>(descriptor_ptr) }
    {
        Ok(d) => d,
        Err(err) => return err,
    };
    let sched_desc = cond_desc.sched;

    if (sched_desc.flags & !ScheduledSystemDescriptor::KNOWN_FLAGS) != 0 {
        return FfiResult::UNSUPPORTED;
    }

    let schedule = match schedule_override {
        Some(s) => s,
        None => {
            let Some(s) = ModSchedule::from_label_id(sched_desc.schedule) else {
                return FfiResult::INVALID_VERSION;
            };
            s
        }
    };

    // SAFETY: `sched_desc.after` is zero-filled (`count == 0`) for v1 descriptors or caller-supplied.
    let after = match unsafe { parse_cstr_name_array(sched_desc.after, sched_desc.after_count) } {
        Ok(v) => v,
        Err(err) => return err,
    };
    // SAFETY: Same contract for `sched_desc.before`.
    let before = match unsafe { parse_cstr_name_array(sched_desc.before, sched_desc.before_count) }
    {
        Ok(v) => v,
        Err(err) => return err,
    };

    let desc = sched_desc.base;
    let run_if = cond_desc.run_if.map(|cb| (cb, cond_desc.run_if_user_data));

    if desc.query_count > MAX_QUERY_COMPONENTS {
        return FfiResult::OUT_OF_RANGE;
    }

    let sys_name = if !desc.name.is_null() {
        // SAFETY: `desc.name` is non-null and points to a NUL-terminated C string.
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
        // SAFETY: `desc.queries` is non-null and points to `desc.query_count` elements.
        let queries_slice = unsafe { core::slice::from_raw_parts(desc.queries, desc.query_count) };
        for q in queries_slice {
            if q.name.is_null() {
                return FfiResult::NULL_POINTER;
            }
            if !q.mode.is_valid() {
                return FfiResult::INVALID_ARGUMENT;
            }
            // SAFETY: `q.name` is non-null and points to a NUL-terminated C string.
            let Ok(q_name) = (unsafe { CStr::from_ptr(q.name) }).to_str() else {
                return FfiResult::INVALID_UTF8;
            };
            query_descs.push(ComponentAccessDescOwned {
                name: q_name.to_string(),
                mode: q.mode,
            });
        }
    }

    if let Err(init_err) = validate_registry_independent_groups(core::slice::from_ref(&query_descs))
    {
        return init_err.to_ffi_result();
    }

    let mut system = DynamicCSystem::new(
        sys_name,
        query_descs,
        desc.callback,
        desc.user_data,
        Arc::clone(&init_ctx.registry),
    )
    .with_schedule(schedule)
    .with_ordering(after, before)
    .with_run_if(run_if)
    .with_is_active(Arc::clone(&init_ctx.is_active));
    if let Some(mod_id) = init_ctx.mod_id {
        system.set_mod_id(mod_id);
    }
    init_ctx.registered_systems.push(system);

    FfiResult::OK
}

/// C-ABI implementation of `register_system`.
///
/// # Safety
/// `host_context` must point to the active [`ModInitContext`] on the calling thread, and
/// `descriptor` must point to a valid [`SystemDescriptor`], [`ScheduledSystemDescriptor`], or
/// [`ConditionalSystemDescriptor`].
pub unsafe extern "C" fn host_register_system(
    host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
) -> FfiResult {
    // SAFETY: Delegating through `validate_active_init_context` and `read_descriptor`.
    unsafe { register_system_from_extensible_ptr(host_context, descriptor.cast::<c_void>(), None) }
}

/// C-ABI implementation of `register_scheduled_system`.
///
/// # Safety
/// `host_context` must point to the active [`ModInitContext`] on the calling thread, and
/// `descriptor` must point to a valid [`ScheduledSystemDescriptor`] or [`ConditionalSystemDescriptor`].
pub unsafe extern "C" fn host_register_scheduled_system(
    host_context: *mut c_void,
    descriptor: *const ScheduledSystemDescriptor,
) -> FfiResult {
    // SAFETY: Delegating through `validate_active_init_context` and `read_descriptor`.
    unsafe { register_system_from_extensible_ptr(host_context, descriptor.cast::<c_void>(), None) }
}

/// C-ABI implementation of `register_conditional_system`.
///
/// # Safety
/// `host_context` must point to the active [`ModInitContext`] on the calling thread, and
/// `descriptor` must point to a valid [`ConditionalSystemDescriptor`].
pub unsafe extern "C" fn host_register_conditional_system(
    host_context: *mut c_void,
    descriptor: *const ConditionalSystemDescriptor,
) -> FfiResult {
    // SAFETY: Delegating through `validate_active_init_context` and `read_descriptor`.
    unsafe { register_system_from_extensible_ptr(host_context, descriptor.cast::<c_void>(), None) }
}

/// C-ABI symbol `register_system_in_schedule` targeting a specific [`ModSchedule`].
///
/// # Safety
/// `host_context` must point to the active [`ModInitContext`] on the calling thread, and
/// `descriptor` must point to a valid [`SystemDescriptor`].
pub unsafe extern "C" fn host_register_system_in_schedule(
    host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
    schedule_raw: u32,
) -> FfiResult {
    if host_context.is_null() || descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Verifies active context before inspecting `schedule_raw`.
    if let Err(err) = unsafe { validate_active_init_context(host_context) } {
        return err;
    }
    let Some(schedule) = ModSchedule::from_raw(schedule_raw) else {
        return FfiResult::INVALID_ARGUMENT;
    };
    // SAFETY: Delegating through `validate_active_init_context` and `read_descriptor`.
    unsafe {
        register_system_from_extensible_ptr(
            host_context,
            descriptor.cast::<c_void>(),
            Some(schedule),
        )
    }
}

unsafe fn register_multi_query_from_extensible_ptr(
    host_context: *mut c_void,
    descriptor_ptr: *const c_void,
) -> FfiResult {
    if host_context.is_null() || descriptor_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Verifies active pointer identity, magic, and generation before any dereference.
    let init_ctx = match unsafe { validate_active_init_context(host_context) } {
        Ok(ctx) => ctx,
        Err(err) => return err,
    };

    // SAFETY: Safely reads `ConditionalMultiQuerySystemDescriptor` via `struct_size`.
    let cond_desc =
        match unsafe { read_descriptor::<ConditionalMultiQuerySystemDescriptor>(descriptor_ptr) } {
            Ok(d) => d,
            Err(err) => return err,
        };
    let sched_desc = cond_desc.sched;

    if (sched_desc.flags & !ScheduledMultiQuerySystemDescriptor::KNOWN_FLAGS) != 0 {
        return FfiResult::UNSUPPORTED;
    }

    let Some(schedule) = ModSchedule::from_label_id(sched_desc.schedule) else {
        return FfiResult::INVALID_VERSION;
    };

    // SAFETY: Zero-filled for v1 descriptors or caller-supplied pointer array for v2/v3 descriptors.
    let after = match unsafe { parse_cstr_name_array(sched_desc.after, sched_desc.after_count) } {
        Ok(v) => v,
        Err(err) => return err,
    };
    // SAFETY: Same contract for `sched_desc.before`.
    let before = match unsafe { parse_cstr_name_array(sched_desc.before, sched_desc.before_count) }
    {
        Ok(v) => v,
        Err(err) => return err,
    };

    let desc = sched_desc.base;
    let run_if = cond_desc.run_if.map(|cb| (cb, cond_desc.run_if_user_data));

    if desc.query_spec_count == 0 || desc.query_spec_count > MAX_SYSTEM_QUERIES {
        return FfiResult::OUT_OF_RANGE;
    }
    if desc.query_specs.is_null() {
        return FfiResult::NULL_POINTER;
    }

    let sys_name = if !desc.name.is_null() {
        // SAFETY: `desc.name` is non-null and points to a NUL-terminated C string.
        unsafe { CStr::from_ptr(desc.name) }
            .to_str()
            .unwrap_or("unnamed_multi_query_guest_system")
            .to_string()
    } else {
        "unnamed_multi_query_guest_system".to_string()
    };

    // SAFETY: `desc.query_specs` points to an array of `desc.query_spec_count` elements.
    let specs_slice =
        unsafe { core::slice::from_raw_parts(desc.query_specs, desc.query_spec_count) };
    let mut parsed_specs = Vec::with_capacity(desc.query_spec_count);

    for spec in specs_slice {
        if spec.query_count > MAX_QUERY_COMPONENTS {
            return FfiResult::OUT_OF_RANGE;
        }
        let mut query_descs = Vec::with_capacity(spec.query_count);
        if spec.query_count > 0 {
            if spec.queries.is_null() {
                return FfiResult::NULL_POINTER;
            }
            // SAFETY: `spec.queries` points to an array of `spec.query_count` elements.
            let queries_slice =
                unsafe { core::slice::from_raw_parts(spec.queries, spec.query_count) };
            for q in queries_slice {
                if q.name.is_null() {
                    return FfiResult::NULL_POINTER;
                }
                if !q.mode.is_valid() {
                    return FfiResult::INVALID_ARGUMENT;
                }
                // SAFETY: `q.name` is non-null and points to a NUL-terminated C string.
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

    if let Err(init_err) = validate_registry_independent_groups(&parsed_specs) {
        return init_err.to_ffi_result();
    }

    let mut system = DynamicCSystem::new_multi_query(
        sys_name,
        parsed_specs,
        desc.callback,
        desc.user_data,
        Arc::clone(&init_ctx.registry),
    )
    .with_schedule(schedule)
    .with_ordering(after, before)
    .with_run_if(run_if)
    .with_is_active(Arc::clone(&init_ctx.is_active));
    if let Some(mod_id) = init_ctx.mod_id {
        system.set_mod_id(mod_id);
    }
    init_ctx.registered_systems.push(system);

    FfiResult::OK
}

/// C-ABI implementation of `register_multi_query_system`.
///
/// # Safety
/// `host_context` must point to the active [`ModInitContext`] on the calling thread, and
/// `descriptor` must point to a valid multi-query system descriptor.
pub unsafe extern "C" fn host_register_multi_query_system(
    host_context: *mut c_void,
    descriptor: *const MultiQuerySystemDescriptor,
) -> FfiResult {
    // SAFETY: Delegating through `validate_active_init_context` and `read_descriptor`.
    unsafe { register_multi_query_from_extensible_ptr(host_context, descriptor.cast::<c_void>()) }
}

/// C-ABI implementation of `register_conditional_multi_query_system`.
///
/// # Safety
/// `host_context` must point to the active [`ModInitContext`] on the calling thread, and
/// `descriptor` must point to a valid [`ConditionalMultiQuerySystemDescriptor`].
pub unsafe extern "C" fn host_register_conditional_multi_query_system(
    host_context: *mut c_void,
    descriptor: *const ConditionalMultiQuerySystemDescriptor,
) -> FfiResult {
    // SAFETY: Delegating through `validate_active_init_context` and `read_descriptor`.
    unsafe { register_multi_query_from_extensible_ptr(host_context, descriptor.cast::<c_void>()) }
}

/// C-ABI implementation of `register_component` for dynamically registering runtime components.
///
/// Validates active [`ModInitContext`] pointer, magic header, generation, non-null `World`,
/// layout alignment/size invariants, and UUID/metadata collisions before registering the component.
///
/// # Safety
/// - `host_context` must point to the active [`ModInitContext`] on the calling thread.
/// - `descriptor` must point to a valid [`ModComponentDescriptor`].
pub unsafe extern "C" fn host_register_component(
    host_context: *mut c_void,
    descriptor: *const ModComponentDescriptor,
) -> FfiResult {
    if host_context.is_null() || descriptor.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Verifies active pointer identity, magic, and generation before any dereference.
    let init_ctx = match unsafe { validate_active_init_context(host_context) } {
        Ok(ctx) => ctx,
        Err(err) => return err,
    };

    // SAFETY: Reads `struct_size` first and copies `min(struct_size, size_of::<ModComponentDescriptor>())`
    // without forming `&ModComponentDescriptor` on truncated C buffers.
    let desc =
        match unsafe { read_descriptor::<ModComponentDescriptor>(descriptor.cast::<c_void>()) } {
            Ok(d) => d,
            Err(err) => return err,
        };
    if desc.name.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `desc.name` is non-null and points to a NUL-terminated C string.
    let Ok(name_str) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
        return FfiResult::INVALID_UTF8;
    };
    if desc.align == 0 || !desc.align.is_power_of_two() {
        return FfiResult::INVALID_ARGUMENT;
    }
    if !desc.size.is_multiple_of(desc.align) {
        return FfiResult::INVALID_ARGUMENT;
    }
    let Some(layout) = desc.layout() else {
        return FfiResult::INVALID_ARGUMENT;
    };

    if init_ctx.world.is_null() {
        return FfiResult::INVALID_STATE;
    }
    // SAFETY: `init_ctx.world` was constructed from an exclusive `&mut World` active during `init`.
    let world = unsafe { &mut *init_ctx.world };
    let world_id = world.id();
    let owner = init_ctx.mod_id;

    if init_ctx
        .registry
        .check_registration_collision(name_str, desc.uuid, desc.size, desc.align, false, None)
        .is_err()
    {
        return FfiResult::INVALID_COMPONENT;
    }
    if let Some(world_reg) = world.get_resource::<ModRegistry>()
        && world_reg
            .check_registration_collision(name_str, desc.uuid, desc.size, desc.align, false, None)
            .is_err()
    {
        return FfiResult::INVALID_COMPONENT;
    }

    if let Some(existing) = world
        .get_resource::<ModRegistry>()
        .and_then(|r| r.get_by_name(name_str))
        .cloned()
    {
        if existing.size == desc.size
            && existing.align == desc.align
            && existing.uuid == desc.uuid
            && !existing.is_resource
        {
            if existing.is_dynamic {
                if let Some(drop_fn) = desc.drop_fn
                    && allocate_drop_trampoline(
                        world_id,
                        desc.uuid,
                        drop_fn,
                        Arc::clone(&init_ctx.is_active),
                    )
                    .is_none()
                {
                    return FfiResult::CAPACITY_EXCEEDED;
                }

                if let Some(mut world_reg) = world.get_resource_mut::<ModRegistry>() {
                    if world_reg
                        .try_register_dynamic_component_owned(
                            existing.component_id,
                            &desc,
                            name_str.to_string(),
                            owner,
                        )
                        .is_err()
                    {
                        return FfiResult::INVALID_COMPONENT;
                    }
                    if let Some(updated) = world_reg.get_by_id(existing.component_id).cloned()
                        && !init_ctx
                            .registered_components
                            .iter()
                            .any(|c| c.component_id == existing.component_id)
                    {
                        init_ctx.registered_components.push(updated);
                    }
                }

                let mut reg_clone = (*init_ctx.registry).clone();
                if reg_clone
                    .try_register_dynamic_component_owned(
                        existing.component_id,
                        &desc,
                        name_str.to_string(),
                        owner,
                    )
                    .is_err()
                {
                    return FfiResult::INVALID_COMPONENT;
                }
                init_ctx.registry = Arc::new(reg_clone);
                crate::registry::symbols::CURRENT_REGISTRY.with(|cell| {
                    *cell.borrow_mut() = Some(Arc::clone(&init_ctx.registry));
                });
            }
            return FfiResult::OK;
        }
        return FfiResult::INVALID_COMPONENT;
    }

    let drop_trampoline = if let Some(drop_fn) = desc.drop_fn {
        let Some(t) = allocate_drop_trampoline(
            world_id,
            desc.uuid,
            drop_fn,
            Arc::clone(&init_ctx.is_active),
        ) else {
            return FfiResult::CAPACITY_EXCEEDED;
        };
        Some(t)
    } else {
        None
    };

    let clone_behavior = if desc.drop_fn.is_some() {
        ComponentCloneBehavior::Ignore
    } else {
        ComponentCloneBehavior::Default
    };

    // SAFETY: `layout` was verified above; `drop_trampoline` matches `OwningPtr` contract; `Table` storage is used.
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
        if registry
            .try_register_dynamic_component_owned(comp_id, &desc, name_str.to_string(), owner)
            .is_err()
        {
            return FfiResult::INVALID_COMPONENT;
        }
        if let Some(reg_comp) = registry.get_by_id(comp_id) {
            init_ctx.registered_components.push(reg_comp.clone());
        }
    }

    let mut reg_clone = (*init_ctx.registry).clone();
    if reg_clone
        .try_register_dynamic_component_owned(comp_id, &desc, name_str.to_string(), owner)
        .is_err()
    {
        return FfiResult::INVALID_COMPONENT;
    }
    init_ctx.registry = Arc::new(reg_clone);
    crate::registry::symbols::CURRENT_REGISTRY.with(|cell| {
        *cell.borrow_mut() = Some(Arc::clone(&init_ctx.registry));
    });

    FfiResult::OK
}
