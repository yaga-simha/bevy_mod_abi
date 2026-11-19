#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

//! Fuzz Target 3: `host_cmd_*` FFI entry points (`spawn_empty`, `spawn`, `despawn`,
//! `insert`, `insert_default`, `remove`) with arbitrary component handles, forged/placeholder/null
//! entities, mismatched sizes, padded byte payloads, duplicate batch IDs, type-confused handles,
//! and alternating commit (`apply_to_world`) vs. fault rollback (`clear`).

use bevy_abi_mod_host::{ComponentOptions, ModComponent, ModRegistry, ModWorldExt};
use bevy_abi_mod_host_sys::{
    HostCommands, HostCommandsStorage, LoadContext, host_cmd_despawn, host_cmd_insert,
    host_cmd_insert_default, host_cmd_remove, host_cmd_spawn, host_cmd_spawn_empty,
    host_register_component,
};
use bevy_abi_mod_sys::{
    BamCommands, BamComponent, ComponentDesc, ComponentUuid, FfiEntity, FfiResult,
};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::resource::Resource;
use bevy_ecs::world::World;
use core::sync::atomic::AtomicBool;
use libfuzzer_sys::fuzz_target;
use std::hint::black_box;
use std::sync::Arc;

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModComponent)]
#[mod_component(name = "fuzz_cmd::PaddedPod", no_pod)]
struct PaddedPod {
    tag: u8,
    value: u32,
}

#[derive(Component, Default, ModComponent)]
#[mod_component(name = "fuzz_cmd::OpaqueHeap")]
struct OpaqueHeap {
    #[allow(dead_code)]
    label: String,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Default, ModComponent)]
#[mod_component(name = "fuzz_cmd::SingletonRes")]
struct SingletonRes {
    ticks: u64,
}

unsafe extern "C" fn noop_foreign_drop(_ptr: *mut core::ffi::c_void) {}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    let mut world = World::new();
    world.init_resource::<SingletonRes>();
    let _ = world.register_mod_component::<PaddedPod>(
        ComponentOptions::new().with_raw_insert().with_default(),
    );
    let _ = world.register_mod_component::<OpaqueHeap>(ComponentOptions::new().with_default());
    let _ = world.register_mod_resource::<SingletonRes>();

    let base_reg = world.resource::<ModRegistry>().clone();
    let mut load_ctx = LoadContext::with_world_and_mod_id(
        base_reg,
        Arc::new(AtomicBool::new(true)),
        &mut world as *mut World,
        None,
    );
    let mut dyn_desc = ComponentDesc::new(
        c"fuzz_cmd::DynDrop".as_ptr(),
        ComponentUuid::from_parts("fuzz_cmd::DynDrop", 8, 4, 99),
        8,
        4,
    );
    dyn_desc.drop_fn = Some(noop_foreign_drop);
    let mut dyn_h = BamComponent::INVALID;
    // SAFETY: Active `load_ctx`, valid `dyn_desc` and `dyn_h`.
    let _ = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &dyn_desc, &mut dyn_h) };
    let registry = load_ctx.registry.clone();
    load_ctx.finish_init();

    let live_e = world.spawn(PaddedPod::default()).id();
    let mut known_entities = vec![
        FfiEntity::NULL,
        FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()),
        FfiEntity::from_bits(live_e.to_bits()),
        FfiEntity::from_bits(0xDEAD_BEEF_0000_0001),
    ];

    let snapshot = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();

    // Test passing a wrong handle header (`LoadContext` cast to `BamCommands*`) under `strict-ffi-checks`.
    let bogus_cmds_ptr = (&mut load_ctx as *mut LoadContext).cast::<BamCommands>();
    // SAFETY: `strict-ffi-checks` validates `COMMANDS_MAGIC` before dereferencing fields.
    unsafe {
        let mut dummy = FfiEntity::NULL;
        assert_ne!(
            host_cmd_spawn_empty(bogus_cmds_ptr, &mut dummy),
            FfiResult::OK
        );
    }

    {
        let allocator = world.entity_allocator();
        let mut cmds = HostCommands::new(&mut storage, allocator, &snapshot);
        let cmds_ptr = cmds.as_ffi_ptr();

        let mut i = 0usize;
        let mut ops = 0usize;
        while i < data.len() && ops < 64 {
            let op = data[i] % 7;
            i += 1;
            ops += 1;

            match op {
                0 => {
                    let mut out = FfiEntity::NULL;
                    // SAFETY: Active `cmds_ptr` and writable `out`.
                    if unsafe { host_cmd_spawn_empty(cmds_ptr, &mut out) } == FfiResult::OK {
                        known_entities.push(out);
                    }
                }
                1 => {
                    let count = if i < data.len() {
                        let c = (data[i] as usize) % 68;
                        i += 1;
                        c
                    } else {
                        0
                    };
                    let mut comp_vec = Vec::with_capacity(count);
                    let mut byte_bufs = Vec::with_capacity(count);
                    for _ in 0..count {
                        let h_raw = if i < data.len() {
                            let v = (data[i] % 7) as u32;
                            i += 1;
                            v
                        } else {
                            1
                        };
                        comp_vec.push(BamComponent::from_raw(h_raw));
                        let mut buf = [0u8; 32];
                        let avail = (data.len() - i).min(32);
                        buf[..avail].copy_from_slice(&data[i..i + avail]);
                        i += avail.min(4);
                        byte_bufs.push(buf);
                    }
                    let ptr_vec: Vec<*const u8> = byte_bufs.iter().map(|b| b.as_ptr()).collect();
                    let mut out = FfiEntity::NULL;
                    // SAFETY: `comp_vec` and `ptr_vec` hold `count` elements, and each buffer is 32 bytes
                    // (>= max registered raw-insertable component size of 8 bytes).
                    if unsafe {
                        host_cmd_spawn(
                            cmds_ptr,
                            comp_vec.as_ptr(),
                            ptr_vec.as_ptr(),
                            count,
                            &mut out,
                        )
                    } == FfiResult::OK
                    {
                        known_entities.push(out);
                    }
                }
                2 => {
                    let ent = if i < data.len() {
                        let idx = (data[i] as usize) % known_entities.len();
                        i += 1;
                        known_entities[idx]
                    } else {
                        FfiEntity::NULL
                    };
                    // SAFETY: Active `cmds_ptr`.
                    let _ = black_box(unsafe { host_cmd_despawn(cmds_ptr, ent) });
                }
                3 => {
                    let ent = if i < data.len() {
                        let idx = (data[i] as usize) % known_entities.len();
                        i += 1;
                        known_entities[idx]
                    } else {
                        FfiEntity::NULL
                    };
                    let comp = if i < data.len() {
                        let c = BamComponent::from_raw((data[i] % 7) as u32);
                        i += 1;
                        c
                    } else {
                        BamComponent::INVALID
                    };
                    let decl_len = if i < data.len() {
                        let l = (data[i] as usize) % 28;
                        i += 1;
                        l
                    } else {
                        8
                    };
                    let mut payload = [0u8; 32];
                    let n = (data.len() - i).min(decl_len);
                    payload[..n].copy_from_slice(&data[i..i + n]);
                    i += n;
                    let ptr = if decl_len == 0 && (ops & 1 == 0) {
                        core::ptr::null()
                    } else {
                        payload.as_ptr()
                    };
                    // SAFETY: `payload` has 32 readable bytes >= `decl_len` (`0..28`).
                    let _ =
                        black_box(unsafe { host_cmd_insert(cmds_ptr, ent, comp, ptr, decl_len) });
                }
                4 => {
                    let ent = if i < data.len() {
                        let idx = (data[i] as usize) % known_entities.len();
                        i += 1;
                        known_entities[idx]
                    } else {
                        FfiEntity::NULL
                    };
                    let comp = if i < data.len() {
                        let c = BamComponent::from_raw((data[i] % 7) as u32);
                        i += 1;
                        c
                    } else {
                        BamComponent::INVALID
                    };
                    // SAFETY: Active `cmds_ptr`.
                    let _ = black_box(unsafe { host_cmd_insert_default(cmds_ptr, ent, comp) });
                }
                5 => {
                    let ent = if i < data.len() {
                        let idx = (data[i] as usize) % known_entities.len();
                        i += 1;
                        known_entities[idx]
                    } else {
                        FfiEntity::NULL
                    };
                    let comp = if i < data.len() {
                        let c = BamComponent::from_raw((data[i] % 7) as u32);
                        i += 1;
                        c
                    } else {
                        BamComponent::INVALID
                    };
                    // SAFETY: Active `cmds_ptr`.
                    let _ = black_box(unsafe { host_cmd_remove(cmds_ptr, ent, comp) });
                }
                _ => {
                    // SAFETY: Null pointers are explicitly rejected by `host_cmd_*`.
                    unsafe {
                        let _ = black_box(host_cmd_spawn_empty(
                            core::ptr::null_mut(),
                            core::ptr::null_mut(),
                        ));
                        let _ = black_box(host_cmd_spawn(
                            cmds_ptr,
                            core::ptr::null(),
                            core::ptr::null(),
                            2,
                            core::ptr::null_mut(),
                        ));
                    }
                }
            }
        }
    }

    if data[0] & 1 == 0 {
        storage.apply_to_world(&mut world, &snapshot);
    } else {
        storage.clear();
    }
});
