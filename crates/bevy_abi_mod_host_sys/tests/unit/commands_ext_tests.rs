use super::*;
use crate::HostCommands;
use crate::ffi::commands::HostCommandsStorage;
use crate::loader::ModId;
use crate::registry::{ComponentOptions, ModRegistry};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::AccessMode;
use bevy_ecs::component::Component;
use bevy_ecs::world::World;
use core::ptr::{null, null_mut};

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "commands_ext_test::Val")]
struct CmdExtVal(i32);

#[test]
fn extended_commands_guards_and_execution() {
    let mut world = World::new();
    let cid = world.register_component::<CmdExtVal>();
    let mut reg = ModRegistry::new();
    let val_h = reg
        .register_component::<CmdExtVal>(cid, ComponentOptions::new().with_raw_insert())
        .unwrap();
    let snap = reg.snapshot_arc();
    let bare = world.spawn_empty().id();
    let bare_ffi = FfiEntity::from_bits(bare.to_bits());
    let mut storage = HostCommandsStorage::new();
    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
        .with_mod_context(Some(ModId(1)), None);
    let mut out_ent = FfiEntity::NULL;

    // Guard branches: null `out_entity` and null/invalid `commands`.
    let bad_cmds = (&raw const snap).cast_mut().cast::<BamCommands>();
    // SAFETY: Intentional null/invalid handles are validated before dereferencing.
    unsafe {
        assert_eq!(
            host_cmd_spawn_persistent(cmds.as_ffi_ptr(), null(), null(), 0, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_persistent(null_mut(), null(), null(), 0, &raw mut out_ent),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_persistent(bad_cmds, null(), null(), 0, &raw mut out_ent),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_spawn_batch(null_mut(), null(), null(), 0, 0, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_batch(bad_cmds, null(), null(), 0, 0, null_mut()),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_despawn_all(null_mut(), null(), 0),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_despawn_all(bad_cmds, null(), 0),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_insert_if_new(null_mut(), FfiEntity::NULL, val_h, null(), 0),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_insert_if_new(bad_cmds, FfiEntity::NULL, val_h, null(), 0),
            FfiResult::INVALID_STATE
        );
    }

    // Functional path for all 4 entrypoints:
    let init_val = CmdExtVal(10);
    let comps = [val_h];
    let ptrs = [(&raw const init_val).cast::<u8>()];
    let soa_vals = [CmdExtVal(20), CmdExtVal(30)];
    let soa_ptrs = [soa_vals.as_ptr().cast::<u8>()];
    let mut soa_ents = [FfiEntity::NULL; 2];
    let keep_val = CmdExtVal(99);
    let fresh_val = CmdExtVal(77);

    // SAFETY: All pointers reference valid stack arrays matching `CmdExtVal`'s layout.
    unsafe {
        assert_eq!(
            host_cmd_spawn_persistent(
                cmds.as_ffi_ptr(),
                comps.as_ptr(),
                ptrs.as_ptr(),
                1,
                &raw mut out_ent,
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_spawn_batch(
                cmds.as_ffi_ptr(),
                comps.as_ptr(),
                soa_ptrs.as_ptr(),
                1,
                2,
                soa_ents.as_mut_ptr(),
            ),
            FfiResult::OK
        );
        // Keep existing `10` on `out_ent`, insert `77` on `bare`.
        assert_eq!(
            host_cmd_insert_if_new(
                cmds.as_ffi_ptr(),
                out_ent,
                val_h,
                (&raw const keep_val).cast::<u8>(),
                core::mem::size_of::<CmdExtVal>(),
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_insert_if_new(
                cmds.as_ffi_ptr(),
                bare_ffi,
                val_h,
                (&raw const fresh_val).cast::<u8>(),
                core::mem::size_of::<CmdExtVal>(),
            ),
            FfiResult::OK
        );
    }
    drop(cmds);
    storage.apply_to_world(&mut world, &snap);

    let persist_e = bevy_ecs::entity::Entity::try_from_bits(out_ent.to_bits()).unwrap();
    let batch_e0 = bevy_ecs::entity::Entity::try_from_bits(soa_ents[0].to_bits()).unwrap();
    let batch_e1 = bevy_ecs::entity::Entity::try_from_bits(soa_ents[1].to_bits()).unwrap();
    assert_eq!(
        world.entity(persist_e).get::<CmdExtVal>(),
        Some(&CmdExtVal(10))
    );
    assert_eq!(world.entity(bare).get::<CmdExtVal>(), Some(&CmdExtVal(77)));
    assert_eq!(
        world.entity(batch_e0).get::<CmdExtVal>(),
        Some(&CmdExtVal(20))
    );
    assert_eq!(
        world.entity(batch_e1).get::<CmdExtVal>(),
        Some(&CmdExtVal(30))
    );

    // `host_cmd_despawn_all` removes all entities matching `With<CmdExtVal>`.
    let mut cmds2 = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
    let term = [BamTermDesc::new(val_h, AccessMode::FILTER_ONLY)];
    // SAFETY: `cmds2` and `term` are valid on stack.
    unsafe {
        assert_eq!(
            host_cmd_despawn_all(cmds2.as_ffi_ptr(), term.as_ptr(), 1),
            FfiResult::OK
        );
    }
    drop(cmds2);
    storage.apply_to_world(&mut world, &snap);
    assert!(world.get_entity(persist_e).is_err());
    assert!(world.get_entity(bare).is_err());
    assert!(world.get_entity(batch_e0).is_err());
    assert!(world.get_entity(batch_e1).is_err());
}
