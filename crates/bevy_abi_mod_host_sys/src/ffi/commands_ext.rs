//! Extended deferred entity command entrypoints (`cmd_spawn_persistent`, `cmd_spawn_batch`,
//! `cmd_despawn_all`, and `cmd_insert_if_new`).

use core::ffi::c_void;

use bevy_abi_mod_sys::{BamCommands, BamComponent, BamTermDesc, FfiEntity, FfiResult};

use crate::{
    ffi::commands::{HostCommands, spawn_batch_with_scope},
    handle_header::{COMMANDS_MAGIC, validate_handle_mut},
};

/// Reserves a fresh entity whose lifecycle persists across mod unload and queues a batched spawn.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `out_entity` must point to writable [`FfiEntity`] storage, or null.
/// - When `count > 0`, `components` and `data_ptrs` must each point to `count` readable entries.
pub unsafe extern "C" fn host_cmd_spawn_persistent(
    commands: *mut BamCommands,
    components: *const BamComponent,
    data_ptrs: *const *const u8,
    count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: `cmds` is validated, `out_entity` is non-null and writable, and caller guarantees `components`/`data_ptrs`.
    unsafe { spawn_batch_with_scope(cmds, components, data_ptrs, count, out_entity, true) }
}

/// Reserves `entity_count` fresh entities and queues a bulk SoA spawn across `components[0..component_count]`.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - When `component_count > 0`, `components` and `column_ptrs` must be non-null and readable.
/// - When `out_entities` is non-null and `entity_count > 0`, `out_entities` must be writable for `entity_count` slots.
pub unsafe extern "C" fn host_cmd_spawn_batch(
    commands: *mut BamCommands,
    components: *const BamComponent,
    column_ptrs: *const *const u8,
    component_count: usize,
    entity_count: usize,
    out_entities: *mut FfiEntity,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: `host_cmds` is validated and caller guarantees `components`, `column_ptrs`, and
    // optional `out_entities` invariants.
    unsafe {
        host_cmds.spawn_soa_batch_raw(
            components,
            column_ptrs,
            component_count,
            entity_count,
            out_entities,
        )
    }
}

/// Queues deferred bulk despawning (`World::despawn_all`) of non-resource entities matching `terms[0..term_count]`.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - When `term_count > 0`, `terms` must point to `term_count` readable [`BamTermDesc`] elements.
pub unsafe extern "C" fn host_cmd_despawn_all(
    commands: *mut BamCommands,
    terms: *const BamTermDesc,
    term_count: usize,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: `host_cmds` is validated and caller guarantees `terms` is readable for `term_count`.
    unsafe { host_cmds.despawn_all_raw(terms, term_count) }
}

/// Queues deferred conditional insertion (`InsertMode::Keep`) of `component` onto `entity` iff absent.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - When `data_len > 0`, `data_ptr` must point to `data_len` readable bytes.
pub unsafe extern "C" fn host_cmd_insert_if_new(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: `host_cmds` is validated and caller guarantees `data_ptr` is readable for `data_len`.
    unsafe { host_cmds.insert_raw_inner(entity, component, data_ptr, data_len, true) }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
