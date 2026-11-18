//! Integration tests for `bridge-bsn` (`ModBsnPlugin`, `HostSceneRegistry`,
//! `expose_mod_scene`, `expose_mod_scene_bsn`, `expose_mod_scene_patch`, `cmd_spawn_scene`,
//! `ChildOf` attachment, `ModScoped` cleanup, and `bevy_scene::Ready` guest observers).

use std::sync::{Arc, Mutex};

use bevy_abi_mod_host::bridges::bsn::{
    CANONICAL_READY_NAME, HostSceneRegistry, ModBsnAppExt, ModBsnPlugin, SCENE_PATCH_ALIAS,
};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, ModId, ModScoped, host_cmd_observe, host_cmd_spawn_scene,
    host_sys_commands,
};
use bevy_abi_mod_sys::{
    BamEventKind, BamObserverDesc, BamSystemCtx, ComponentUuid, FfiAssetHandle, FfiEntity,
    FfiResult,
};
use bevy_app::{App, TaskPoolPlugin, Update};
use bevy_asset::{AssetPlugin, AssetServer, Assets};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::hierarchy::{ChildOf, Children};
use bevy_ecs::name::Name;
use bevy_ecs::observer::On;
use bevy_ecs::world::World;
use bevy_scene::{Ready, ScenePatch, bsn};
use core::ffi::c_void;

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bsn_test::BsnTag")]
struct BsnTag {
    value: u32,
}

fn setup_bsn_app() -> App {
    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        AssetPlugin::default(),
        AbiModPlugin,
        ModBsnPlugin,
    ));
    app.register_mod_component::<BsnTag>(ComponentOptions::new().with_default())
        .expect("register BsnTag");
    app
}

#[test]
fn expose_mod_scene_bsn_and_patch_spawn_and_notify_guest_ready_observers() {
    let mut app = setup_bsn_app();

    let custom_handle = app
        .expose_mod_scene(
            "bsn_test::custom_tower",
            |world: &mut World, root: Entity| {
                world.entity_mut(root).insert(BsnTag { value: 77 });
                world.spawn((ChildOf(root), BsnTag { value: 88 }));
                Ok(())
            },
        )
        .expect("expose custom scene");

    let bsn_handle = app
        .expose_mod_scene_bsn(
            "bsn_test::bsn_prop",
            bsn! {
                #BsnPropRoot
                Children [
                    #BsnPropChild
                ]
            },
        )
        .expect("expose bsn! scene");

    // Pre-resolve a `ScenePatch` directly into `Assets<ScenePatch>` and expose it via handle.
    let patch_handle = {
        let world = app.world_mut();
        let mut patch = {
            let server = world.resource::<AssetServer>();
            ScenePatch::load(
                server,
                bsn! {
                    #PatchedRoot
                    Children [
                        #PatchedLeaf
                    ]
                },
            )
        };
        world.resource_scope(
            |world, patches: bevy_ecs::prelude::Mut<'_, Assets<ScenePatch>>| {
                let server = world.resource::<AssetServer>();
                patch.resolve(server, &patches).expect("resolve patch");
            },
        );
        world.resource_mut::<Assets<ScenePatch>>().add(patch)
    };
    let exposed_patch_handle = app
        .expose_mod_scene_patch("bsn_test::pre_resolved_patch", patch_handle)
        .expect("expose patch handle");

    // Deterministic UUID resolution matches `HostSceneRegistry` lookup.
    let reg = app.world().resource::<HostSceneRegistry>().clone();
    let scene_type = reg.scene_type_handle().expect("ScenePatch handle");
    assert_eq!(
        custom_handle,
        FfiAssetHandle::from_uuid(
            scene_type,
            ComponentUuid::from_canonical_name("bsn_test::custom_tower")
        )
    );
    assert_eq!(reg.len(), 3);
    assert!(reg.contains("bsn_test::bsn_prop"));
    assert_eq!(
        app.resolve_mod_scene("bsn_test::pre_resolved_patch"),
        Some(exposed_patch_handle)
    );
    assert!(
        app.world()
            .resource::<ModRegistry>()
            .get_by_name(SCENE_PATCH_ALIAS)
            .is_some()
    );
    assert!(
        app.world()
            .resource::<ModRegistry>()
            .get_by_name(CANONICAL_READY_NAME)
            .is_some()
    );

    // Observe `Ready` events on the Bevy host AND via a guest FFI `BamEventKind::READY` observer.
    let host_ready_entities: Arc<Mutex<Vec<Entity>>> = Arc::new(Mutex::new(Vec::new()));
    let host_ready_log = Arc::clone(&host_ready_entities);
    app.add_observer(move |ready: On<Ready>| {
        host_ready_log.lock().unwrap().push(ready.entity);
    });

    let guest_ready_entities: Arc<Mutex<Vec<FfiEntity>>> = Arc::new(Mutex::new(Vec::new()));

    unsafe extern "C" fn guest_ready_cb(
        target: FfiEntity,
        _event_data: *const c_void,
        _cmds: *mut bevy_abi_mod_sys::BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        let log = unsafe { &*(user_data as *const Mutex<Vec<FfiEntity>>) };
        log.lock().unwrap().push(target);
        FfiResult::OK
    }

    // Parent entity owned by `ModId(9)` so child scene spawn inherits `ModScoped(ModId(9))`.
    let parent_entity = app
        .world_mut()
        .spawn((Name::new("SceneAnchor"), ModScoped(ModId(9))))
        .id();

    #[derive(Default)]
    struct SpawnState {
        custom_handle: FfiAssetHandle,
        bsn_handle: FfiAssetHandle,
        patch_handle: FfiAssetHandle,
        parent: FfiEntity,
        spawned_custom: FfiEntity,
        spawned_bsn: FfiEntity,
        spawned_bsn_second: FfiEntity,
        spawned_patch: FfiEntity,
        unknown_err: FfiResult,
        invalid_kind_err: FfiResult,
        guest_ready_log: *const Mutex<Vec<FfiEntity>>,
        tick: u32,
    }

    // SAFETY: `guest_ready_log` points to an `Arc<Mutex<Vec<FfiEntity>>>` held alive across the test.
    unsafe impl Send for SpawnState {}
    // SAFETY: Protected by outer `Mutex<SpawnState>`.
    unsafe impl Sync for SpawnState {}

    let state = Arc::new(Mutex::new(SpawnState {
        custom_handle,
        bsn_handle,
        patch_handle: exposed_patch_handle,
        parent: FfiEntity::from_bits(parent_entity.to_bits()),
        guest_ready_log: Arc::as_ptr(&guest_ready_entities),
        ..Default::default()
    }));

    unsafe extern "C" fn spawn_scenes_sys(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let mut st = unsafe { &*(user_data as *const Mutex<SpawnState>) }
            .lock()
            .unwrap();
        let mut cmds = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_commands(sys_ctx, &mut cmds) },
            FfiResult::OK
        );

        match st.tick {
            0 => {
                // Frame 1: Register the guest `Ready` FFI observer before spawning scenes.
                let desc = BamObserverDesc::new_lifecycle(
                    BamEventKind::READY,
                    core::ptr::null(),
                    0,
                    guest_ready_cb,
                    st.guest_ready_log.cast_mut().cast::<c_void>(),
                );
                assert_eq!(
                    unsafe { host_cmd_observe(cmds, &desc, core::ptr::null_mut()) },
                    FfiResult::OK
                );
            }
            1 => {
                // Frame 2: Spawn custom, `bsn!` (twice to verify cache reuse), and `ScenePatch` scenes.
                let mut custom_out = FfiEntity::NULL;
                let custom_h = st.custom_handle;
                let parent = st.parent;
                assert_eq!(
                    unsafe { host_cmd_spawn_scene(cmds, &custom_h, parent, &mut custom_out) },
                    FfiResult::OK
                );
                assert_ne!(custom_out, FfiEntity::NULL);
                st.spawned_custom = custom_out;

                let mut bsn_out = FfiEntity::NULL;
                let bsn_h = st.bsn_handle;
                assert_eq!(
                    unsafe { host_cmd_spawn_scene(cmds, &bsn_h, FfiEntity::NULL, &mut bsn_out) },
                    FfiResult::OK
                );
                assert_ne!(bsn_out, FfiEntity::NULL);
                st.spawned_bsn = bsn_out;

                let mut bsn_out_2 = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_scene(cmds, &bsn_h, FfiEntity::NULL, &mut bsn_out_2) },
                    FfiResult::OK
                );
                assert_ne!(bsn_out_2, FfiEntity::NULL);
                assert_ne!(bsn_out_2, bsn_out);
                st.spawned_bsn_second = bsn_out_2;

                let mut patch_out = FfiEntity::NULL;
                let patch_h = st.patch_handle;
                assert_eq!(
                    unsafe {
                        host_cmd_spawn_scene(cmds, &patch_h, FfiEntity::NULL, &mut patch_out)
                    },
                    FfiResult::OK
                );
                assert_ne!(patch_out, FfiEntity::NULL);
                st.spawned_patch = patch_out;

                // Error checks: unknown UUID and non-UUID index-backed asset handle.
                let unknown_h = FfiAssetHandle::from_uuid(
                    custom_h.asset_type,
                    ComponentUuid::from_canonical_name("bsn_test::does_not_exist"),
                );
                let mut dummy_out = FfiEntity::NULL;
                st.unknown_err = unsafe {
                    host_cmd_spawn_scene(cmds, &unknown_h, FfiEntity::NULL, &mut dummy_out)
                };
                let index_h = FfiAssetHandle::from_index_bits(custom_h.asset_type, 1);
                st.invalid_kind_err = unsafe {
                    host_cmd_spawn_scene(cmds, &index_h, FfiEntity::NULL, &mut dummy_out)
                };
            }
            _ => {}
        }
        st.tick += 1;
        FfiResult::OK
    }

    let mod_reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "bsn_spawner",
        vec![BamQuerySpecOwned::new(vec![])],
        spawn_scenes_sys,
        Arc::as_ptr(&state) as *mut c_void,
        mod_reg,
    );
    app.add_systems(Update, sys);

    // Frame 1 installs the guest `Ready` observer; Frame 2 spawns the scenes.
    app.update();
    app.update();

    let st = state.lock().unwrap();
    assert_eq!(st.unknown_err, FfiResult::NOT_FOUND);
    assert_eq!(st.invalid_kind_err, FfiResult::INVALID_ARGUMENT);

    let world = app.world();
    let custom_root = Entity::from_bits(st.spawned_custom.to_bits());
    let bsn_root = Entity::from_bits(st.spawned_bsn.to_bits());
    let bsn_root_2 = Entity::from_bits(st.spawned_bsn_second.to_bits());
    let patch_root = Entity::from_bits(st.spawned_patch.to_bits());

    // Custom scene root attached under `parent_entity` and inherited `ModScoped(ModId(9))`.
    assert_eq!(
        world.get::<ChildOf>(custom_root),
        Some(&ChildOf(parent_entity))
    );
    assert_eq!(
        world.get::<ModScoped>(custom_root),
        Some(&ModScoped(ModId(9)))
    );
    assert_eq!(
        world.get::<BsnTag>(custom_root),
        Some(&BsnTag { value: 77 })
    );
    let custom_children = world
        .get::<Children>(custom_root)
        .expect("custom scene children");
    assert_eq!(custom_children.len(), 1);
    assert_eq!(
        world.get::<BsnTag>(custom_children[0]),
        Some(&BsnTag { value: 88 })
    );

    // Both `bsn!` scene spawns applied root Name & child Name hierarchy from cached `ResolvedSceneRoot`.
    for root in [bsn_root, bsn_root_2] {
        assert_eq!(
            world.get::<Name>(root).map(Name::as_str),
            Some("BsnPropRoot")
        );
        let children = world.get::<Children>(root).expect("bsn! root has Children");
        assert_eq!(children.len(), 1);
        assert_eq!(
            world.get::<Name>(children[0]).map(Name::as_str),
            Some("BsnPropChild")
        );
    }

    // Pre-resolved `ScenePatch` applied root Name & leaf Name hierarchy.
    assert_eq!(
        world.get::<Name>(patch_root).map(Name::as_str),
        Some("PatchedRoot")
    );
    let patch_children = world
        .get::<Children>(patch_root)
        .expect("patch root has Children");
    assert_eq!(patch_children.len(), 1);
    assert_eq!(
        world.get::<Name>(patch_children[0]).map(Name::as_str),
        Some("PatchedLeaf")
    );

    let bsn_child = world.get::<Children>(bsn_root).unwrap()[0];
    let bsn_child_2 = world.get::<Children>(bsn_root_2).unwrap()[0];

    // Verify both Bevy `On<Ready>` AND the guest FFI `BamEventKind::READY` observer fired!
    let host_ready = host_ready_entities.lock().unwrap();
    let guest_ready = guest_ready_entities.lock().unwrap();
    for expected in [
        custom_root,
        bsn_root,
        bsn_child,
        bsn_root_2,
        bsn_child_2,
        patch_root,
        patch_children[0],
    ] {
        assert!(
            host_ready.contains(&expected),
            "host missed Ready for {expected:?}"
        );
        assert!(
            guest_ready.contains(&FfiEntity::from_bits(expected.to_bits())),
            "guest missed Ready for {expected:?}"
        );
    }
}

#[test]
fn bsn_ready_name_constant_matches_type_name() {
    assert_eq!(CANONICAL_READY_NAME, core::any::type_name::<Ready>());
}
