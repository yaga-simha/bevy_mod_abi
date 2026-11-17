//! Transactional `LoadContext` (`BamLoadCtx`), `host_register_component`, `host_resolve_component`,
//! and unified `host_register_system`.

use crate::drop_trampoline::{
    StagedDropBinding, StagedHookBinding, WorldDropGuard, on_add_hook_for_slot,
    on_insert_hook_for_slot, on_remove_hook_for_slot, prune_drop_trampolines_for_guard,
};
use crate::handle_header::{
    HOST_CTX_MAGIC, HandleHeader, HostContext, LOAD_CTX_MAGIC, validate_handle_mut,
    validate_handle_ref,
};
use crate::loader::ModId;
use crate::registry::{
    ExtractEventTargetFn, ModRegistry, RegistrySnapshot, component_flags_to_storage_flags,
};
use crate::schedule::ModNamedSystemSet;
use crate::sync_ptr::{GuestFnPtr, GuestUserData};
use crate::system::access_builder::validate_system_queries_against_snapshot;
use crate::system::{BamQuerySpecOwned, GuestSystem};
use bevy_abi_mod_sys::{
    BamComponent, BamHostCtx, BamLoadCtx, BamObserverDesc, BamObserverFn, BamQueryDesc,
    BamSystemDesc, BamTermDesc, ComponentDesc, ComponentUuid, FfiResult, read_descriptor,
};
use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, ComponentId, StorageType};
use bevy_ecs::entity::Entity;
use bevy_ecs::event::EventKey;
use bevy_ecs::world::World;
use core::ffi::{CStr, c_char, c_void};
use core::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Dynamic Bevy ECS observer registration captured during `bevy_abi_mod_init`.
#[derive(Clone)]
pub struct GuestObserverBinding {
    /// Human-readable observer name for diagnostics.
    pub name: Arc<str>,
    /// Pre-resolved 1-based [`BamComponent`] handle of the observed event.
    pub event: BamComponent,
    /// Underlying Bevy [`ComponentId`] corresponding to `event`.
    pub event_id: ComponentId,
    /// Bevy [`EventKey`] dispatched by `world.trigger*` / `world.trigger_dynamic*`.
    pub event_key: EventKey,
    /// Payload size (`size_of::<E>()`).
    pub event_size: usize,
    /// Optional watched target entity (`None` for global observers).
    pub target_entity: Option<Entity>,
    /// Extractor returning the target entity from typed host `EntityEvent` payloads.
    pub extract_target_fn: Option<ExtractEventTargetFn>,
    /// Foreign C-ABI callback (`BamObserverFn`).
    pub callback: GuestFnPtr<BamObserverFn>,
    /// Opaque guest state pointer forwarded to `callback`.
    pub user_data: GuestUserData,
    /// Shared module liveness flag disarmed before `dlclose`.
    pub is_active: Arc<AtomicBool>,
    /// CoW registry handle for snapshot lookup inside commands recorded by the observer.
    pub registry: ModRegistry,
    /// Owning [`ModId`].
    pub mod_id: Option<ModId>,
}

/// Concrete `#[repr(C)]` initialization context backing opaque [`BamLoadCtx`].
///
/// Valid strictly for the duration of `bevy_abi_mod_init`; poisoned automatically on `finish_init`
/// and `Drop` without any thread-local global state.
#[repr(C)]
pub struct LoadContext {
    /// Leading handle validation header (`LOAD_CTX_MAGIC`) at byte offset `0`.
    pub header: HandleHeader,
    /// Embedded world-scoped `HostContext` returned by `host_load_host_ctx`.
    pub host_ctx: HostContext,
    /// Mutable Copy-on-Write [`ModRegistry`] updated as dynamic components register.
    pub registry: ModRegistry,
    /// Accumulated systems registered during `bevy_abi_mod_init`.
    pub registered_systems: Vec<GuestSystem>,
    /// Accumulated dynamic observers registered during `bevy_abi_mod_init`.
    pub registered_observers: Vec<GuestObserverBinding>,
    /// Handles of dynamic components registered during this init session.
    pub registered_components: Vec<BamComponent>,
    /// Shared liveness flag for this module load session.
    pub is_active: Arc<AtomicBool>,
    /// Pointer to the exclusively borrowed Bevy [`World`] (or null in standalone unit tests).
    pub world_ptr: *mut World,
    /// Fallback standalone `World` used only when `world_ptr` is null in isolated FFI unit tests.
    standalone_world: Option<Box<World>>,
    /// Assigned [`ModId`] for this load session.
    pub mod_id: Option<ModId>,
    /// Optional manifest module name used to prefix unqualified guest component names.
    pub manifest_name: Option<Arc<str>>,
    pre_snapshot: Arc<RegistrySnapshot>,
    staged_drop_bindings: Arc<Mutex<Vec<StagedDropBinding>>>,
    staged_hook_bindings: Arc<Mutex<Vec<StagedHookBinding>>>,
    in_init: bool,
}

const _: () = assert!(core::mem::offset_of!(LoadContext, header) == 0);

impl LoadContext {
    /// Creates an active [`LoadContext`] bound to `registry`, `is_active`, `world_ptr`, and `mod_id`.
    pub fn with_world_and_mod_id(
        registry: ModRegistry,
        is_active: Arc<AtomicBool>,
        world_ptr: *mut World,
        mod_id: Option<ModId>,
    ) -> Self {
        Self::with_manifest_name(registry, is_active, world_ptr, mod_id, None)
    }

    /// Creates an active [`LoadContext`] bound to `registry`, `is_active`, `world_ptr`, `mod_id`,
    /// and canonical `manifest_name` (`"<manifest.name>::<Name>"` prefixing for unqualified components).
    pub fn with_manifest_name(
        registry: ModRegistry,
        is_active: Arc<AtomicBool>,
        world_ptr: *mut World,
        mod_id: Option<ModId>,
        manifest_name: Option<Arc<str>>,
    ) -> Self {
        let pre_snapshot = registry.snapshot_arc();
        let host_ctx =
            HostContext::new(Arc::clone(&pre_snapshot)).with_namespace(manifest_name.clone());
        Self {
            header: HandleHeader::new(LOAD_CTX_MAGIC),
            host_ctx,
            registry,
            registered_systems: Vec::new(),
            registered_observers: Vec::new(),
            registered_components: Vec::new(),
            is_active,
            world_ptr,
            standalone_world: None,
            mod_id,
            manifest_name,
            pre_snapshot,
            staged_drop_bindings: Arc::new(Mutex::new(Vec::new())),
            staged_hook_bindings: Arc::new(Mutex::new(Vec::new())),
            in_init: true,
        }
    }

    /// Shares the transaction's [`StagedDropBinding`] and [`StagedHookBinding`] logs so
    /// `LoadTransaction::drop` automatically rolls back every trampoline mutation recorded during `bevy_abi_mod_init`.
    pub(crate) fn with_staged_drop_bindings(
        mut self,
        staged: Arc<Mutex<Vec<StagedDropBinding>>>,
    ) -> Self {
        self.staged_drop_bindings = staged;
        self
    }

    pub(crate) fn with_staged_hook_bindings(
        mut self,
        staged_hooks: Arc<Mutex<Vec<StagedHookBinding>>>,
    ) -> Self {
        self.staged_hook_bindings = staged_hooks;
        self
    }

    /// Creates an active standalone [`LoadContext`] for unit testing without an external `World`.
    pub fn new(registry: ModRegistry) -> Self {
        Self::with_world_and_mod_id(
            registry,
            Arc::new(AtomicBool::new(true)),
            core::ptr::null_mut(),
            None,
        )
    }

    /// Ends the active initialization window, poisoning both `LoadContext` and its embedded
    /// `HostContext` so any stashed FFI pointers fail validation immediately.
    #[inline]
    pub fn finish_init(&mut self) {
        self.in_init = false;
        self.header.poison();
        self.host_ctx.header.poison();
    }

    /// Rolls back all drop trampoline bindings and registry mutations recorded in this `LoadContext`,
    /// returning freshly allocated trampoline slots to `FREE_LIST` and
    /// restoring the initial `RegistrySnapshot`.
    pub fn rollback(&mut self) {
        self.finish_init();
        let bindings = {
            let mut guard = self
                .staged_drop_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        let hook_bindings = {
            let mut guard = self
                .staged_hook_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        if let Some(mut drop_guard) = self.world_mut().get_resource_mut::<WorldDropGuard>() {
            drop_guard.rollback_staged_bindings(&bindings);
            drop_guard.rollback_staged_hook_bindings(&hook_bindings);
        }
        prune_drop_trampolines_for_guard(&self.is_active);
        let pre = Arc::clone(&self.pre_snapshot);
        self.registry.restore_snapshot(Arc::clone(&pre));
        self.host_ctx.set_snapshot(Arc::clone(&pre));
        if !self.world_ptr.is_null()
            && let Some(mut world_reg) = self.world_mut().get_resource_mut::<ModRegistry>()
        {
            world_reg.restore_snapshot(pre);
        }
        self.registered_components.clear();
        self.registered_systems.clear();
    }

    /// Casts `&mut self` to an opaque `*mut BamLoadCtx` FFI pointer.
    #[inline]
    pub fn as_ffi_ptr(&mut self) -> *mut BamLoadCtx {
        (self as *mut Self).cast::<BamLoadCtx>()
    }

    fn world_mut(&mut self) -> &mut World {
        if !self.world_ptr.is_null() {
            // SAFETY: `self.world_ptr` comes from an exclusively borrowed `&mut World` held for the
            // duration of `bevy_abi_mod_init` with no aliasing references.
            unsafe { &mut *self.world_ptr }
        } else {
            self.standalone_world
                .get_or_insert_with(|| Box::new(World::new()))
        }
    }
}

impl Drop for LoadContext {
    #[inline]
    fn drop(&mut self) {
        self.in_init = false;
        self.header.poison();
    }
}

#[inline]
unsafe fn validate_active_load_ctx<'a>(
    load_ctx: *mut BamLoadCtx,
) -> Result<&'a mut LoadContext, FfiResult> {
    // SAFETY: Unconditionally validates non-null, alignment, and `LOAD_CTX_MAGIC` + non-zero generation.
    let ctx = unsafe {
        validate_handle_mut::<LoadContext>(load_ctx.cast::<c_void>(), LOAD_CTX_MAGIC, true)?
    };
    if !ctx.in_init {
        return Err(FfiResult::INVALID_STATE);
    }
    Ok(ctx)
}

/// Obtains the world-scoped [`BamHostCtx`] pointer from an active [`BamLoadCtx`].
///
/// # Safety
/// `load_ctx` must point to an active [`LoadContext`] cast to [`BamLoadCtx`], or null.
pub unsafe extern "C" fn host_load_host_ctx(load_ctx: *const BamLoadCtx) -> *const BamHostCtx {
    // SAFETY: Unconditionally validates `load_ctx` against `LOAD_CTX_MAGIC`.
    let Ok(ctx) = (unsafe {
        validate_handle_ref::<LoadContext>(load_ctx.cast::<c_void>(), LOAD_CTX_MAGIC, true)
    }) else {
        return core::ptr::null();
    };
    if !ctx.in_init {
        return core::ptr::null();
    }
    ctx.host_ctx.as_ffi_ptr()
}

/// Resolves an existing component or resource by canonical `name`, `uuid`, or both.
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - If non-null, `name` must point to a NUL-terminated C string.
/// - If non-null, `uuid` must point to a readable [`ComponentUuid`].
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_resolve_component(
    ctx: *const BamHostCtx,
    name: *const c_char,
    uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    if name.is_null() && uuid.is_null() {
        host_ctx.set_last_error("resolve_component requires at least one of `name` or `uuid`");
        return FfiResult::INVALID_ARGUMENT;
    }

    let parsed_name = if !name.is_null() {
        // SAFETY: Caller guarantees `name` points to a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            host_ctx.set_last_error("resolve_component `name` is not valid UTF-8");
            return FfiResult::INVALID_UTF8;
        };
        if s.is_empty() {
            host_ctx.set_last_error("resolve_component `name` must not be empty");
            return FfiResult::INVALID_ARGUMENT;
        }
        Some(s)
    } else {
        None
    };

    let parsed_uuid = if !uuid.is_null() {
        // SAFETY: Caller guarantees `uuid` points to a readable `ComponentUuid`.
        Some(unsafe { core::ptr::read_unaligned(uuid) })
    } else {
        None
    };

    let snap = host_ctx.snapshot();
    let resolve_by_name = |raw_name: &str| {
        if let Some(found) = snap.get_by_name(raw_name) {
            return Some(found);
        }
        if !raw_name.contains("::")
            && let Some(ns) = host_ctx.default_namespace()
        {
            let qualified = format!("{ns}::{raw_name}");
            return snap.get_by_name(&qualified);
        }
        None
    };

    let entry = match (parsed_name, parsed_uuid) {
        (Some(n), Some(u)) => match resolve_by_name(n) {
            Some(by_name) => {
                if by_name.uuid != u {
                    host_ctx.set_last_error(format!(
                        "resolve_component UUID mismatch for '{n}': expected {:?}, got {u:?}",
                        by_name.uuid
                    ));
                    return FfiResult::TYPE_MISMATCH;
                }
                by_name
            }
            None => {
                if let Some(by_uuid) = snap.get_by_uuid(&u) {
                    host_ctx.set_last_error(format!(
                        "resolve_component UUID {u:?} is bound to '{}', not '{n}'",
                        by_uuid.name
                    ));
                    return FfiResult::TYPE_MISMATCH;
                }
                host_ctx.set_last_error(format!("component or resource '{n}' not found"));
                return FfiResult::NOT_FOUND;
            }
        },
        (Some(n), None) => {
            let Some(by_name) = resolve_by_name(n) else {
                host_ctx.set_last_error(format!("component or resource '{n}' not found"));
                return FfiResult::NOT_FOUND;
            };
            by_name
        }
        (None, Some(u)) => {
            let Some(by_uuid) = snap.get_by_uuid(&u) else {
                host_ctx.set_last_error(format!("component or resource with UUID {u:?} not found"));
                return FfiResult::NOT_FOUND;
            };
            by_uuid
        }
        (None, None) => unreachable!(),
    };

    // SAFETY: `out_component` was checked non-null above and caller guarantees writable `BamComponent`.
    unsafe {
        *out_component = entry.handle;
    }
    FfiResult::OK
}

/// Registers a dynamic component described by `desc_ptr` inside `load_ctx` and writes its 1-based
/// [`BamComponent`] handle into `*out_component`.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`ComponentDesc`], or null.
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_register_component(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };

    // SAFETY: `read_descriptor` checks null and `struct_size >= ComponentDesc::MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<ComponentDesc>(desc_ptr.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid ComponentDesc header ({err})"));
            return err;
        }
    };

    let val_status = desc.validate();
    if val_status.is_err() {
        ctx.host_ctx
            .set_last_error(format!("ComponentDesc validation failed ({val_status})"));
        return val_status;
    }

    let Some(layout) = desc.layout() else {
        ctx.host_ctx
            .set_last_error("ComponentDesc size/align violates C struct layout invariants");
        return FfiResult::INVALID_ARGUMENT;
    };

    // SAFETY: `desc.validate()` verified `!desc.name.is_null()`.
    let Ok(name_str) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
        ctx.host_ctx
            .set_last_error("ComponentDesc `name` is not valid UTF-8");
        return FfiResult::INVALID_UTF8;
    };
    if name_str.is_empty() {
        ctx.host_ctx
            .set_last_error("ComponentDesc `name` must not be empty");
        return FfiResult::INVALID_ARGUMENT;
    }

    // Prefix unqualified dynamic component names with `"<manifest.name>::<Name>"` unless
    // an unqualified entry of that exact name was already registered on the host.
    let canonical_name: std::borrow::Cow<'_, str> =
        if !name_str.contains("::") && ctx.registry.get_by_name(name_str).is_none() {
            if let Some(ref ns) = ctx.manifest_name {
                std::borrow::Cow::Owned(format!("{ns}::{name_str}"))
            } else {
                std::borrow::Cow::Borrowed(name_str)
            }
        } else {
            std::borrow::Cow::Borrowed(name_str)
        };
    let name_ref = canonical_name.as_ref();

    let storage_flags = component_flags_to_storage_flags(desc.flags);
    let existing_comp_id = ctx.registry.get_by_name(name_ref).map(|e| e.component_id);

    if let Err(collision) = ctx.registry.check_registration_collision(
        name_ref,
        desc.uuid,
        desc.size,
        desc.align,
        storage_flags,
        existing_comp_id,
    ) {
        ctx.host_ctx.set_last_error(collision.to_string());
        return FfiResult::TYPE_MISMATCH;
    }
    if let Err(drop_mismatch) = ctx
        .registry
        .check_dynamic_drop_compatibility(name_ref, desc.drop_fn.is_some())
    {
        ctx.host_ctx.set_last_error(drop_mismatch.to_string());
        return FfiResult::TYPE_MISMATCH;
    }
    if let Err(hook_mismatch) = ctx.registry.check_dynamic_hook_compatibility(
        name_ref,
        desc.on_add.is_some(),
        desc.on_remove.is_some(),
    ) {
        ctx.host_ctx.set_last_error(hook_mismatch.to_string());
        return FfiResult::TYPE_MISMATCH;
    }

    let is_active = Arc::clone(&ctx.is_active);
    let owner = ctx.mod_id;
    let staged_log = Arc::clone(&ctx.staged_drop_bindings);
    let staged_hook_log = Arc::clone(&ctx.staged_hook_bindings);

    let (comp_id, drop_slot, hook_slot): (ComponentId, Option<usize>, Option<usize>) = {
        let world = ctx.world_mut();
        if !world.contains_resource::<WorldDropGuard>() {
            world.insert_resource(WorldDropGuard::new());
        }

        let (slot_opt, trampoline_opt) = if let Some(drop_fn) = desc.drop_fn {
            let bound = world
                .get_resource_mut::<WorldDropGuard>()
                .and_then(|mut g| g.bind_or_update(desc.uuid, drop_fn, Arc::clone(&is_active)));
            let Some((slot, tramp, staged_binding)) = bound else {
                ctx.host_ctx.set_last_error(
                    "exhausted MAX_DROP_TRAMPOLINES (1024) foreign destructor slots",
                );
                return FfiResult::CAPACITY_EXCEEDED;
            };
            staged_log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(staged_binding);
            (Some(slot), Some(tramp))
        } else {
            (None, None)
        };

        let hook_slot_opt = if desc.has_hooks() {
            let bound_hook = world
                .get_resource_mut::<WorldDropGuard>()
                .and_then(|mut g| {
                    g.bind_or_update_hooks(
                        desc.uuid,
                        desc.on_add,
                        desc.on_remove,
                        desc.hook_user_data,
                        Arc::clone(&is_active),
                    )
                });
            let Some((hslot, staged_hook)) = bound_hook else {
                ctx.host_ctx
                    .set_last_error("exhausted MAX_DROP_TRAMPOLINES (1024) foreign hook slots");
                return FfiResult::CAPACITY_EXCEEDED;
            };
            staged_hook_log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(staged_hook);
            Some(hslot)
        } else {
            None
        };

        if let Some(existing_id) = existing_comp_id {
            (existing_id, slot_opt, hook_slot_opt)
        } else {
            let storage_type = if desc.is_sparse_set() {
                StorageType::SparseSet
            } else {
                StorageType::Table
            };
            let clone_behavior = if desc.drop_fn.is_some() {
                ComponentCloneBehavior::Ignore
            } else {
                ComponentCloneBehavior::Default
            };
            // SAFETY: `layout` was verified by `desc.layout()`; `trampoline_opt` follows Bevy's
            // `OwningPtr` drop contract; `storage_type` and mutability match `desc.flags`.
            let bevy_desc = unsafe {
                ComponentDescriptor::new_with_layout(
                    name_ref.to_string(),
                    storage_type,
                    layout,
                    trampoline_opt,
                    !desc.is_immutable(),
                    storage_type == StorageType::Table,
                    clone_behavior,
                    None,
                )
            };
            let new_id = world.register_component_with_descriptor(bevy_desc);
            if (slot_opt.is_some() || hook_slot_opt.is_some())
                && let Some(hooks) = world.register_component_hooks_by_id(new_id)
            {
                if let Some(slot) = slot_opt {
                    hooks.on_insert(on_insert_hook_for_slot(slot));
                }
                if let Some(hslot) = hook_slot_opt {
                    if desc.on_add.is_some() {
                        hooks.on_add(on_add_hook_for_slot(hslot));
                    }
                    if desc.on_remove.is_some() {
                        hooks.on_remove(on_remove_hook_for_slot(hslot));
                    }
                }
            }
            (new_id, slot_opt, hook_slot_opt)
        }
    };

    let handle = match ctx
        .registry
        .register_dynamic_component_owned(comp_id, &desc, name_ref, owner, drop_slot, hook_slot)
    {
        Ok(h) => h,
        Err(err) => {
            ctx.host_ctx.set_last_error(err.to_string());
            return FfiResult::TYPE_MISMATCH;
        }
    };

    ctx.host_ctx.set_snapshot(ctx.registry.snapshot_arc());
    if !ctx.world_ptr.is_null() {
        let updated_reg = ctx.registry.clone();
        ctx.world_mut().insert_resource(updated_reg);
    }
    if !ctx.registered_components.contains(&handle) {
        ctx.registered_components.push(handle);
    }

    // SAFETY: `out_component` was checked non-null above and caller guarantees writable `BamComponent`.
    unsafe {
        *out_component = handle;
    }
    FfiResult::OK
}

/// Parses an array of `count` NUL-terminated UTF-8 C strings and validates ordering target syntax.
///
/// # Safety
/// If `count > 0` and `ptr` is non-null, `ptr` must point to `count` readable `*const c_char` slots.
unsafe fn parse_ordering_targets(
    ptr: *const *const c_char,
    count: usize,
) -> Result<Vec<String>, (FfiResult, String)> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err((
            FfiResult::NULL_POINTER,
            "ordering array pointer is null with non-zero count".to_string(),
        ));
    }
    // SAFETY: Caller guarantees `count` readable `*const c_char` elements.
    let slice = unsafe { core::slice::from_raw_parts(ptr, count) };
    let mut out = Vec::with_capacity(count);
    for (idx, &entry) in slice.iter().enumerate() {
        if entry.is_null() {
            return Err((
                FfiResult::NULL_POINTER,
                format!("ordering target at index {idx} is null"),
            ));
        }
        // SAFETY: `entry` is non-null and points to a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(entry) }).to_str() else {
            return Err((
                FfiResult::INVALID_UTF8,
                format!("ordering target at index {idx} is not valid UTF-8"),
            ));
        };
        if ModNamedSystemSet::parse_target(s, "mod").is_none() {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!("invalid ordering target syntax '{s}' at index {idx}"),
            ));
        }
        out.push(s.to_string());
    }
    Ok(out)
}

/// Registers a unified [`BamSystemDesc`] inside `load_ctx`.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamSystemDesc`], or null.
pub unsafe extern "C" fn host_register_system(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const BamSystemDesc,
) -> FfiResult {
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };

    // SAFETY: `read_descriptor` checks null and `struct_size >= BamSystemDesc::MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<BamSystemDesc>(desc_ptr.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid BamSystemDesc header ({err})"));
            return err;
        }
    };

    let val_status = desc.validate();
    if val_status.is_err() {
        ctx.host_ctx
            .set_last_error(format!("BamSystemDesc validation failed ({val_status})"));
        return val_status;
    }

    let Some(callback) = desc.callback else {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `callback` is null");
        return FfiResult::NULL_POINTER;
    };

    // SAFETY: `desc.validate()` verified `!desc.name.is_null()`.
    let Ok(sys_name) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `name` is not valid UTF-8");
        return FfiResult::INVALID_UTF8;
    };
    if sys_name.is_empty() {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `name` must not be empty");
        return FfiResult::INVALID_ARGUMENT;
    }

    // Parse `queries[0..desc.query_count]`.
    let mut query_specs = Vec::with_capacity(desc.query_count);
    if desc.query_count > 0 {
        let base_ptr = desc.queries.cast::<u8>();
        // Determine element stride from the first `BamQueryDesc::struct_size` so forward-extended
        // query arrays step accurately.
        // SAFETY: `desc.validate()` verified `!desc.queries.is_null()` when `query_count > 0`.
        let first_size = unsafe { core::ptr::read_unaligned(base_ptr.cast::<u32>()) } as usize;
        if first_size < BamQueryDesc::MIN_V0_2_SIZE {
            ctx.host_ctx.set_last_error(format!(
                "BamQueryDesc[0] struct_size ({first_size}) < MIN_V0_2_SIZE"
            ));
            return FfiResult::INVALID_VERSION;
        }

        for q_idx in 0..desc.query_count {
            let Some(byte_offset) = q_idx.checked_mul(first_size) else {
                return FfiResult::INVALID_ARGUMENT;
            };
            // SAFETY: Caller guarantees `desc.queries` holds `desc.query_count` contiguous elements.
            let q_ptr = unsafe { base_ptr.add(byte_offset).cast::<c_void>() };
            // SAFETY: Reading extensible `BamQueryDesc` element `q_idx`.
            let q_desc = match unsafe { read_descriptor::<BamQueryDesc>(q_ptr) } {
                Ok(q) => q,
                Err(err) => {
                    ctx.host_ctx
                        .set_last_error(format!("invalid BamQueryDesc[{q_idx}] ({err})"));
                    return err;
                }
            };
            let q_val = q_desc.validate();
            if q_val.is_err() {
                ctx.host_ctx
                    .set_last_error(format!("BamQueryDesc[{q_idx}] validation failed ({q_val})"));
                return q_val;
            }

            let mut terms = Vec::with_capacity(q_desc.term_count);
            for t_idx in 0..q_desc.term_count {
                // SAFETY: `q_desc.validate()` verified `!q_desc.terms.is_null()` when `term_count > 0`.
                let term: BamTermDesc =
                    unsafe { core::ptr::read_unaligned(q_desc.terms.add(t_idx)) };
                terms.push(term);
            }
            query_specs.push(BamQuerySpecOwned {
                flags: q_desc.flags,
                terms,
            });
        }
    }

    if let Err((status, msg)) =
        validate_system_queries_against_snapshot(ctx.registry.snapshot(), &query_specs)
    {
        ctx.host_ctx.set_last_error(msg);
        return status;
    }

    // SAFETY: `desc.validate()` verified `after` and `before` non-null when their counts > 0.
    let after_systems = match unsafe { parse_ordering_targets(desc.after, desc.after_count) } {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return status;
        }
    };
    // SAFETY: Same guarantee for `desc.before[0..desc.before_count]`.
    let before_systems = match unsafe { parse_ordering_targets(desc.before, desc.before_count) } {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return status;
        }
    };
    // SAFETY: `desc.validate()` verified `in_sets` non-null when `in_set_count > 0`.
    let in_sets = match unsafe { parse_ordering_targets(desc.in_sets, desc.in_set_count) } {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return status;
        }
    };

    let run_if = desc.run_if.map(|cb| (cb, desc.run_if_user_data));

    let mut sys = GuestSystem::new(
        sys_name,
        query_specs,
        callback,
        desc.user_data,
        ctx.registry.clone(),
    )
    .with_schedule(desc.schedule)
    .with_main_thread(desc.is_main_thread())
    .with_ordering(after_systems, before_systems)
    .with_in_sets(in_sets)
    .with_run_if(run_if)
    .with_is_active(Arc::clone(&ctx.is_active));

    if let Some(id) = ctx.mod_id {
        sys.set_mod_id(id);
    }

    ctx.registered_systems.push(sys);
    FfiResult::OK
}

/// Registers a dynamic Bevy ECS [`Observer`](bevy_ecs::observer::Observer) from a [`BamObserverDesc`]
/// during `bevy_abi_mod_init`.
///
/// # Safety
/// - `load_ctx` must point to an active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc` must point to a valid aligned [`BamObserverDesc`] whose `name` (if non-null) is NUL-terminated UTF-8.
pub unsafe extern "C" fn host_register_observer(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamObserverDesc,
) -> FfiResult {
    // SAFETY: Validating `load_ctx` against `LOAD_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let ctx = match unsafe {
        validate_handle_mut::<LoadContext>(load_ctx.cast::<c_void>(), LOAD_CTX_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    if !ctx.in_init {
        ctx.host_ctx
            .set_last_error("host_register_observer called outside bevy_abi_mod_init");
        return FfiResult::INVALID_STATE;
    }
    ctx.host_ctx.clear_last_error();

    if desc.is_null() {
        ctx.host_ctx
            .set_last_error("host_register_observer received null BamObserverDesc pointer");
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: `desc` is non-null; `read_descriptor` checks alignment and `struct_size >= MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<BamObserverDesc>(desc.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid BamObserverDesc descriptor ({err})"));
            return err;
        }
    };

    let validation = desc.validate();
    if validation.is_err() {
        ctx.host_ctx
            .set_last_error(format!("BamObserverDesc validation failed ({validation})"));
        return validation;
    }

    let obs_name: Arc<str> = if desc.name.is_null() {
        Arc::from("<unnamed_observer>")
    } else {
        // SAFETY: `desc.name` was verified non-null and points to NUL-terminated UTF-8.
        let Ok(s) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
            ctx.host_ctx
                .set_last_error("BamObserverDesc `name` is not valid UTF-8");
            return FfiResult::INVALID_UTF8;
        };
        if s.is_empty() {
            ctx.host_ctx
                .set_last_error("BamObserverDesc `name` must not be empty");
            return FfiResult::INVALID_ARGUMENT;
        }
        Arc::from(s)
    };

    let Some(callback) = desc.callback else {
        ctx.host_ctx
            .set_last_error("BamObserverDesc `callback` must not be null");
        return FfiResult::NULL_POINTER;
    };

    let Some(meta) = ctx.registry.get_by_handle(desc.event) else {
        ctx.host_ctx
            .set_last_error("BamObserverDesc `event` handle is not registered");
        return FfiResult::INVALID_COMPONENT;
    };
    if meta.is_resource() || meta.is_message() {
        ctx.host_ctx.set_last_error(
            "BamObserverDesc `event` must refer to an Event, EntityEvent, or component, not a Resource or Message",
        );
        return FfiResult::INVALID_COMPONENT;
    }
    let event_key = meta.event_key.unwrap_or_else(|| {
        // SAFETY: `meta.component_id` is valid in `world` and matches `meta`'s registered layout.
        unsafe { EventKey::new(meta.component_id) }
    });

    let raw_target = desc.entity();
    let target_entity = if raw_target.is_null() {
        None
    } else {
        let Some(entity) = Entity::try_from_bits(raw_target.to_bits()) else {
            ctx.host_ctx
                .set_last_error("BamObserverDesc target entity is invalid");
            return FfiResult::INVALID_ENTITY;
        };
        if entity == Entity::PLACEHOLDER {
            ctx.host_ctx
                .set_last_error("BamObserverDesc target entity is Entity::PLACEHOLDER");
            return FfiResult::INVALID_ENTITY;
        }
        Some(entity)
    };

    ctx.registered_observers.push(GuestObserverBinding {
        name: obs_name,
        event: desc.event,
        event_id: meta.component_id,
        event_key,
        event_size: meta.size,
        target_entity,
        extract_target_fn: meta.extract_event_target_fn,
        callback: GuestFnPtr(callback),
        user_data: GuestUserData::new(desc.user_data),
        is_active: Arc::clone(&ctx.is_active),
        registry: ctx.registry.clone(),
        mod_id: ctx.mod_id,
    });

    FfiResult::OK
}
