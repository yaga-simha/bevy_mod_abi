//! Hierarchy and `RelationshipTarget` inspection entrypoint (`sys_entity_children`).

use core::ffi::c_void;

use bevy_abi_mod_sys::{BAM_CHILDREN_NAME, BamComponent, BamSystemCtx, FfiEntity, FfiResult};
use bevy_ecs::{entity::Entity, hierarchy::Children, world::World};

use crate::{
    ffi::register::DynamicComponentHookMap,
    handle_header::{SYS_CTX_MAGIC, validate_handle_ref},
    registry::ModRegistry,
    system::HostSystemCtx,
};

/// Ensures Bevy's built-in [`Children`] hierarchy component is registered in `world` and in its
/// [`ModRegistry`] resource (if present) for zero-copy `sys_entity_children` traversal.
pub fn ensure_children_registered_in_world(world: &mut World) {
    let children_id = world.register_component::<Children>();
    if let Some(mut reg) = world.get_resource_mut::<ModRegistry>()
        && reg.get_by_name(BAM_CHILDREN_NAME).is_none()
    {
        let _ = reg.register_children_hierarchy(children_id);
    }
}

/// Borrows the contiguous slice of child or custom `RelationshipTarget` entities for `entity`,
/// writing the zero-copy pointer and length into `*out_entities` and `*out_len`.
///
/// # Safety
/// - `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
/// - `out_entities` must point to writable `*const FfiEntity` storage, or null.
/// - `out_len` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_sys_entity_children(
    sys_ctx: *const BamSystemCtx,
    entity: FfiEntity,
    relationship_target: BamComponent,
    out_entities: *mut *const FfiEntity,
    out_len: *mut usize,
) -> FfiResult {
    if out_entities.is_null() || out_len.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_entities.is_aligned() || !out_len.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_entities` and `out_len` are non-null, aligned, and writable.
    unsafe {
        *out_entities = core::ptr::null();
        *out_len = 0;
    }
    // SAFETY: Validates `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(ctx) = (unsafe {
        validate_handle_ref::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    }) else {
        return if sys_ctx.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    if entity.is_null() {
        return FfiResult::INVALID_ENTITY;
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return FfiResult::INVALID_ENTITY;
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return FfiResult::INVALID_ENTITY;
    }

    let (snapshot, world_opt) = ctx.snapshot_and_world();
    let Some(world) = world_opt else {
        return FfiResult::INVALID_STATE;
    };

    let target_meta = if !relationship_target.is_valid() {
        let Some(meta) = snapshot
            .get_by_name(BAM_CHILDREN_NAME)
            .or_else(|| snapshot.get_by_name("Children"))
        else {
            return FfiResult::NOT_FOUND;
        };
        meta
    } else {
        let Some(meta) = snapshot.get_by_handle(relationship_target) else {
            return FfiResult::INVALID_COMPONENT;
        };
        meta
    };

    if !ctx.has_component_read_access(target_meta.component_id) {
        return FfiResult::PERMISSION_DENIED;
    }

    let Ok(entity_cell) = world.get_entity(bevy_entity) else {
        return FfiResult::INVALID_ENTITY;
    };

    if let Some(extract_fn) = target_meta.extract_relationship_targets_fn {
        // SAFETY: `ctx.has_component_read_access` verified declared entity-column read access on
        // `target_meta.component_id` in Bevy's `FilteredAccessSet` for this system execution.
        let Some(comp_ptr) = (unsafe { entity_cell.get_by_id(target_meta.component_id) }) else {
            return FfiResult::OK;
        };
        // SAFETY: `comp_ptr` points to an initialized `RelationshipTarget` component matching
        // `target_meta.component_id` with shared read access for the duration of this system tick.
        let (slice_ptr, slice_len) = unsafe { extract_fn(comp_ptr.as_ptr()) };
        if slice_len > 0 && !slice_ptr.is_null() {
            // SAFETY: `out_entities` and `out_len` are non-null, aligned, and writable.
            unsafe {
                *out_entities = slice_ptr;
                *out_len = slice_len;
            }
        }
        return FfiResult::OK;
    }

    if target_meta.is_dynamic {
        // SAFETY: `DynamicComponentHookMap` is never mutably borrowed concurrently with system
        // execution (only mutated during `LoadContext` or exclusive `DeferredWorld` hooks).
        let Some(hook_map) = (unsafe { world.get_resource::<DynamicComponentHookMap>() }) else {
            return FfiResult::INVALID_COMPONENT;
        };
        if !hook_map.is_relationship_target(target_meta.component_id) {
            return FfiResult::INVALID_COMPONENT;
        }
        let slice = hook_map.relationship_targets_for(target_meta.component_id, bevy_entity);
        if !slice.is_empty() {
            // SAFETY: `out_entities` and `out_len` are non-null, aligned, and writable.
            unsafe {
                *out_entities = slice.as_ptr();
                *out_len = slice.len();
            }
        }
        return FfiResult::OK;
    }

    FfiResult::INVALID_COMPONENT
}

#[cfg(test)]
mod tests {
    use core::ffi::{CStr, c_void};
    use core::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use bevy_abi_mod_sys::{
        AccessMode, BAM_CHILD_OF_NAME, BAM_CHILDREN_NAME, BamCommands, BamComponent, BamQueryDesc,
        BamSystemCtx, BamSystemDesc, BamTermDesc, ComponentDesc, ComponentUuid, FfiChildOf,
        FfiEntity, FfiResult, ModComponent,
    };
    use bevy_ecs::{
        hierarchy::{ChildOf, Children},
        prelude::{Component, Entity},
        ptr::OwningPtr,
        schedule::Schedule,
        world::World,
    };

    use super::{ensure_children_registered_in_world, host_sys_entity_children};
    use crate::{
        ffi::commands::{host_cmd_insert, host_cmd_remove, host_cmd_spawn},
        ffi::register::{
            DynamicComponentHookMap, LoadContext, ensure_dynamic_hook_map_in_world,
            host_register_component, host_register_system,
        },
        loader::ModId,
        registry::ModRegistry,
        system::host_sys_commands,
    };

    #[repr(C)]
    #[derive(Component, Copy, Clone, Default)]
    struct TestPos(u32);

    impl ModComponent for TestPos {
        const CANONICAL_NAME: &'static str = "hier_test::TestPos";
        const COMPONENT_CSTR: &'static CStr = c"hier_test::TestPos";
        const UUID: ComponentUuid = ComponentUuid::from_canonical_name("hier_test::TestPos");
        const IS_RAW_INSERTABLE: bool = true;
        const IS_ABI_POD: bool = true;
        type VTable = ();

        fn vtable() -> &'static Self::VTable {
            &()
        }

        fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
            &[]
        }
    }

    #[derive(Component)]
    #[relationship(relationship_target = CustomFollowers)]
    struct CustomFollowing(#[allow(dead_code)] Entity);

    #[derive(Component)]
    #[relationship_target(relationship = CustomFollowing)]
    struct CustomFollowers(Vec<Entity>);

    impl core::ops::Deref for CustomFollowers {
        type Target = [Entity];

        fn deref(&self) -> &[Entity] {
            &self.0
        }
    }

    struct ChildrenReadProbe {
        target: FfiEntity,
        rel_target: BamComponent,
        out_status: Mutex<FfiResult>,
        out_ptr_addr: Mutex<usize>,
        out_entities: Mutex<Vec<FfiEntity>>,
    }

    unsafe extern "C" fn probe_children_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `ChildrenReadProbe`.
        let probe = unsafe { &*user_data.cast::<ChildrenReadProbe>() };
        let mut ptr: *const FfiEntity = core::ptr::null();
        let mut len: usize = 0;
        // SAFETY: `sys_ctx` is active during `GuestSystem` run and `ptr`/`len` are writable.
        let status = unsafe {
            host_sys_entity_children(
                sys_ctx.cast_const(),
                probe.target,
                probe.rel_target,
                &mut ptr,
                &mut len,
            )
        };
        *probe.out_status.lock().unwrap() = status;
        *probe.out_ptr_addr.lock().unwrap() = ptr as usize;
        if status.is_ok() && len > 0 && !ptr.is_null() {
            // SAFETY: `host_sys_entity_children` validated `ptr[0..len]` for this system tick.
            let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
            *probe.out_entities.lock().unwrap() = slice.to_vec();
        } else {
            probe.out_entities.lock().unwrap().clear();
        }
        FfiResult::OK
    }

    struct HierarchyCmdPlan {
        child_of_handle: BamComponent,
        pos_handle: BamComponent,
        parent: FfiEntity,
        existing_child: FfiEntity,
        detach_child: FfiEntity,
        spawned_child: Mutex<FfiEntity>,
    }

    unsafe extern "C" fn apply_hierarchy_cmds_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `HierarchyCmdPlan`.
        let plan = unsafe { &*user_data.cast::<HierarchyCmdPlan>() };
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        // SAFETY: `sys_ctx` is an active `HostSystemCtx`.
        let rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
        if rc.is_err() {
            return rc;
        }

        // 1. add_child via `host_cmd_insert(FfiChildOf)`
        let link = FfiChildOf::new(plan.parent);
        // SAFETY: `cmds` is active and `link` matches `FfiChildOf` layout.
        let r_add = unsafe {
            host_cmd_insert(
                cmds,
                plan.existing_child,
                plan.child_of_handle,
                (&raw const link).cast::<u8>(),
                core::mem::size_of::<FfiChildOf>(),
            )
        };
        if r_add.is_err() {
            return r_add;
        }

        // 2. remove_parent via `host_cmd_remove(ChildOf)`
        // SAFETY: `cmds` is active.
        let r_rem = unsafe { host_cmd_remove(cmds, plan.detach_child, plan.child_of_handle) };
        if r_rem.is_err() {
            return r_rem;
        }

        // 3. spawn_child via `host_cmd_spawn([ChildOf, TestPos])`
        let pos = TestPos(77);
        let comps = [plan.child_of_handle, plan.pos_handle];
        let ptrs = [
            (&raw const link).cast::<u8>(),
            (&raw const pos).cast::<u8>(),
        ];
        let mut spawned = FfiEntity::NULL;
        // SAFETY: `cmds` is active and `comps`/`ptrs` match registered component layouts.
        let r_spawn = unsafe {
            host_cmd_spawn(
                cmds,
                comps.as_ptr(),
                ptrs.as_ptr(),
                comps.len(),
                &mut spawned,
            )
        };
        if r_spawn.is_err() {
            return r_spawn;
        }
        *plan.spawned_child.lock().unwrap() = spawned;
        FfiResult::OK
    }

    #[test]
    fn children_zero_copy_slice_and_undeclared_permission_denied() {
        let mut world = World::new();
        ensure_dynamic_hook_map_in_world(&mut world);
        let mut reg = ModRegistry::new();
        let pos_cid = world.register_component::<TestPos>();
        let pos_handle = reg
            .register_component::<TestPos>(pos_cid, Default::default())
            .unwrap();
        world.insert_resource(reg.clone());

        // Idempotent world registration check.
        ensure_children_registered_in_world(&mut world);
        ensure_children_registered_in_world(&mut world);
        reg = world.resource::<ModRegistry>().clone();

        let parent = world.spawn(TestPos(1)).id();
        let child1 = world.spawn((TestPos(2), ChildOf(parent))).id();
        let child2 = world.spawn((TestPos(3), ChildOf(parent))).id();
        let leaf = world.spawn(TestPos(4)).id();

        let parent_ffi = FfiEntity::from_bits(parent.to_bits());
        let child1_ffi = FfiEntity::from_bits(child1.to_bits());
        let child2_ffi = FfiEntity::from_bits(child2.to_bits());
        let leaf_ffi = FfiEntity::from_bits(leaf.to_bits());

        let children_handle = reg.get_by_name(BAM_CHILDREN_NAME).unwrap().handle;

        let active = Arc::new(AtomicBool::new(true));
        let probe_ok = Box::new(ChildrenReadProbe {
            target: parent_ffi,
            rel_target: BamComponent::INVALID,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let probe_no_access = Box::new(ChildrenReadProbe {
            target: parent_ffi,
            rel_target: BamComponent::INVALID,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let probe_filter_only = Box::new(ChildrenReadProbe {
            target: parent_ffi,
            rel_target: BamComponent::INVALID,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });

        let systems = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg.clone(),
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(1)),
            );

            // System 1: declares READ on Children.
            let t_read = [BamTermDesc::new(children_handle, AccessMode::READ)];
            let q_read = [BamQueryDesc::new(t_read.as_ptr(), t_read.len())];
            let s1 = BamSystemDesc::new(
                c"probe_ok".as_ptr(),
                q_read.as_ptr(),
                q_read.len(),
                probe_children_system,
                (&*probe_ok as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rs1 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s1) };
            assert_eq!(rs1, FfiResult::OK);

            // System 2: declares READ on TestPos only (NO Children read access).
            let t_pos = [BamTermDesc::new(pos_handle, AccessMode::READ)];
            let q_pos = [BamQueryDesc::new(t_pos.as_ptr(), t_pos.len())];
            let s2 = BamSystemDesc::new(
                c"probe_denied".as_ptr(),
                q_pos.as_ptr(),
                q_pos.len(),
                probe_children_system,
                (&*probe_no_access as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rs2 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s2) };
            assert_eq!(rs2, FfiResult::OK);

            // System 3: declares With<Children> (FILTER_ONLY, NOT component data read).
            let t_filter = [BamTermDesc::new(children_handle, AccessMode::FILTER_ONLY)];
            let q_filter = [BamQueryDesc::new(t_filter.as_ptr(), t_filter.len())];
            let s3 = BamSystemDesc::new(
                c"probe_filter_only".as_ptr(),
                q_filter.as_ptr(),
                q_filter.len(),
                probe_children_system,
                (&*probe_filter_only as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rs3 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s3) };
            assert_eq!(rs3, FfiResult::OK);

            ctx.finish_init();
            core::mem::take(&mut ctx.registered_systems)
        };

        let mut sched = Schedule::default();
        for sys in systems {
            sched.add_systems(sys);
        }
        sched.run(&mut world);

        assert_eq!(*probe_ok.out_status.lock().unwrap(), FfiResult::OK);
        assert_eq!(
            *probe_ok.out_entities.lock().unwrap(),
            vec![child1_ffi, child2_ffi]
        );
        // Zero-copy guarantee: returned pointer matches Bevy's native `Children` backing buffer.
        let expected_addr = world.entity(parent).get::<Children>().unwrap().as_ptr() as usize;
        assert_eq!(*probe_ok.out_ptr_addr.lock().unwrap(), expected_addr);

        // Undeclared / filter-only access MUST return PERMISSION_DENIED.
        assert_eq!(
            *probe_no_access.out_status.lock().unwrap(),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            *probe_filter_only.out_status.lock().unwrap(),
            FfiResult::PERMISSION_DENIED
        );

        // Leaf without Children returns Ok(&[]) and null/dead entity returns INVALID_ENTITY.
        let leaf_check = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                world.resource::<ModRegistry>().clone(),
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(2)),
            );
            let probe_leaf = Box::new(ChildrenReadProbe {
                target: leaf_ffi,
                rel_target: BamComponent::INVALID,
                out_status: Mutex::new(FfiResult::UNSUPPORTED),
                out_ptr_addr: Mutex::new(1),
                out_entities: Mutex::new(Vec::new()),
            });
            let t_read = [BamTermDesc::new(children_handle, AccessMode::READ)];
            let q_read = [BamQueryDesc::new(t_read.as_ptr(), t_read.len())];
            let s_leaf = BamSystemDesc::new(
                c"probe_leaf".as_ptr(),
                q_read.as_ptr(),
                q_read.len(),
                probe_children_system,
                (&*probe_leaf as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rl = unsafe { host_register_system(ctx.as_ffi_ptr(), &s_leaf) };
            assert_eq!(rl, FfiResult::OK);
            ctx.finish_init();
            (core::mem::take(&mut ctx.registered_systems), probe_leaf)
        };
        let mut leaf_sched = Schedule::default();
        for sys in leaf_check.0 {
            leaf_sched.add_systems(sys);
        }
        leaf_sched.run(&mut world);
        assert_eq!(*leaf_check.1.out_status.lock().unwrap(), FfiResult::OK);
        assert_eq!(*leaf_check.1.out_ptr_addr.lock().unwrap(), 0);
        assert!(leaf_check.1.out_entities.lock().unwrap().is_empty());

        // Null / uninitialized ctx & pointers checks.
        let mut p: *const FfiEntity = core::ptr::null();
        let mut l: usize = 0;
        // SAFETY: Null ctx handled safely.
        let r_null = unsafe {
            host_sys_entity_children(
                core::ptr::null(),
                parent_ffi,
                BamComponent::INVALID,
                &mut p,
                &mut l,
            )
        };
        assert_eq!(r_null, FfiResult::NULL_POINTER);
    }

    #[test]
    fn ffi_child_of_commands_add_spawn_and_remove_parent_update_children() {
        let mut world = World::new();
        ensure_dynamic_hook_map_in_world(&mut world);
        let mut reg = ModRegistry::new();
        let pos_cid = world.register_component::<TestPos>();
        let pos_handle = reg
            .register_component::<TestPos>(pos_cid, Default::default())
            .unwrap();
        let child_of_cid = world.register_component::<ChildOf>();
        let child_of_handle = reg.register_hierarchy(child_of_cid, true).unwrap();
        world.insert_resource(reg.clone());

        let parent = world.spawn_empty().id();
        let existing_child = world.spawn_empty().id();
        let detach_child = world.spawn(ChildOf(parent)).id();

        let parent_ffi = FfiEntity::from_bits(parent.to_bits());
        let existing_ffi = FfiEntity::from_bits(existing_child.to_bits());
        let detach_ffi = FfiEntity::from_bits(detach_child.to_bits());

        let active = Arc::new(AtomicBool::new(true));
        let plan = Box::new(HierarchyCmdPlan {
            child_of_handle,
            pos_handle,
            parent: parent_ffi,
            existing_child: existing_ffi,
            detach_child: detach_ffi,
            spawned_child: Mutex::new(FfiEntity::NULL),
        });
        let probe = Box::new(ChildrenReadProbe {
            target: parent_ffi,
            rel_target: BamComponent::INVALID,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });

        let (sys_cmds, sys_probe, children_handle) = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(3)),
            );
            let ch_handle = ctx
                .registry
                .get_by_name(BAM_CHILDREN_NAME)
                .expect("LoadContext auto-registers Children")
                .handle;
            let s_cmd = BamSystemDesc::new(
                c"apply_hierarchy_cmds".as_ptr(),
                core::ptr::null(),
                0,
                apply_hierarchy_cmds_system,
                (&*plan as *const HierarchyCmdPlan)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rc1 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s_cmd) };
            assert_eq!(rc1, FfiResult::OK);
            let t_read = [BamTermDesc::new(ch_handle, AccessMode::READ)];
            let q_read = [BamQueryDesc::new(t_read.as_ptr(), t_read.len())];
            let s_probe = BamSystemDesc::new(
                c"probe_after_cmds".as_ptr(),
                q_read.as_ptr(),
                q_read.len(),
                probe_children_system,
                (&*probe as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rc2 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s_probe) };
            assert_eq!(rc2, FfiResult::OK);
            ctx.finish_init();
            let mut systems = core::mem::take(&mut ctx.registered_systems);
            let s2 = systems.pop().unwrap();
            let s1 = systems.pop().unwrap();
            (s1, s2, ch_handle)
        };

        assert!(children_handle.is_valid());
        let mut cmd_sched = Schedule::default();
        cmd_sched.add_systems(sys_cmds);
        cmd_sched.run(&mut world);

        let spawned_ffi = *plan.spawned_child.lock().unwrap();
        assert!(!spawned_ffi.is_null());
        let spawned_ent = Entity::try_from_bits(spawned_ffi.to_bits()).unwrap();
        assert_eq!(world.entity(spawned_ent).get::<TestPos>().unwrap().0, 77);
        assert!(world.entity(detach_child).get::<ChildOf>().is_none());

        let mut probe_sched = Schedule::default();
        probe_sched.add_systems(sys_probe);
        probe_sched.run(&mut world);

        assert_eq!(*probe.out_status.lock().unwrap(), FfiResult::OK);
        let read_children = probe.out_entities.lock().unwrap().clone();
        assert_eq!(read_children.len(), 2);
        assert!(read_children.contains(&existing_ffi));
        assert!(read_children.contains(&spawned_ffi));
        assert!(!read_children.contains(&detach_ffi));
        assert_eq!(BAM_CHILD_OF_NAME, "bevy_ecs::hierarchy::ChildOf");
    }

    #[test]
    fn dynamic_relationship_pair_attach_retarget_self_guard_despawn_and_zero_copy_read() {
        let mut world = World::new();
        ensure_dynamic_hook_map_in_world(&mut world);
        let reg = ModRegistry::new();
        let active = Arc::new(AtomicBool::new(true));

        let mut rel_h = BamComponent::INVALID;
        let mut tgt_h = BamComponent::INVALID;
        let mut bind_out = BamComponent::INVALID;

        let reg_after = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(4)),
            );
            let rel_desc = ComponentDesc::new(
                c"space::Orbiting".as_ptr(),
                ComponentUuid::from_parts("space::Orbiting", 8, 8, 0),
                8,
                8,
            );
            let tgt_desc = ComponentDesc::new(
                c"space::Orbiters".as_ptr(),
                ComponentUuid::from_parts("space::Orbiters", 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &rel_desc, &mut rel_h),
                    FfiResult::OK
                );
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &tgt_desc, &mut tgt_h),
                    FfiResult::OK
                );
            }
            let bind_name =
                std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", rel_h.0, tgt_h.0))
                    .unwrap();
            let bind_desc = ComponentDesc::new(
                bind_name.as_ptr(),
                ComponentUuid::from_parts(bind_name.to_str().unwrap(), 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and synthetic `__bam_rel_bind` descriptor.
            let rb =
                unsafe { host_register_component(ctx.as_ffi_ptr(), &bind_desc, &mut bind_out) };
            assert_eq!(rb, FfiResult::OK);
            assert_eq!(bind_out, tgt_h);

            // Idempotent re-bind succeeds; wrong self-bind fails.
            let mut rebind = BamComponent::INVALID;
            // SAFETY: Valid `LoadContext` and synthetic `__bam_rel_bind` descriptor.
            let rr = unsafe { host_register_component(ctx.as_ffi_ptr(), &bind_desc, &mut rebind) };
            assert_eq!(rr, FfiResult::OK);
            let bad_self_name =
                std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", rel_h.0, rel_h.0))
                    .unwrap();
            let bad_self_desc = ComponentDesc::new(
                bad_self_name.as_ptr(),
                ComponentUuid::from_parts("bad_self", 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and invalid self-bind descriptor.
            let r_bad =
                unsafe { host_register_component(ctx.as_ffi_ptr(), &bad_self_desc, &mut rebind) };
            assert_eq!(r_bad, FfiResult::TYPE_MISMATCH);

            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        let rel_cid = reg_after.get_by_handle(rel_h).unwrap().component_id;
        let tgt_cid = reg_after.get_by_handle(tgt_h).unwrap().component_id;

        let sun1 = world.spawn_empty().id();
        let sun2 = world.spawn_empty().id();
        let planet1 = world.spawn_empty().id();
        let planet2 = world.spawn_empty().id();
        let sun1_ffi = FfiEntity::from_bits(sun1.to_bits());
        let sun2_ffi = FfiEntity::from_bits(sun2.to_bits());
        let planet1_ffi = FfiEntity::from_bits(planet1.to_bits());
        let planet2_ffi = FfiEntity::from_bits(planet2.to_bits());

        // 1. on_insert attach: planet1 & planet2 orbit sun1.
        let mut target_bits_1 = sun1_ffi.to_bits();
        let mut target_bits_2 = sun1_ffi.to_bits();
        // SAFETY: `rel_cid` has `(8, 8)` layout holding `FfiEntity` (`u64` bits).
        unsafe {
            world.entity_mut(planet1).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut target_bits_1).cast()),
            );
            world.entity_mut(planet2).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut target_bits_2).cast()),
            );
        }
        assert!(world.entity(sun1).contains_id(tgt_cid));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun1),
            &[planet1_ffi, planet2_ffi]
        );

        // Zero-copy read via scheduled GuestSystem + undeclared PERMISSION_DENIED check.
        let probe_ok = Box::new(ChildrenReadProbe {
            target: sun1_ffi,
            rel_target: tgt_h,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let probe_denied = Box::new(ChildrenReadProbe {
            target: sun1_ffi,
            rel_target: tgt_h,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let systems = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg_after,
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(5)),
            );
            let t_ok = [BamTermDesc::new(tgt_h, AccessMode::READ)];
            let q_ok = [BamQueryDesc::new(t_ok.as_ptr(), t_ok.len())];
            let s_ok = BamSystemDesc::new(
                c"read_orbiters".as_ptr(),
                q_ok.as_ptr(),
                q_ok.len(),
                probe_children_system,
                (&*probe_ok as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            let s_den = BamSystemDesc::new(
                c"read_orbiters_denied".as_ptr(),
                core::ptr::null(),
                0,
                probe_children_system,
                (&*probe_denied as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`s.
            unsafe {
                assert_eq!(host_register_system(ctx.as_ffi_ptr(), &s_ok), FfiResult::OK);
                assert_eq!(
                    host_register_system(ctx.as_ffi_ptr(), &s_den),
                    FfiResult::OK
                );
            }
            ctx.finish_init();
            core::mem::take(&mut ctx.registered_systems)
        };
        let mut sched = Schedule::default();
        for sys in systems {
            sched.add_systems(sys);
        }
        sched.run(&mut world);

        assert_eq!(*probe_ok.out_status.lock().unwrap(), FfiResult::OK);
        assert_eq!(
            *probe_ok.out_entities.lock().unwrap(),
            vec![planet1_ffi, planet2_ffi]
        );
        let map_slice_addr = world
            .resource::<DynamicComponentHookMap>()
            .relationship_targets_for(tgt_cid, sun1)
            .as_ptr() as usize;
        assert_eq!(*probe_ok.out_ptr_addr.lock().unwrap(), map_slice_addr);
        assert_eq!(
            *probe_denied.out_status.lock().unwrap(),
            FfiResult::PERMISSION_DENIED
        );

        // 2. Retarget overwrite (on_discard + on_insert): move planet1 then planet2 to sun2.
        let mut retarget_1 = sun2_ffi.to_bits();
        let mut retarget_2 = sun2_ffi.to_bits();
        // SAFETY: `rel_cid` has `(8, 8)` layout.
        unsafe {
            world.entity_mut(planet1).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut retarget_1).cast()),
            );
        }
        assert!(world.entity(sun1).contains_id(tgt_cid));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun1),
            &[planet2_ffi]
        );
        assert!(world.entity(sun2).contains_id(tgt_cid));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2),
            &[planet1_ffi]
        );

        // SAFETY: `rel_cid` has `(8, 8)` layout.
        unsafe {
            world.entity_mut(planet2).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut retarget_2).cast()),
            );
        }
        assert!(!world.entity(sun1).contains_id(tgt_cid));
        assert!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun1)
                .is_empty()
        );
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2),
            &[planet1_ffi, planet2_ffi]
        );

        // 3. Self-referential guard: inserting Orbiting(planet1) on planet1 removes Orbiting.
        let mut self_bits = planet1_ffi.to_bits();
        // SAFETY: `rel_cid` has `(8, 8)` layout.
        unsafe {
            world.entity_mut(planet1).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut self_bits).cast()),
            );
        }
        assert!(!world.entity(planet1).contains_id(rel_cid));
        assert!(!world.entity(planet1).contains_id(tgt_cid));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2),
            &[planet2_ffi]
        );

        // 4. Re-attach planet1 -> sun2, despawn planet2 (source despawn cleanup).
        let mut reattach_1 = sun2_ffi.to_bits();
        // SAFETY: `rel_cid` has `(8, 8)` layout.
        unsafe {
            world.entity_mut(planet1).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut reattach_1).cast()),
            );
        }
        assert!(world.despawn(planet2));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2),
            &[planet1_ffi]
        );

        // 5. Target despawn cleanup: despawn sun2 -> planet1 loses Orbiting cleanly.
        assert!(world.despawn(sun2));
        assert!(!world.entity(planet1).contains_id(rel_cid));
        assert!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2)
                .is_empty()
        );
    }

    #[test]
    fn bridged_custom_relationship_target_and_load_ctx_rollback() {
        let mut world = World::new();
        ensure_dynamic_hook_map_in_world(&mut world);
        let mut reg = ModRegistry::new();
        let followers_cid = world.register_component::<CustomFollowers>();
        let followers_h = reg
            .register_relationship_target::<CustomFollowers>(
                "custom_test::CustomFollowers",
                Some("CustomFollowers"),
                followers_cid,
            )
            .unwrap();

        let target = world.spawn_empty().id();
        let f1 = world.spawn(CustomFollowing(target)).id();
        let f2 = world.spawn(CustomFollowing(target)).id();
        let target_ffi = FfiEntity::from_bits(target.to_bits());
        let f1_ffi = FfiEntity::from_bits(f1.to_bits());
        let f2_ffi = FfiEntity::from_bits(f2.to_bits());

        let probe = Box::new(ChildrenReadProbe {
            target: target_ffi,
            rel_target: followers_h,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let active = Arc::new(AtomicBool::new(true));
        let sys = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg.clone(),
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(6)),
            );
            let t_read = [BamTermDesc::new(followers_h, AccessMode::READ)];
            let q_read = [BamQueryDesc::new(t_read.as_ptr(), t_read.len())];
            let s = BamSystemDesc::new(
                c"probe_custom_rt".as_ptr(),
                q_read.as_ptr(),
                q_read.len(),
                probe_children_system,
                (&*probe as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rs = unsafe { host_register_system(ctx.as_ffi_ptr(), &s) };
            assert_eq!(rs, FfiResult::OK);
            ctx.finish_init();
            ctx.registered_systems.pop().unwrap()
        };
        let mut sched = Schedule::default();
        sched.add_systems(sys);
        sched.run(&mut world);

        assert_eq!(*probe.out_status.lock().unwrap(), FfiResult::OK);
        assert_eq!(*probe.out_entities.lock().unwrap(), vec![f1_ffi, f2_ffi]);
        let expected_addr = world
            .entity(target)
            .get::<CustomFollowers>()
            .unwrap()
            .as_ptr() as usize;
        assert_eq!(*probe.out_ptr_addr.lock().unwrap(), expected_addr);

        // Verify `LoadContext::rollback()` cleans up uncommitted `__bam_rel_bind` pairs.
        let mut ctx_rb = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            &mut world as *mut World,
            Some(ModId(7)),
        );
        let mut h_r = BamComponent::INVALID;
        let mut h_t = BamComponent::INVALID;
        let mut h_b = BamComponent::INVALID;
        let d_r = ComponentDesc::new(
            c"rb::Rel".as_ptr(),
            ComponentUuid::from_parts("rb::Rel", 8, 8, 0),
            8,
            8,
        );
        let d_t = ComponentDesc::new(
            c"rb::Tgt".as_ptr(),
            ComponentUuid::from_parts("rb::Tgt", 0, 1, 0),
            0,
            1,
        );
        // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
        unsafe {
            assert_eq!(
                host_register_component(ctx_rb.as_ffi_ptr(), &d_r, &mut h_r),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx_rb.as_ffi_ptr(), &d_t, &mut h_t),
                FfiResult::OK
            );
        }
        let tgt_cid_rb = ctx_rb.registry.get_by_handle(h_t).unwrap().component_id;
        let bind_name =
            std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", h_r.0, h_t.0)).unwrap();
        let d_b = ComponentDesc::new(
            bind_name.as_ptr(),
            ComponentUuid::from_parts(bind_name.to_str().unwrap(), 0, 1, 0),
            0,
            1,
        );
        // SAFETY: Valid `LoadContext` and `__bam_rel_bind` descriptor.
        let r_bind = unsafe { host_register_component(ctx_rb.as_ffi_ptr(), &d_b, &mut h_b) };
        assert_eq!(r_bind, FfiResult::OK);
        assert!(
            world
                .resource::<DynamicComponentHookMap>()
                .is_relationship_target(tgt_cid_rb)
        );
        ctx_rb.rollback();
        assert!(
            !world
                .resource::<DynamicComponentHookMap>()
                .is_relationship_target(tgt_cid_rb)
        );
    }
}
