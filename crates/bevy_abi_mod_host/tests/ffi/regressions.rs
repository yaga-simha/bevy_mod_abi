//! Wave 1 FFI regressions (`S-04` padded struct insertion, `S-10` entity bit layout) on ABI v0.2.

use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
use bevy_abi_mod_host_sys::{
    HostCommands, HostCommandsStorage, LoadContext, host_cmd_insert, host_cmd_spawn,
    host_cmd_spawn_empty, host_register_component,
};
use bevy_abi_mod_sys::{BamComponent, ComponentDesc, ComponentUuid, FfiEntity, FfiResult};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use core::mem::MaybeUninit;
use core::sync::atomic::AtomicBool;
use std::sync::Arc;

#[repr(C)]
#[derive(Component, ModComponent, Copy, Clone, Debug, PartialEq, Eq)]
#[mod_component(name = "ffi_reg::PaddedPair")]
struct PaddedPair {
    tag: u8,
    // 7 padding bytes at offsets 1..8
    value: u64,
}

// ============================================================================
// S-04: Padded struct insertion with uninitialized padding bytes
// ============================================================================

#[test]
fn regression_s04_padded_struct_insert() {
    assert_eq!(core::mem::size_of::<PaddedPair>(), 16);
    assert_eq!(core::mem::align_of::<PaddedPair>(), 8);

    // Construct a `PaddedPair` leaving its 7 padding bytes genuinely uninitialized.
    let mut raw_slot = MaybeUninit::<PaddedPair>::uninit();
    unsafe {
        let base = raw_slot.as_mut_ptr();
        (&raw mut (*base).tag).write(0x5A);
        (&raw mut (*base).value).write(0xDEAD_BEEF_CAFE_BABEu64);
    }
    let raw_bytes_ptr = raw_slot.as_ptr().cast::<u8>();

    // 1. Static host-registered component insert + batched spawn paths
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let padded_handle = app
        .register_mod_component::<PaddedPair>(ComponentOptions::new())
        .expect("register PaddedPair");

    let (e_static, e_static_batch) = {
        let world = app.world_mut();
        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let (e1, e2) = {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let cmds_ptr = cmds.as_ffi_ptr();

            let mut e1 = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(cmds_ptr, &mut e1) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        cmds_ptr,
                        e1,
                        padded_handle,
                        raw_bytes_ptr,
                        core::mem::size_of::<PaddedPair>(),
                    )
                },
                FfiResult::OK
            );

            let comps = [padded_handle];
            let ptrs = [raw_bytes_ptr];
            let mut e2 = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn(cmds_ptr, comps.as_ptr(), ptrs.as_ptr(), 1, &mut e2) },
                FfiResult::OK
            );
            (e1, e2)
        };
        storage.apply_to_world(world, &snap);
        (
            Entity::from_bits(e1.to_bits()),
            Entity::from_bits(e2.to_bits()),
        )
    };

    for ent in [e_static, e_static_batch] {
        let got = app
            .world()
            .get::<PaddedPair>(ent)
            .expect("PaddedPair inserted via static command path");
        assert_eq!(got.tag, 0x5A);
        assert_eq!(got.value, 0xDEAD_BEEF_CAFE_BABEu64);
    }

    // 2. Dynamic guest-registered component insert + batched spawn paths
    let mut dyn_handle = BamComponent::INVALID;
    {
        let world = app.world_mut();
        let reg = world.resource::<ModRegistry>().clone();
        let is_active = Arc::new(AtomicBool::new(true));
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, is_active, world as *mut World, None);

        let desc = ComponentDesc::new(
            c"ffi_reg::DynamicPaddedPair".as_ptr(),
            ComponentUuid::from_parts("ffi_reg::DynamicPaddedPair", 16, 8, 0),
            16,
            8,
        );
        let rc = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut dyn_handle) };
        assert_eq!(rc, FfiResult::OK);
        load_ctx.finish_init();
    }

    let dyn_comp_id = app
        .world()
        .resource::<ModRegistry>()
        .get_by_handle(dyn_handle)
        .expect("DynamicPaddedPair registered")
        .component_id;

    let (e_dyn, e_dyn_batch) = {
        let world = app.world_mut();
        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let (e1, e2) = {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let cmds_ptr = cmds.as_ffi_ptr();

            let mut e1 = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(cmds_ptr, &mut e1) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe { host_cmd_insert(cmds_ptr, e1, dyn_handle, raw_bytes_ptr, 16) },
                FfiResult::OK
            );

            let comps = [dyn_handle];
            let ptrs = [raw_bytes_ptr];
            let mut e2 = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn(cmds_ptr, comps.as_ptr(), ptrs.as_ptr(), 1, &mut e2) },
                FfiResult::OK
            );
            (e1, e2)
        };
        storage.apply_to_world(world, &snap);
        (
            Entity::from_bits(e1.to_bits()),
            Entity::from_bits(e2.to_bits()),
        )
    };

    for ent in [e_dyn, e_dyn_batch] {
        let entity_ref = app.world().entity(ent);
        let untyped_ptr = entity_ref
            .get_by_id(dyn_comp_id)
            .expect("DynamicPaddedPair present on entity");
        unsafe {
            let base = untyped_ptr.as_ptr();
            assert_eq!(core::ptr::read(base), 0x5A);
            assert_eq!(
                core::ptr::read_unaligned(base.add(8).cast::<u64>()),
                0xDEAD_BEEF_CAFE_BABEu64
            );
        }
    }
}

// ============================================================================
// S-10: Entity <-> FfiEntity bit-representation layout guard
// ============================================================================

#[test]
fn regression_s10_entity_bits_layout() {
    assert_eq!(
        core::mem::size_of::<Entity>(),
        core::mem::size_of::<FfiEntity>()
    );
    assert_eq!(
        core::mem::align_of::<Entity>(),
        core::mem::align_of::<FfiEntity>()
    );

    let mut world = World::new();
    let mut entities = Vec::new();
    for _ in 0..32 {
        entities.push(world.spawn_empty().id());
    }
    for e in entities.drain(..16).collect::<Vec<_>>() {
        world.despawn(e);
    }
    for _ in 0..32 {
        entities.push(world.spawn_empty().id());
    }

    for entity in entities {
        let raw_transmuted = unsafe { core::mem::transmute_copy::<Entity, u64>(&entity) };
        assert_eq!(raw_transmuted, entity.to_bits());
        let ffi = FfiEntity::from_bits(entity.to_bits());
        assert_eq!(ffi.to_bits(), raw_transmuted);
        assert_eq!(Entity::try_from_bits(ffi.to_bits()), Some(entity));
    }
}
