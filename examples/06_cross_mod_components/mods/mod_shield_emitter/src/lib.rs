//! Mod A: Shield Emitter Mod (`mod_shield_emitter`)
//!
//! Demonstrates:
//! 1. Introducing a brand-new component type (`EnergyShield`) at runtime via `register_runtime_component`.
//!    Neither the host game engine nor any other mod knows about this component at host compile-time.
//! 2. Spawning new autonomous entities and attaching `EnergyShield` dynamically using deferred commands
//!    (`GuestCommandQueue::spawn_empty` + `insert_runtime_component`).
//! 3. Running an active recharge system that modifies `EnergyShield` each tick.

#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use guest_sdk::prelude::*;

/// Runtime structured data introduced dynamically by Mod A.
/// Layout: 12 bytes, 4-byte aligned (POD).
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct EnergyShield {
    /// Current shield charge (MW).
    pub current: f32,
    /// Maximum shield capacity (MW).
    pub max: f32,
    /// Harmonic modulation frequency (MHz).
    pub frequency_mhz: u32,
}

static HOST_API: AtomicPtr<HostApi> = AtomicPtr::new(core::ptr::null_mut());
static SPAWNED_FLAG: AtomicUsize = AtomicUsize::new(0);

struct SpawnerContext {
    cmd_vtable: CommandBridgeVTable,
}

/// System 1: Autonomous Shield Drone Spawner
///
/// Executes once on startup, creating 3 autonomous drone entities possessing `EnergyShield`.
unsafe fn shield_spawner_system(
    _iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    // Only spawn once on tick 0
    if SPAWNED_FLAG.fetch_add(1, Ordering::SeqCst) != 0 {
        return FfiResult::OK;
    }

    if commands.is_null() || user_data.is_null() {
        return FfiResult::NULL_POINTER;
    }

    let ctx = unsafe { &*(user_data as *const SpawnerContext) };
    let mut queue = GuestCommandQueue::new(commands, ctx.cmd_vtable);

    // Initial shield configurations for the 3 drones
    let drone_specs = [
        EnergyShield {
            current: 100.0,
            max: 100.0,
            frequency_mhz: 142,
        },
        EnergyShield {
            current: 80.0,
            max: 100.0,
            frequency_mhz: 242,
        },
        EnergyShield {
            current: 60.0,
            max: 100.0,
            frequency_mhz: 342,
        },
    ];

    for (i, spec) in drone_specs.iter().enumerate() {
        let entity = queue.spawn_empty();
        if entity.0 == 0 || entity == FfiEntity::PLACEHOLDER {
            return FfiResult::INTERNAL_ERROR;
        }

        let res = queue.insert_runtime_component(entity, c"EnergyShield", spec);
        if res.is_err() {
            return res;
        }

        // Host/stdout telemetry for the example
        println!(
            "  [Mod A::spawner] Spawned Drone #{} (FfiEntity {:?}) with EnergyShield({:.1}/{:.1} MW, {} MHz)",
            i + 1,
            entity,
            spec.current,
            spec.max,
            spec.frequency_mhz
        );
    }

    FfiResult::OK
}

/// System 2: Shield Generator & Recharge Loop
///
/// Queries `EnergyShield` (WRITE access) across matching archetype tables.
/// Slowly recharges shields by +5.0 MW each tick up to maximum capacity and modulates frequency.
unsafe fn shield_recharge_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let api_ptr = HOST_API.load(Ordering::Acquire);
    if api_ptr.is_null() || iter.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let api = unsafe { &*api_ptr };
    let mut chunk_iter = GuestChunkIter::new(iter, api);

    loop {
        let count = chunk_iter.next_chunk();
        if count == 0 {
            break;
        }

        let mut slice = chunk_iter.mutable_slice(0, count);
        let Some(shields) = slice.as_typed_slice::<EnergyShield>() else {
            return FfiResult::INTERNAL_ERROR;
        };

        for shield in shields.iter_mut() {
            let before = shield.current;
            shield.current = (shield.current + 5.0).min(shield.max);
            shield.frequency_mhz = shield.frequency_mhz.wrapping_add(1);
            let delta = shield.current - before;

            if delta > 0.0 {
                println!(
                    "  [Mod A::recharge] Recharged drone shield: {:.1} -> {:.1} MW (+{:.1} MW)",
                    before, shield.current, delta
                );
            }
        }
    }

    FfiResult::OK
}

wrap_system_callback!(fn c_shield_spawner calls shield_spawner_system);
wrap_system_callback!(fn c_shield_recharge calls shield_recharge_system);

fn mod_init(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult {
    if host_api.is_null() || host_context.is_null() {
        return FfiResult::NULL_POINTER;
    }
    HOST_API.store(host_api as *mut HostApi, Ordering::Release);
    SPAWNED_FLAG.store(0, Ordering::SeqCst);

    // 1. Introduce & register runtime component: `EnergyShield`
    // Size = 12, Align = 4, POD (no drop glue).
    let reg_res = unsafe {
        register_runtime_component::<EnergyShield>(host_api, host_context, c"EnergyShield")
    };
    if reg_res.is_err() {
        return reg_res;
    }

    let api = unsafe { &*host_api };

    // 2. Register System 1: Spawner (queries 0 components, uses RawCommandQueue)
    let spawner_ctx = Box::into_raw(Box::new(SpawnerContext {
        cmd_vtable: CommandBridgeVTable::resolve(get_proc_address),
    }));

    let spawner_desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"shield_spawner_system".as_ptr(),
        queries: core::ptr::null(),
        query_count: 0,
        callback: c_shield_spawner,
        user_data: spawner_ctx as *mut c_void,
    };
    let s1_res = unsafe { (api.register_system)(host_context, &spawner_desc) };
    if s1_res.is_err() {
        return s1_res;
    }

    // 3. Register System 2: Recharger (queries `EnergyShield` with WRITE access)
    let recharge_queries = [ComponentAccessDesc {
        name: c"EnergyShield".as_ptr(),
        mode: ComponentAccessMode::WRITE,
    }];
    let recharge_desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"shield_recharge_system".as_ptr(),
        queries: recharge_queries.as_ptr(),
        query_count: recharge_queries.len(),
        callback: c_shield_recharge,
        user_data: core::ptr::null_mut(),
    };
    unsafe { (api.register_system)(host_context, &recharge_desc) }
}

fn mod_shutdown(_host_context: *mut c_void) -> FfiResult {
    HOST_API.store(core::ptr::null_mut(), Ordering::Release);
    FfiResult::OK
}

export_mod!(mod_init, mod_shutdown);
