//! Dynamic observer execution (`ModObserverState`, `guest_observer_runner`), extended deferred
//! trigger (`cmd_trigger_ex`), and runtime observer (`cmd_observe`) entrypoints.

use core::ffi::c_void;
use core::sync::atomic::Ordering;
use std::sync::Arc;

use bevy_abi_mod_sys::{
    BamCommands, BamObserverDesc, BamTriggerDesc, FfiEntity, FfiResult, read_descriptor,
};
use bevy_ecs::{
    component::Component,
    entity::Entity,
    observer::{Observer, TriggerContext},
    ptr::PtrMut,
    world::{DeferredWorld, World},
};

use crate::{
    ffi::{
        commands::{HostCommands, HostCommandsStorage},
        register::GuestObserverBinding,
    },
    handle_header::{COMMANDS_MAGIC, validate_handle_mut},
    loader::ModId,
    registry::ModRegistry,
    sync_ptr::GuestFnPtr,
};

/// ECS [`Component`] storing the shared [`GuestObserverBinding`] on each spawned guest observer entity.
#[derive(Component)]
pub(crate) struct ModObserverState(pub(crate) Arc<GuestObserverBinding>);

/// Dynamic [`ObserverRunner`](bevy_ecs::observer::ObserverRunner) dispatching Bevy `Event` /
/// `EntityEvent` triggers into a registered guest [`bevy_abi_mod_sys::BamObserverFn`].
///
/// # Safety
/// - `world` must be the [`DeferredWorld`] owning `observer`.
/// - `event_ptr` must point to an initialized value matching the registered event layout.
pub(crate) unsafe fn guest_observer_runner(
    mut world: DeferredWorld,
    observer: Entity,
    _trigger_ctx: &TriggerContext,
    event_ptr: PtrMut,
    _trigger_ptr: PtrMut,
) {
    let Some(binding) = world
        .get::<ModObserverState>(observer)
        .map(|state| Arc::clone(&state.0))
    else {
        return;
    };
    if !binding.is_active.load(Ordering::Acquire) {
        return;
    }

    let target_ffi = if let Some(extract_fn) = binding.extract_target_fn {
        // SAFETY: `event_ptr` holds an initialized `EntityEvent` matching `binding.event_id`.
        match unsafe { extract_fn(event_ptr.as_ptr()) } {
            Some(entity) => FfiEntity::from_bits(entity.to_bits()),
            None => FfiEntity::NULL,
        }
    } else if let Some(watched) = binding.target_entity {
        FfiEntity::from_bits(watched.to_bits())
    } else {
        FfiEntity::NULL
    };

    let mut storage = HostCommandsStorage::new();
    let snap = binding.registry.latest_snapshot_arc();
    let status = {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut host_cmds = HostCommands::new(&mut storage, allocator, &snap);
        let GuestFnPtr(callback) = binding.callback;
        let wire_entity = target_ffi;
        let data_ptr = if binding.synthesizes_ffi_entity_payload {
            (&raw const wire_entity).cast::<c_void>()
        } else if binding.event_size == 0 {
            core::ptr::null()
        } else {
            event_ptr.as_ptr().cast::<c_void>()
        };
        // SAFETY: `binding.is_active` is verified true and `host_cmds` is poisoned before exit.
        unsafe {
            callback(
                target_ffi,
                data_ptr,
                host_cmds.as_ffi_ptr(),
                binding.user_data.as_ptr(),
            )
        }
    };

    if status == FfiResult::OK && !storage.is_empty() {
        world.commands().queue(move |w: &mut World| {
            storage.apply_to_world(w, &snap);
        });
    } else {
        storage.clear();
    }
}

/// Spawns all init-registered [`GuestObserverBinding`]s into `world`, flushing immediately and
/// returning the spawned observer entity IDs so `LoadedMod` can despawn them on unload.
pub(crate) fn spawn_registered_observers_in_world(
    world: &mut World,
    registered_observers: Vec<GuestObserverBinding>,
    registry: &ModRegistry,
    mod_id: ModId,
) -> Vec<Entity> {
    if registered_observers.is_empty() {
        return Vec::new();
    }
    let mut observer_entities = Vec::with_capacity(registered_observers.len());
    for mut obs_binding in registered_observers {
        obs_binding.registry = registry.clone();
        obs_binding.mod_id = Some(mod_id);
        let mut obs = Observer::with_dynamic_runner(guest_observer_runner);
        // SAFETY: `obs_binding.event_key` was resolved/registered in `world` with matching payload layout.
        obs = unsafe { obs.with_event_key(obs_binding.event_key) };
        if let Some(target) = obs_binding.target_entity {
            obs.watch_entity(target);
        }
        let obs_entity = world
            .spawn((obs, ModObserverState(Arc::new(obs_binding))))
            .id();
        observer_entities.push(obs_entity);
    }
    world.flush();
    observer_entities
}

/// Queues an extended deferred trigger described by [`BamTriggerDesc`].
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `desc` must point to a readable [`BamTriggerDesc`], or null.
pub unsafe extern "C" fn host_cmd_trigger_ex(
    commands: *mut BamCommands,
    desc: *const BamTriggerDesc,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(_host_cmds) = (unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    }) else {
        return if commands.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamTriggerDesc::MIN_V0_2_SIZE`.
    let parsed = match unsafe { read_descriptor::<BamTriggerDesc>(desc.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => return err,
    };
    let val = parsed.validate();
    if val.is_err() {
        return val;
    }
    FfiResult::UNSUPPORTED
}

/// Queues deferred spawning of a runtime observer described by [`BamObserverDesc`], optionally
/// writing the reserved observer [`FfiEntity`] into `*out_entity` when non-null.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `desc` must point to a readable [`BamObserverDesc`], or null.
/// - When `out_entity` is non-null, it must point to writable [`FfiEntity`] storage.
pub unsafe extern "C" fn host_cmd_observe(
    commands: *mut BamCommands,
    desc: *const BamObserverDesc,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if !out_entity.is_null() {
        if !out_entity.is_aligned() {
            return FfiResult::INVALID_ARGUMENT;
        }
        // SAFETY: `out_entity` is non-null, aligned, and writable.
        unsafe {
            *out_entity = FfiEntity::NULL;
        }
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(_host_cmds) = (unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    }) else {
        return if commands.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamObserverDesc::MIN_V0_2_SIZE`.
    let parsed = match unsafe { read_descriptor::<BamObserverDesc>(desc.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => return err,
    };
    let val = parsed.validate();
    if val.is_err() {
        return val;
    }
    FfiResult::UNSUPPORTED
}
