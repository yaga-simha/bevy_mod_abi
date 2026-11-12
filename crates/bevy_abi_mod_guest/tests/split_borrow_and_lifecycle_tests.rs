//! Split-borrow, multi-query nested iteration, and per-World `GuestPlugin` / `export_mod!`
//! lifecycle isolation tests (`L-02`, `L-08`, `G-02`).

mod common;

use core::ffi::CStr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use bevy_abi_mod_guest::prelude::*;
use common::{MockChunkData, MockHostWorld, RecordedCommand};

#[cfg(not(feature = "derive"))]
macro_rules! impl_pod_no_derive {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: Plain `#[repr(C)]` test struct with fixed-width POD fields.
            unsafe impl RawInsertable for $ty {}
            // SAFETY: Plain `#[repr(C)]` test struct with fixed-width POD fields.
            unsafe impl AbiPod for $ty {}
        )*
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct Pos {
    x: f32,
    y: f32,
}

impl GuestComponent for Pos {
    const NAME: &'static CStr = c"sim::Pos";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct Vel {
    dx: f32,
    dy: f32,
}

impl GuestComponent for Vel {
    const NAME: &'static CStr = c"sim::Vel";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct Accel {
    ax: f32,
    ay: f32,
}

impl GuestComponent for Accel {
    const NAME: &'static CStr = c"sim::Accel";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct LocalCounter {
    ticks: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct WrongCounter {
    ticks: u32,
}

#[cfg(not(feature = "derive"))]
impl_pod_no_derive!(Pos, Vel, Accel, LocalCounter, WrongCounter);

impl GuestComponent for LocalCounter {
    // Unqualified name: must auto-prefix with `<manifest_name>::LocalCounter` (`C-10`)!
    const NAME: &'static CStr = c"LocalCounter";
}

impl GuestComponent for WrongCounter {
    // Identical `u32` layout, different unqualified name: `C-13` must still reject!
    const NAME: &'static CStr = c"WrongCounter";
}

static SHUTDOWN_EVENTS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static NEXT_WORLD_TAG: AtomicUsize = AtomicUsize::new(1);

struct IsolatedPlugin {
    world_tag: u32,
    local_ticks: Arc<AtomicUsize>,
}

impl Default for IsolatedPlugin {
    fn default() -> Self {
        let tag = NEXT_WORLD_TAG.fetch_add(1, Ordering::SeqCst) as u32;
        Self {
            world_tag: tag,
            local_ticks: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl GuestPlugin for IsolatedPlugin {
    const NAME: &'static CStr = c"isolated_plugin";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        let counter_h = match app.register_component::<LocalCounter>() {
            Ok(h) => h,
            Err(e) => return e,
        };
        assert_eq!(app.resolve_component::<LocalCounter>(), Ok(counter_h));
        let ticks = Arc::clone(&self.local_ticks);
        let tag = self.world_tag;

        app.system(c"tick_world")
            .query(|q| q.write::<LocalCounter>())
            .build(move |ctx| {
                let run_idx = ticks.fetch_add(1, Ordering::SeqCst);
                let (iter, cmds) = ctx.iter_and_commands();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    // `C-10` + `C-13`: auto-prefixed `LocalCounter` succeeds on both `Chunk` and
                    // `ChunkSlice` `component*` views, while `WrongCounter` (same layout) returns `None`!
                    assert!(chunk.component::<LocalCounter>(0).is_some());
                    assert!(chunk.component::<WrongCounter>(0).is_none());
                    assert!(
                        chunk
                            .readonly_slice(0)
                            .as_component_slice::<LocalCounter>()
                            .is_some()
                    );
                    assert!(
                        chunk
                            .readonly_slice(0)
                            .as_component_slice::<WrongCounter>()
                            .is_none()
                    );
                    {
                        let mut mut_slice = chunk.mutable_slice(0);
                        assert!(mut_slice.as_component_slice_mut::<LocalCounter>().is_some());
                        assert!(mut_slice.as_component_slice_mut::<WrongCounter>().is_none());
                    }

                    let (entities, counters) = chunk
                        .entities_and_component_mut::<LocalCounter>(0)
                        .expect("entities + mutable auto-prefixed LocalCounter");
                    for i in 0..counters.len() {
                        counters[i].ticks += tag;
                        if run_idx == 0 && i == 0 {
                            assert_eq!(
                                cmds.insert_component::<LocalCounter>(
                                    entities[i],
                                    &LocalCounter { ticks: 777 }
                                ),
                                FfiResult::OK
                            );
                            // Dynamic guest components have `has_default == false` on the host, so
                            // resolving `isolated_plugin::LocalCounter` succeeds and reaches
                            // `cmd_insert_default` (`PERMISSION_DENIED`), whereas an unregistered
                            // unqualified component fails resolution with `NOT_FOUND`.
                            assert_eq!(
                                cmds.insert_component_default::<LocalCounter>(entities[i]),
                                FfiResult::PERMISSION_DENIED
                            );
                            assert_eq!(
                                cmds.insert_component_default::<WrongCounter>(entities[i]),
                                FfiResult::NOT_FOUND
                            );
                            assert_eq!(
                                cmds.remove_component::<LocalCounter>(entities[i]),
                                FfiResult::OK
                            );
                            assert_eq!(
                                cmds.remove_component::<WrongCounter>(entities[i]),
                                FfiResult::NOT_FOUND
                            );
                            assert_eq!(
                                cmds.insert_component::<WrongCounter>(
                                    entities[i],
                                    &WrongCounter { ticks: 1 }
                                ),
                                FfiResult::NOT_FOUND
                            );
                        }
                        if counters[i].ticks > 100 {
                            let _ = cmds.despawn(entities[i]);
                        }
                    }
                }
            })
    }

    fn on_shutdown(&mut self) {
        let total = self.local_ticks.load(Ordering::SeqCst) as u32;
        SHUTDOWN_EVENTS
            .lock()
            .unwrap()
            .push(self.world_tag * 1000 + total);
    }
}

// Export real ABI v0.2 C entry points in this integration test binary (`L-02`, `L-08`).
export_mod!(plugin: IsolatedPlugin);

#[test]
fn export_mod_manifest_and_multi_world_instance_isolation() {
    SHUTDOWN_EVENTS.lock().unwrap().clear();
    NEXT_WORLD_TAG.store(1, Ordering::SeqCst);

    // 1. Validate `bevy_abi_mod_manifest()` metadata (`L-08`).
    // SAFETY: `bevy_abi_mod_manifest` is side-effect free and returns a valid static `BamManifest`.
    let manifest_ptr = unsafe { bevy_abi_mod_manifest() };
    assert!(!manifest_ptr.is_null());
    // SAFETY: `manifest_ptr` is non-null and points to static `BamManifest`.
    let manifest = unsafe { &*manifest_ptr };
    assert_eq!(manifest.validate(), FfiResult::OK);
    // SAFETY: `manifest.name` and `manifest.version` are NUL-terminated static C strings.
    let name_cstr = unsafe { CStr::from_ptr(manifest.name) };
    // SAFETY: `manifest.version` is a NUL-terminated static C string.
    let ver_cstr = unsafe { CStr::from_ptr(manifest.version) };
    assert_eq!(name_cstr, c"isolated_plugin");
    assert_eq!(ver_cstr, c"0.2.0");

    // 2. Load the SAME exported `.cdylib` entry points into TWO independent `MockHostWorld`s simultaneously (`L-02`).
    let mut world_a = MockHostWorld::new();
    let mut world_b = MockHostWorld::new();

    let mut inst_a = core::ptr::null_mut();
    let mut inst_b = core::ptr::null_mut();

    // SAFETY: Valid mock host pointers for World A.
    let status_a =
        unsafe { bevy_abi_mod_init(world_a.api_ptr(), world_a.raw_ptr().cast(), &mut inst_a) };
    assert_eq!(status_a, FfiResult::OK);
    // SAFETY: Valid mock host pointers for World B.
    let status_b =
        unsafe { bevy_abi_mod_init(world_b.api_ptr(), world_b.raw_ptr().cast(), &mut inst_b) };
    assert_eq!(status_b, FfiResult::OK);
    assert!(!inst_a.is_null());
    assert!(!inst_b.is_null());
    assert_ne!(inst_a, inst_b);

    // Verify auto-qualified dynamic component name (`isolated_plugin::LocalCounter`, `C-10`):
    let comp_a = world_a.name_to_handle["isolated_plugin::LocalCounter"];
    let comp_b = world_b.name_to_handle["isolated_plugin::LocalCounter"];

    let mut chunk_a = MockChunkData::new(vec![FfiEntity::from_bits(1)]);
    chunk_a.insert_dense(comp_a, &[LocalCounter { ticks: 0 }]);
    world_a.chunks.push(chunk_a);

    let mut chunk_b = MockChunkData::new(vec![FfiEntity::from_bits(2)]);
    chunk_b.insert_dense(comp_b, &[LocalCounter { ticks: 10 }]);
    world_b.chunks.push(chunk_b);

    // Run World A 3 times (tag = 1) and World B 1 time (tag = 2):
    for _ in 0..3 {
        assert_eq!(world_a.run_system(0), FfiResult::OK);
    }
    assert_eq!(world_b.run_system(0), FfiResult::OK);

    assert_eq!(
        world_a.commands,
        vec![
            RecordedCommand::Insert {
                entity: FfiEntity::from_bits(1),
                component: comp_a,
                byte_len: core::mem::size_of::<LocalCounter>(),
            },
            RecordedCommand::Remove {
                entity: FfiEntity::from_bits(1),
                component: comp_a,
            },
        ]
    );
    assert_eq!(
        world_b.commands,
        vec![
            RecordedCommand::Insert {
                entity: FfiEntity::from_bits(2),
                component: comp_b,
                byte_len: core::mem::size_of::<LocalCounter>(),
            },
            RecordedCommand::Remove {
                entity: FfiEntity::from_bits(2),
                component: comp_b,
            },
        ]
    );

    // Shut down World A FIRST while World B remains live and continues ticking!
    // SAFETY: `inst_a` was returned by `bevy_abi_mod_init` for World A.
    unsafe { bevy_abi_mod_shutdown(inst_a) };
    assert_eq!(*SHUTDOWN_EVENTS.lock().unwrap(), vec![1003]);

    // World B continues running cleanly with zero interference from World A's teardown:
    assert_eq!(world_b.run_system(0), FfiResult::OK);
    assert_eq!(
        world_b.chunks[0].read_dense::<LocalCounter>(comp_b),
        vec![LocalCounter { ticks: 14 }]
    );

    // SAFETY: `inst_b` was returned by `bevy_abi_mod_init` for World B.
    unsafe { bevy_abi_mod_shutdown(inst_b) };
    assert_eq!(*SHUTDOWN_EVENTS.lock().unwrap(), vec![1003, 2002]);
}

#[test]
fn split_borrows_and_multi_query_nested_reset_enforce_disjointness() {
    let mut world = MockHostWorld::new();
    let pos_h = world.expose_component::<Pos>("sim::Pos", Pos::UUID, 0, false);
    let vel_h = world.expose_component::<Vel>("sim::Vel", Vel::UUID, 0, false);
    let acc_h = world.expose_component::<Accel>("sim::Accel", Accel::UUID, 0, false);

    let mut chunk0 = MockChunkData::new(vec![FfiEntity::from_bits(50), FfiEntity::from_bits(51)]);
    chunk0.insert_dense(pos_h, &[Pos { x: 0.0, y: 0.0 }, Pos { x: 10.0, y: 10.0 }]);
    chunk0.insert_dense(vel_h, &[Vel { dx: 1.0, dy: 2.0 }, Vel { dx: 3.0, dy: 4.0 }]);
    chunk0.insert_dense(
        acc_h,
        &[Accel { ax: 0.5, ay: 0.5 }, Accel { ax: 1.0, ay: 1.0 }],
    );
    world.chunks.push(chunk0);

    let inst = world
        .load_fn(c"split_mod", move |app| {
            app.system(c"split_and_multi_query")
                .query(|q| {
                    q.write::<Pos>()
                        .write::<Vel>()
                        .read::<Accel>()
                        .write::<Pos>()
                })
                .query(|q| q.read::<Accel>())
                .build(move |ctx| {
                    assert_eq!(ctx.query_count(), 2);
                    assert!(ctx.queries_2_mut(0, 0).is_none());
                    assert!(ctx.queries_2_mut(0, 2).is_none());

                    let (q0, q1, cmds) = ctx
                        .queries_2_and_commands(0, 1)
                        .expect("disjoint queries + commands");

                    let mut pair_count = 0usize;
                    while let Some(mut c0) = q0.next_chunk_view() {
                        // Same column index (`0, 0`) or duplicate component handle (`0, 3` both `Pos`) MUST return `None`!
                        assert!(c0.write_2::<Pos, Pos>(0, 0).is_none());
                        assert!(c0.write_2::<Pos, Pos>(0, 3).is_none());
                        assert!(c0.read_write::<Pos, Pos>(0, 3).is_none());

                        // Disjoint `component_write_2` (`Pos` col 0 + `Vel` col 1):
                        {
                            let (pos, vel) = c0
                                .component_write_2::<Pos, Vel>(0, 1)
                                .expect("disjoint Pos + Vel write");
                            for i in 0..pos.len() {
                                pos[i].x += vel[i].dx;
                                pos[i].y += vel[i].dy;
                            }
                        }

                        // Disjoint `entities_and_component_read_write` (`Accel` col 2 -> `Vel` col 1):
                        {
                            let (entities, acc, vel) = c0
                                .entities_and_component_read_write::<Accel, Vel>(2, 1)
                                .expect("entities + Accel + Vel");
                            assert_eq!(entities.len(), 2);
                            for i in 0..vel.len() {
                                vel[i].dx += acc[i].ax;
                                vel[i].dy += acc[i].ay;
                            }
                        }

                        // Nested inner query with `reset()` for N x M pairs:
                        assert_eq!(q1.reset(), FfiResult::OK);
                        while let Some(c1) = q1.next_chunk_view() {
                            let acc = c1.component::<Accel>(0).unwrap();
                            pair_count += c0.len() * acc.len();
                        }
                    }
                    assert_eq!(pair_count, 4);
                    let _ = cmds.spawn_empty().unwrap();
                })
        })
        .expect("load split_mod");

    assert_eq!(world.run_system(0), FfiResult::OK);
    assert_eq!(
        world.chunks[0].read_dense::<Pos>(pos_h),
        vec![Pos { x: 1.0, y: 2.0 }, Pos { x: 13.0, y: 14.0 }]
    );
    assert_eq!(
        world.chunks[0].read_dense::<Vel>(vel_h),
        vec![Vel { dx: 1.5, dy: 2.5 }, Vel { dx: 4.0, dy: 5.0 }]
    );
    assert_eq!(
        world.commands,
        vec![RecordedCommand::SpawnEmpty(FfiEntity::from_bits(100))]
    );

    world.unload_instance(inst);
}

#[test]
fn regression_c10_c13_unqualified_guest_component_sparse_resource_and_cross_mod_rejection() {
    use bevy_abi_mod_guest::raw::BAM_COMPONENT_FLAG_SPARSE_SET;

    let mut world = MockHostWorld::new();
    // Another mod registered `other_mod::LocalCounter` with the exact same `u32` layout:
    let other_mod_uuid = guest_component_uuid::<LocalCounter>(
        "other_mod::LocalCounter",
        LocalCounter::FIELD_SCHEMA_HASH,
    );
    let other_h =
        world.expose_component::<LocalCounter>("other_mod::LocalCounter", other_mod_uuid, 0, false);

    let inst = world
        .load_fn(c"my_mod", move |app| {
            let sparse_h = app
                .register_component_with_flags::<LocalCounter>(BAM_COMPONENT_FLAG_SPARSE_SET)
                .expect("register sparse LocalCounter");
            let res_h = app
                .register_component::<WrongCounter>()
                .expect("register WrongCounter resource");
            assert_ne!(sparse_h, other_h);
            assert_ne!(res_h, other_h);

            app.system(c"check_unqualified_views")
                .query(|q| q.write::<LocalCounter>().read_handle(other_h))
                .resource_write::<WrongCounter>()
                .build(|ctx| {
                    // System-level resource (`my_mod::WrongCounter`): succeeds for `WrongCounter`,
                    // rejects `LocalCounter`!
                    assert!(ctx.component_resource::<LocalCounter>(0).is_none());
                    assert!(ctx.component_resource_mut::<LocalCounter>(0).is_none());
                    {
                        let r = ctx
                            .component_resource_mut::<WrongCounter>(0)
                            .expect("qualified WrongCounter resource");
                        r.ticks += 50;
                    }
                    assert_eq!(
                        ctx.component_resource::<WrongCounter>(0),
                        Some(&WrongCounter { ticks: 55 })
                    );

                    let iter = ctx.iter();
                    while let Some(mut chunk) = iter.next_chunk_view() {
                        // Sparse rows (`my_mod::LocalCounter` at col 0):
                        assert!(chunk.component_rows::<WrongCounter>(0).is_none());
                        {
                            let mut rows = chunk
                                .component_rows_mut::<LocalCounter>(0)
                                .expect("auto-prefixed sparse rows");
                            rows.for_each_mut(|_, c| c.ticks += 10);
                        }
                        let ro_rows = chunk
                            .component_rows::<LocalCounter>(0)
                            .expect("auto-prefixed readonly sparse rows");
                        assert_eq!(ro_rows[0], LocalCounter { ticks: 17 });

                        // Cross-mod rejection: col 1 is `other_mod::LocalCounter`, which MUST NOT
                        // match `my_mod`'s unqualified `LocalCounter`!
                        assert!(chunk.component::<LocalCounter>(1).is_none());
                        assert!(
                            chunk
                                .readonly_slice(1)
                                .as_component_slice::<LocalCounter>()
                                .is_none()
                        );
                    }
                })
        })
        .expect("load my_mod");

    let my_sparse_h = world.name_to_handle["my_mod::LocalCounter"];
    let my_res_h = world.name_to_handle["my_mod::WrongCounter"];
    let my_res_uuid = guest_component_uuid::<WrongCounter>(
        "my_mod::WrongCounter",
        WrongCounter::FIELD_SCHEMA_HASH,
    );
    assert_eq!(
        world.insert_resource(
            "my_mod::WrongCounter",
            my_res_uuid,
            WrongCounter { ticks: 5 }
        ),
        my_res_h
    );

    let mut chunk = MockChunkData::new(vec![FfiEntity::from_bits(1)]);
    chunk.insert_sparse(my_sparse_h, vec![Some(LocalCounter { ticks: 7 })]);
    chunk.insert_dense(other_h, &[LocalCounter { ticks: 99 }]);
    world.chunks.push(chunk);

    assert_eq!(world.run_system(0), FfiResult::OK);
    assert_eq!(
        world.read_resource::<WrongCounter>(my_res_h),
        WrongCounter { ticks: 55 }
    );
    assert_eq!(
        world.chunks[0].read_sparse::<LocalCounter>(my_sparse_h),
        vec![Some(LocalCounter { ticks: 17 })]
    );

    world.unload_instance(inst);
}
