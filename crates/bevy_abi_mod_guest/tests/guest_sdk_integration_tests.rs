//! Integration tests for `bevy_abi_mod_guest` (`ABI 0.2.0`):
//! - Single `Host` table validation, `"bam.v0.*"` custom symbols, and `VTableHeader` checks.
//! - Unified `SystemBuilder` (`query`, `resource_read`/`resource_write`, `schedule`, `after`/`before`, `main_thread`, `run_if`).
//! - Lock-free re-entrant closure guard (`reentrant_trampoline_conflict`).
//! - Column UUID/size/align/mode validation (`column_uuid_mismatch_returns_none`).
//! - Indirect sparse-set row pointers (`Chunk::rows`, `Chunk::rows_mut`).
//! - Untracked mutable column slices + `mark_changed_range` / `mark_changed`.

#![deny(unsafe_code)]

mod common;

use core::ffi::{CStr, c_void};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use bevy_abi_mod_guest::{
    prelude::*,
    raw::{
        self, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
        BAM_SYSTEM_FLAG_MAIN_THREAD,
    },
};
use common::{MockChunkData, MockHostWorld};

#[cfg(not(feature = "derive"))]
macro_rules! impl_pod_no_derive {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: Plain `#[repr(C)]` test struct with fixed-width POD fields.
            #[allow(unsafe_code)]
            unsafe impl RawInsertable for $ty {}
            // SAFETY: Plain `#[repr(C)]` test struct with fixed-width POD fields.
            #[allow(unsafe_code)]
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
    const NAME: &'static CStr = c"game::Pos";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct Vel {
    dx: f32,
    dy: f32,
}

impl GuestComponent for Vel {
    const NAME: &'static CStr = c"game::Vel";
}

/// Same C layout as `Pos` (`2 * f32`), but distinct `GuestComponent::UUID` (`test).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct ImposterPos {
    x: f32,
    y: f32,
}

impl GuestComponent for ImposterPos {
    const NAME: &'static CStr = c"game::ImposterPos";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct Tag;

impl GuestComponent for Tag {
    const NAME: &'static CStr = c"game::Tag";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct Dead;

impl GuestComponent for Dead {
    const NAME: &'static CStr = c"game::Dead";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct Shield {
    hp: f32,
}

impl GuestComponent for Shield {
    const NAME: &'static CStr = c"game::Shield";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct SimTime {
    dt: f32,
}

impl GuestComponent for SimTime {
    const NAME: &'static CStr = c"game::SimTime";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct Score {
    points: u32,
}

impl GuestComponent for Score {
    const NAME: &'static CStr = c"game::Score";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct SparseBuff {
    multiplier: f32,
}

impl GuestComponent for SparseBuff {
    const NAME: &'static CStr = c"game::SparseBuff";
    const FLAGS: u32 = BAM_COMPONENT_FLAG_SPARSE_SET;
}

#[cfg(not(feature = "derive"))]
impl_pod_no_derive!(
    Pos,
    Vel,
    ImposterPos,
    Tag,
    Dead,
    Shield,
    SimTime,
    Score,
    SparseBuff
);

#[derive(Clone, Copy)]
struct SendWorldPtr(*const MockHostWorld);
// SAFETY: Single-threaded test fixture preserving pointer provenance under Miri strict provenance.
#[allow(unsafe_code)]
unsafe impl Send for SendWorldPtr {}

#[test]
fn reentrant_trampoline_conflict() {
    let mut world = MockHostWorld::new();
    let _pos_h = world.expose_component::<Pos>("game::Pos", Pos::UUID, 0, false);

    // Register a system whose closure re-entrantly invokes its own registered C trampoline.
    // The lock-free AtomicBool guard must return `ACCESS_CONFLICT` without UB or deadlock,
    // and must release cleanly even across panics.
    let raw_world_ptr = SendWorldPtr(world.raw_ptr().cast_const());
    let inner_result = Arc::new(AtomicUsize::new(usize::MAX));
    let inner_result_clone = Arc::clone(&inner_result);

    let inst = world
        .load_fn(c"reentrant_mod", move |app| {
            let status_a = app
                .system(c"reentrant_sys")
                .query(|q| q.read::<Pos>())
                .run_if({
                    let entered = Arc::new(AtomicUsize::new(0));
                    move |ctx| {
                        // Verify `RunConditionContext` works and check non-reentrant execution.
                        assert_eq!(ctx.host_api().abi_version, ABI_VERSION);
                        entered.fetch_add(1, Ordering::Relaxed) < 10
                    }
                })
                .build(move |ctx| {
                    let wrld = raw_world_ptr;
                    let sys_ctx = ctx.as_raw_ptr();
                    #[allow(unsafe_code)]
                    // SAFETY: Invoking registered C system trampoline re-entrantly to test the re-entrancy guard.
                    let nested = unsafe {
                        let host_world = &*wrld.0;
                        let sys = &host_world.systems[0];
                        (sys.callback)(sys_ctx, sys.user_data)
                    };
                    inner_result_clone.store(nested.0 as usize, Ordering::SeqCst);
                    FfiResult::OK
                });
            assert_eq!(status_a, FfiResult::OK);

            // Second system panics on first call; RAII ReleaseGuard must clear the active flag so
            // subsequent invocations are not permanently poisoned.
            let mut panicked_once = false;
            app.system(c"panic_recover_sys").build(move |_ctx| {
                if !panicked_once {
                    panicked_once = true;
                    panic!("simulated guest system panic");
                }
                FfiResult::OK
            })
        })
        .expect("load_fn succeeded");

    assert_eq!(world.run_system(0), FfiResult::OK);
    assert_eq!(
        inner_result.load(Ordering::SeqCst) as u32,
        FfiResult::ACCESS_CONFLICT.0
    );

    // Also test re-entrant `run_if` returning `FfiBool::FALSE`.
    let run_if_world_ptr = SendWorldPtr(world.raw_ptr().cast_const());
    let nested_run_if = Arc::new(AtomicUsize::new(99));
    let nested_run_if_clone = Arc::clone(&nested_run_if);
    let inst2 = world
        .load_fn(c"reentrant_run_if_mod", move |app| {
            app.system(c"cond_sys")
                .run_if(move |_ctx| {
                    let wrld = run_if_world_ptr;
                    #[allow(unsafe_code)]
                    // SAFETY: Invoking registered C `run_if` trampoline re-entrantly to test the re-entrancy guard.
                    let nested = unsafe {
                        let host_world = &*wrld.0;
                        let sys = &host_world.systems[2];
                        (sys.run_if.unwrap())(sys.run_if_user_data)
                    };
                    nested_run_if_clone.store(usize::from(nested.get()), Ordering::SeqCst);
                    true
                })
                .build(|_ctx| FfiResult::OK)
        })
        .expect("load second mod");

    assert_eq!(world.run_system(2), FfiResult::OK);
    assert_eq!(nested_run_if.load(Ordering::SeqCst), 0);

    // Panic containment + RAII guard release check on system 1:
    assert_eq!(world.run_system(1), FfiResult::PANIC_CAUGHT);
    assert_eq!(world.run_system(1), FfiResult::OK);

    world.unload_instance(inst2);
    world.unload_instance(inst);
}

#[test]
fn column_uuid_mismatch_returns_none() {
    let mut world = MockHostWorld::new();
    let pos_h = world.expose_component::<Pos>("game::Pos", Pos::UUID, 0, false);
    let vel_h = world.expose_component::<Vel>("game::Vel", Vel::UUID, 0, false);
    let immut_h = world.expose_component::<Shield>(
        "game::Shield",
        Shield::UUID,
        BAM_COMPONENT_FLAG_IMMUTABLE,
        false,
    );
    let res_h = world.insert_resource("game::SimTime", SimTime::UUID, SimTime { dt: 0.25 });

    let mut chunk = MockChunkData::new(vec![FfiEntity::from_bits(1), FfiEntity::from_bits(2)]);
    chunk.insert_dense(pos_h, &[Pos { x: 1.0, y: 2.0 }, Pos { x: 3.0, y: 4.0 }]);
    chunk.insert_dense(vel_h, &[Vel { dx: 0.5, dy: 0.5 }, Vel { dx: 1.5, dy: 1.5 }]);
    chunk.insert_dense(immut_h, &[Shield { hp: 10.0 }, Shield { hp: 20.0 }]);
    world.chunks.push(chunk);

    let verified = Arc::new(AtomicUsize::new(0));
    let verified_clone = Arc::clone(&verified);

    let inst = world
        .load_fn(c"c13_mod", move |app| {
            app.system(c"check_c13")
                .query(|q| {
                    q.write::<Pos>()
                        .read::<Vel>()
                        .term(immut_h, AccessMode::WRITE)
                })
                .resource_read_handle(res_h)
                .build(move |ctx| {
                    // Resource UUID & size checks:
                    assert!(ctx.component_resource::<SimTime>(0).is_some());
                    assert!(ctx.component_resource::<Shield>(0).is_none()); // Same size (4B), wrong UUID!
                    assert!(ctx.resource::<[f32; 2]>(0).is_none()); // Wrong size (8B vs 4B)!
                    assert!(ctx.resource_mut::<SimTime>(0).is_none()); // Declared RESOURCE_READ, not WRITE!

                    let iter = ctx.iter();
                    while let Some(mut view) = iter.next_chunk_view() {
                        // 1. Exact layout match (`2 * f32`), WRONG `ComponentUuid` -> MUST return `None`!
                        assert!(view.component::<ImposterPos>(0).is_none());
                        assert!(view.component_mut::<ImposterPos>(0).is_none());
                        assert!(view.component_mut_untracked::<ImposterPos>(0).is_none());
                        assert!(
                            view.component_read_write::<Vel, ImposterPos>(1, 0)
                                .is_none()
                        );
                        assert!(
                            view.readonly_slice(0)
                                .as_component_slice::<ImposterPos>()
                                .is_none()
                        );

                        // 2. Wrong element size or alignment -> MUST return `None`!
                        assert!(view.column::<f32>(0).is_none());
                        assert!(view.column::<u64>(0).is_none());

                        // 3. Mutable borrow on READ-only column 1 -> MUST return `None`!
                        assert!(view.column_mut::<Vel>(1).is_none());

                        // 4. Mutable borrow on IMMUTABLE component column 2 -> MUST return `None`!
                        assert!(view.column_mut::<Shield>(2).is_none());
                        assert!(view.component::<Shield>(2).is_some());

                        // 5. Matching `GuestComponent` UUID & layout succeeds!
                        let (vel, pos) = view
                            .component_read_write::<Vel, Pos>(1, 0)
                            .expect("matching UUIDs succeed");
                        for i in 0..pos.len() {
                            pos[i].x += vel[i].dx;
                            pos[i].y += vel[i].dy;
                        }
                        verified_clone.fetch_add(1, Ordering::SeqCst);
                    }
                })
        })
        .expect("load c13_mod");

    assert_eq!(world.run_system(0), FfiResult::OK);
    assert_eq!(verified.load(Ordering::SeqCst), 1);
    assert_eq!(
        world.chunks[0].read_dense::<Pos>(pos_h),
        vec![Pos { x: 1.5, y: 2.5 }, Pos { x: 4.5, y: 5.5 }]
    );

    world.unload_instance(inst);
}

#[test]
fn unified_system_builder_compiles_queries_resources_ordering_and_flags() {
    let mut world = MockHostWorld::new();
    let pos_h = world.expose_component::<Pos>("game::Pos", Pos::UUID, 0, false);
    let vel_h = world.expose_component::<Vel>("game::Vel", Vel::UUID, 0, false);
    let _tag_h = world.expose_component::<Tag>("game::Tag", Tag::UUID, 0, false);
    let dead_h = world.expose_component::<Dead>("game::Dead", Dead::UUID, 0, false);
    let shield_h = world.expose_component::<Shield>("game::Shield", Shield::UUID, 0, false);
    let _time_h = world.insert_resource("game::SimTime", SimTime::UUID, SimTime { dt: 2.0 });
    let score_h = world.insert_resource("game::Score", Score::UUID, Score { points: 10 });

    // Chunk 0: matches (has Pos, Vel, Tag; lacks Dead; has optional Shield)
    let mut chunk0 = MockChunkData::new(vec![FfiEntity::from_bits(10)]);
    chunk0.insert_dense(pos_h, &[Pos { x: 1.0, y: 2.0 }]);
    chunk0.insert_dense(vel_h, &[Vel { dx: 3.0, dy: 4.0 }]);
    chunk0.insert_dense(world.name_to_handle["game::Tag"], &[Tag]);
    chunk0.insert_dense(shield_h, &[Shield { hp: 50.0 }]);
    world.chunks.push(chunk0);

    // Chunk 1: filtered out because it has `Dead` (`Without<Dead>`)
    let mut chunk1 = MockChunkData::new(vec![FfiEntity::from_bits(11)]);
    chunk1.insert_dense(pos_h, &[Pos { x: 99.0, y: 99.0 }]);
    chunk1.insert_dense(vel_h, &[Vel { dx: 1.0, dy: 1.0 }]);
    chunk1.insert_dense(world.name_to_handle["game::Tag"], &[Tag]);
    chunk1.insert_dense(dead_h, &[Dead]);
    world.chunks.push(chunk1);

    // Chunk 2: matches (has Pos, Vel, Tag; lacks optional Shield)
    let mut chunk2 = MockChunkData::new(vec![FfiEntity::from_bits(12)]);
    chunk2.insert_dense(pos_h, &[Pos { x: 5.0, y: 6.0 }]);
    chunk2.insert_dense(vel_h, &[Vel { dx: 1.0, dy: -1.0 }]);
    chunk2.insert_dense(world.name_to_handle["game::Tag"], &[Tag]);
    world.chunks.push(chunk2);

    let inst = world
        .load_fn(c"builder_mod", |app| {
            // Verify zero-arg `.run_if(|| bool)` as well as context `.run_if(|ctx| bool)` both infer!
            let s1 = app
                .system(c"move")
                .query(|q| {
                    q.write::<Pos>()
                        .read::<Vel>()
                        .with::<Tag>()
                        .without::<Dead>()
                        .changed::<Vel>()
                        .added::<Vel>()
                        .optional_read::<Shield>()
                })
                .resource_read::<SimTime>()
                .resource_write::<Score>()
                .schedule(ScheduleId::FIXED_UPDATE)
                .after(c"spawn")
                .before(c"render")
                .main_thread()
                .run_if_fn(|| true)
                .build(|ctx| -> Result<(), FfiResult> {
                    let dt = ctx
                        .component_resource::<SimTime>(0)
                        .ok_or(FfiResult::NOT_FOUND)?
                        .dt;
                    let mut processed = 0u32;
                    {
                        let iter = ctx.iter();
                        while let Some(mut chunk) = iter.next_chunk_view() {
                            let shield_bonus = chunk
                                .optional_component::<Shield>(6)
                                .map_or(0.0, |s| s[0].hp * 0.1);
                            let (vel, pos) = chunk
                                .component_read_write::<Vel, Pos>(1, 0)
                                .ok_or(FfiResult::TYPE_MISMATCH)?;
                            for i in 0..pos.len() {
                                pos[i].x += vel[i].dx * dt + shield_bonus;
                                pos[i].y += vel[i].dy * dt;
                                processed += 1;
                            }
                        }
                    }
                    ctx.component_resource_mut::<Score>(1)
                        .ok_or(FfiResult::NOT_FOUND)?
                        .points += processed;
                    Ok(())
                });
            assert_eq!(s1, FfiResult::OK);

            // Exceeding `MAX_QUERIES_PER_SYSTEM` (8) returns `CAPACITY_EXCEEDED`:
            let mut over_q = app.system(c"too_many_queries");
            for _ in 0..=MAX_QUERIES_PER_SYSTEM {
                over_q = over_q.query(|q| q.read::<Pos>());
            }
            assert_eq!(over_q.build(|_| {}), FfiResult::CAPACITY_EXCEEDED);

            // Unresolvable component propagates `NOT_FOUND` cleanly on `.build(...)`:
            #[repr(C)]
            #[derive(Clone, Copy)]
            #[cfg_attr(feature = "derive", derive(AbiPod))]
            struct Missing(u32);
            #[cfg(not(feature = "derive"))]
            impl_pod_no_derive!(Missing);
            impl GuestComponent for Missing {
                const NAME: &'static CStr = c"game::Missing";
            }
            assert_eq!(
                app.system(c"missing_comp")
                    .query(|q| q.read::<Missing>())
                    .build(|_| {}),
                FfiResult::NOT_FOUND
            );

            FfiResult::OK
        })
        .expect("load builder_mod");

    assert_eq!(world.systems.len(), 1);
    let reg = &world.systems[0];
    assert_eq!(reg.name, "move");
    assert_eq!(reg.schedule, ScheduleId::FIXED_UPDATE);
    assert_eq!(reg.flags, BAM_SYSTEM_FLAG_MAIN_THREAD);
    assert_eq!(reg.after, vec!["spawn"]);
    assert_eq!(reg.before, vec!["render"]);
    // 1 user query + 1 synthetic system-resource query = 2 descriptor queries
    assert_eq!(reg.queries.len(), 2);

    assert_eq!(world.run_system(0), FfiResult::OK);
    // Chunk 0: x = 1.0 + 3.0*2.0 + 5.0 = 12.0, y = 2.0 + 4.0*2.0 = 10.0
    assert_eq!(
        world.chunks[0].read_dense::<Pos>(pos_h),
        vec![Pos { x: 12.0, y: 10.0 }]
    );
    // Chunk 1 (`Dead`): untouched!
    assert_eq!(
        world.chunks[1].read_dense::<Pos>(pos_h),
        vec![Pos { x: 99.0, y: 99.0 }]
    );
    // Chunk 2 (no `Shield`): x = 5.0 + 1.0*2.0 + 0.0 = 7.0, y = 6.0 + (-1.0)*2.0 = 4.0
    assert_eq!(
        world.chunks[2].read_dense::<Pos>(pos_h),
        vec![Pos { x: 7.0, y: 4.0 }]
    );
    assert_eq!(world.read_resource::<Score>(score_h), Score { points: 12 });

    world.unload_instance(inst);
}

#[test]
fn sparse_rows_and_untracked_change_marking_work_end_to_end() {
    let mut world = MockHostWorld::new();
    let pos_h = world.expose_component::<Pos>("game::Pos", Pos::UUID, 0, false);
    let buff_h = world.expose_component::<SparseBuff>(
        "game::SparseBuff",
        SparseBuff::UUID,
        BAM_COMPONENT_FLAG_SPARSE_SET,
        false,
    );

    let mut chunk = MockChunkData::new(vec![
        FfiEntity::from_bits(1),
        FfiEntity::from_bits(2),
        FfiEntity::from_bits(3),
    ]);
    chunk.insert_dense(
        pos_h,
        &[
            Pos { x: 1.0, y: 1.0 },
            Pos { x: 2.0, y: 2.0 },
            Pos { x: 3.0, y: 3.0 },
        ],
    );
    chunk.insert_sparse(
        buff_h,
        vec![
            Some(SparseBuff { multiplier: 2.0 }),
            None,
            Some(SparseBuff { multiplier: 3.0 }),
        ],
    );
    world.chunks.push(chunk);

    let inst = world
        .load_fn(c"sparse_untracked_mod", |app| {
            app.system(c"sparse_and_untracked")
                .query(|q| q.write::<Pos>().optional_write::<SparseBuff>())
                .build(|ctx| {
                    let iter = ctx.iter();
                    while let Some(mut chunk) = iter.next_chunk_view() {
                        // Sparse columns cannot be borrowed as contiguous dense slices:
                        assert!(chunk.column::<SparseBuff>(1).is_none());
                        assert!(chunk.column_mut::<SparseBuff>(1).is_none());

                        // Read and mutate indirect sparse rows!
                        let multipliers: Vec<Option<f32>> = {
                            let ro_rows =
                                chunk.component_rows::<SparseBuff>(1).expect("sparse rows");
                            assert_eq!(ro_rows.len(), 3);
                            let present: Vec<f32> = ro_rows.iter().map(|b| b.multiplier).collect();
                            assert_eq!(present, vec![2.0, 3.0]);
                            (0..ro_rows.len())
                                .map(|i| ro_rows.get(i).map(|b| b.multiplier))
                                .collect()
                        };
                        assert_eq!(multipliers, vec![Some(2.0), None, Some(3.0)]);

                        {
                            let mut mut_rows = chunk
                                .component_rows_mut::<SparseBuff>(1)
                                .expect("mutable sparse rows");
                            mut_rows.for_each_mut(|_idx, buff| {
                                buff.multiplier += 1.0;
                            });
                        }

                        // Untracked dense mutation + selective `mark_changed`:
                        {
                            let pos = chunk
                                .component_mut_untracked::<Pos>(0)
                                .expect("untracked dense slice");
                            // Only modify row 2!
                            pos[2].x *= 10.0;
                        }
                        assert_eq!(chunk.mark_changed(0, 2), FfiResult::OK);
                        assert_eq!(chunk.mark_changed_range(0, 3, 1), FfiResult::OUT_OF_RANGE);
                    }
                })
        })
        .expect("load sparse_untracked_mod");

    assert_eq!(world.run_system(0), FfiResult::OK);

    // Whole-chunk change flag was NOT set because `component_mut_untracked` was used!
    assert!(!world.chunks[0].chunk_marked_changed.contains_key(&pos_h));
    // Only row 2 was marked changed via `mark_changed(0, 2)`!
    assert_eq!(world.chunks[0].marked_rows.get(&pos_h), Some(&vec![2]));
    assert_eq!(
        world.chunks[0].read_dense::<Pos>(pos_h)[2],
        Pos { x: 30.0, y: 3.0 }
    );
    assert_eq!(
        world.chunks[0].read_sparse::<SparseBuff>(buff_h),
        vec![
            Some(SparseBuff { multiplier: 3.0 }),
            None,
            Some(SparseBuff { multiplier: 4.0 }),
        ]
    );

    world.unload_instance(inst);
}

#[test]
fn host_handshake_custom_symbols_and_vtable_validation() {
    let mut world = MockHostWorld::new();
    world.last_error = String::from("synthetic diagnostic");

    static DUMMY_SYM: u32 = 0xCAFE_BABE;
    world.symbols.insert(
        String::from("bam.v0.physics_gravity"),
        (&raw const DUMMY_SYM).cast::<c_void>(),
    );
    world.symbols.insert(
        String::from("legacy_unprefixed"),
        (&raw const DUMMY_SYM).cast::<c_void>(),
    );

    // 1. Corrupt `struct_size` must fail with `INVALID_DESCRIPTOR` and set `*instance_out = null`.
    {
        let mut bad_api = world.api;
        bad_api.struct_size = (raw::BamHostApi::MIN_V0_2_SIZE - 1) as u32;
        let manifest = BamManifest::new(c"bad".as_ptr(), c"0.1.0".as_ptr());
        let mut out: *mut raw::BamInstance = core::ptr::dangling_mut();
        #[allow(unsafe_code)]
        // SAFETY: Passing stack `bad_api` to verify `struct_size < MIN_V0_2_SIZE` rejection.
        let status = unsafe {
            raw::init_guest_instance(
                &raw const bad_api,
                world.raw_ptr().cast::<raw::BamLoadCtx>(),
                &mut out,
                &manifest,
                |_, _| FfiResult::OK,
            )
        };
        assert_eq!(status, FfiResult::INVALID_VERSION);
        assert!(out.is_null());
    }

    // 2. Custom symbol prefix rule (`"bam.v0.*"`) & `last_error` diagnostic retrieval:
    let inst = world
        .load_fn(c"sym_mod", |app| {
            assert_eq!(app.last_error(), "synthetic diagnostic");
            let ok_sym = app
                .resolve_custom_symbol(c"bam.v0.physics_gravity")
                .expect("valid custom symbol");
            assert_eq!(ok_sym, (&raw const DUMMY_SYM).cast::<c_void>());
            assert_eq!(
                app.resolve_custom_symbol(c"legacy_unprefixed"),
                Err(FfiResult::INVALID_ARGUMENT)
            );
            assert_eq!(
                app.resolve_custom_symbol(c"bam.v0.nonexistent"),
                Err(FfiResult::NOT_FOUND)
            );
            FfiResult::OK
        })
        .expect("load sym_mod");

    world.unload_instance(inst);
}

#[cfg(feature = "derive")]
#[test]
fn derive_mod_component_vtable_header_roundtrip() {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, ModComponent)]
    #[mod_component(name = "game::Health")]
    struct Health {
        current: f32,
        max: f32,
    }

    let mut world = MockHostWorld::new();
    for entry in Health::symbols() {
        world.symbols.insert(String::from(entry.name), entry.ptr);
    }

    let inst = world
        .load_fn(c"vtable_mod", |app| {
            #[allow(unsafe_code)]
            // SAFETY: `Health::symbols()` registered the static `HealthVTable` on `world`.
            let vtable = unsafe {
                app.host()
                    .resolve_vtable::<HealthVTable>(c"bam.v0.game::Health.vtable", Health::UUID)
            }
            .expect("resolve HealthVTable");
            let mut h = Health {
                current: 75.0,
                max: 100.0,
            };
            #[allow(unsafe_code)]
            // SAFETY: `h` is a valid initialized `Health` instance on the stack.
            unsafe {
                let cur = (vtable.get_current.unwrap())((&raw const h).cast::<c_void>());
                assert_eq!(cur, 75.0);
                let set_res = (vtable.set_current.unwrap())((&raw mut h).cast::<c_void>(), 90.0);
                assert_eq!(set_res, FfiResult::OK);
            }
            assert_eq!(h.current, 90.0);

            // Wrong UUID must fail with `TYPE_MISMATCH`:
            let wrong_uuid = ComponentUuid::from_u128(0xDEAD);
            #[allow(unsafe_code)]
            // SAFETY: Testing header UUID mismatch rejection before dereferencing `HealthVTable`.
            let mismatch = unsafe {
                app.host()
                    .resolve_vtable::<HealthVTable>(c"bam.v0.game::Health.vtable", wrong_uuid)
            };
            assert_eq!(mismatch.err(), Some(FfiResult::TYPE_MISMATCH));
            FfiResult::OK
        })
        .expect("load vtable_mod");

    world.unload_instance(inst);
}

#[test]
fn typed_tuple_queries_and_par_chunks_without_magic_indices() {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    #[cfg_attr(feature = "derive", derive(AbiPod))]
    struct Accel {
        ax: f32,
        ay: f32,
    }
    #[cfg(not(feature = "derive"))]
    impl_pod_no_derive!(Accel);
    impl GuestComponent for Accel {
        const NAME: &'static CStr = c"game::Accel";
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    #[cfg_attr(feature = "derive", derive(AbiPod))]
    struct Mass(f32);
    #[cfg(not(feature = "derive"))]
    impl_pod_no_derive!(Mass);
    impl GuestComponent for Mass {
        const NAME: &'static CStr = c"game::Mass";
    }

    let mut world = MockHostWorld::new();
    let pos_h = world.expose_component::<Pos>("game::Pos", Pos::UUID, 0, false);
    let vel_h = world.expose_component::<Vel>("game::Vel", Vel::UUID, 0, false);
    let shield_h = world.expose_component::<Shield>("game::Shield", Shield::UUID, 0, false);
    let tag_h = world.expose_component::<Tag>("game::Tag", Tag::UUID, 0, false);
    let dead_h = world.expose_component::<Dead>("game::Dead", Dead::UUID, 0, false);
    let accel_h = world.expose_component::<Accel>("game::Accel", Accel::UUID, 0, false);
    let mass_h = world.expose_component::<Mass>("game::Mass", Mass::UUID, 0, false);

    // Chunk 0: has optional Shield and lacks Dead
    let mut c0 = MockChunkData::new(vec![FfiEntity::from_bits(1), FfiEntity::from_bits(2)]);
    c0.insert_dense(pos_h, &[Pos { x: 0.0, y: 0.0 }, Pos { x: 10.0, y: 20.0 }]);
    c0.insert_dense(vel_h, &[Vel { dx: 1.0, dy: 2.0 }, Vel { dx: 3.0, dy: 4.0 }]);
    c0.insert_dense(shield_h, &[Shield { hp: 5.0 }, Shield { hp: 10.0 }]);
    c0.insert_dense(tag_h, &[Tag, Tag]);
    c0.insert_dense(
        accel_h,
        &[Accel { ax: 0.5, ay: 0.5 }, Accel { ax: 1.0, ay: 1.0 }],
    );
    c0.insert_dense(mass_h, &[Mass(2.0), Mass(4.0)]);
    world.chunks.push(c0);

    // Chunk 1: lacks optional Shield and lacks Dead
    let mut c1 = MockChunkData::new(vec![FfiEntity::from_bits(3)]);
    c1.insert_dense(pos_h, &[Pos { x: 5.0, y: 5.0 }]);
    c1.insert_dense(vel_h, &[Vel { dx: 2.0, dy: -1.0 }]);
    c1.insert_dense(tag_h, &[Tag]);
    c1.insert_dense(accel_h, &[Accel { ax: 0.0, ay: 2.0 }]);
    c1.insert_dense(mass_h, &[Mass(1.0)]);
    world.chunks.push(c1);

    // Chunk 2: has Dead -> excluded by `Without<Dead>`
    let mut c2 = MockChunkData::new(vec![FfiEntity::from_bits(4)]);
    c2.insert_dense(pos_h, &[Pos { x: 99.0, y: 99.0 }]);
    c2.insert_dense(vel_h, &[Vel { dx: 9.0, dy: 9.0 }]);
    c2.insert_dense(tag_h, &[Tag]);
    c2.insert_dense(dead_h, &[Dead]);
    c2.insert_dense(accel_h, &[Accel { ax: 1.0, ay: 1.0 }]);
    c2.insert_dense(mass_h, &[Mass(1.0)]);
    world.chunks.push(c2);

    type FullTuple<'q> = (
        FfiEntity,
        &'q mut Pos,
        &'q Vel,
        Option<&'q mut Shield>,
        Changed<Accel>,
        Added<Mass>,
        With<Tag>,
        Without<Dead>,
    );

    let inst = world
        .load_fn(c"typed_par_mod", |app| {
            // System 0: 8-arity typed tuple query (zero magic indices!)
            let r0 = app
                .system(c"arity8_typed")
                .query_typed::<FullTuple<'_>>()
                .build(|ctx| {
                    // Wrong UUID (`ImposterPos` instead of `Pos`) must fail cleanly with `TYPE_MISMATCH`!
                    {
                        let mut bad = ctx.single_query::<(&mut ImposterPos, &Vel)>();
                        assert_eq!(
                            bad.try_next_chunk().unwrap().err(),
                            Some(FfiResult::TYPE_MISMATCH)
                        );
                        assert_eq!(bad.reset(), FfiResult::OK);
                    }

                    let mut q = ctx.single_query::<FullTuple<'_>>();
                    q.for_each(|(entities, pos, vel, opt_shield, accel, mass, (), ())| {
                        assert_eq!(entities.len(), pos.len());
                        for i in 0..pos.len() {
                            pos[i].x += vel[i].dx + accel[i].ax * mass[i].0;
                            pos[i].y += vel[i].dy + accel[i].ay * mass[i].0;
                        }
                        if let Some(shields) = opt_shield {
                            for s in shields {
                                s.hp += 1.0;
                            }
                        }
                    })
                });
            assert_eq!(r0, FfiResult::OK);

            // System 1: `par_for_each` over typed query
            let r1 = app
                .system(c"par_typed")
                .query_typed::<(&mut Pos, &Vel, Without<Dead>)>()
                .build(|ctx| {
                    ctx.single_query::<(&mut Pos, &Vel, Without<Dead>)>()
                        .par_for_each(|(pos, vel, ())| {
                            for (p, v) in pos.iter_mut().zip(vel.iter()) {
                                p.x += v.dx;
                                p.y += v.dy;
                            }
                        })
                });
            assert_eq!(r1, FfiResult::OK);

            // System 2: `par_chunks` per-task panic containment -> `PANIC_CAUGHT`
            app.system(c"par_panic").query_typed::<&Pos>().build(|ctx| {
                ctx.par_chunks(|_chunk| {
                    panic!("simulated worker chunk panic");
                })
            })
        })
        .expect("load typed_par_mod");

    assert_eq!(world.run_system(0), FfiResult::OK);
    // Chunk 0 row 0: (0+1+0.5*2, 0+2+0.5*2) = (2.0, 3.0); row 1: (10+3+1*4, 20+4+1*4) = (17.0, 28.0)
    assert_eq!(
        world.chunks[0].read_dense::<Pos>(pos_h),
        vec![Pos { x: 2.0, y: 3.0 }, Pos { x: 17.0, y: 28.0 }]
    );
    assert_eq!(
        world.chunks[0].read_dense::<Shield>(shield_h),
        vec![Shield { hp: 6.0 }, Shield { hp: 11.0 }]
    );
    // Chunk 1: (5+2+0*1, 5-1+2*1) = (7.0, 6.0)
    assert_eq!(
        world.chunks[1].read_dense::<Pos>(pos_h),
        vec![Pos { x: 7.0, y: 6.0 }]
    );
    // Chunk 2 (Dead): untouched!
    assert_eq!(
        world.chunks[2].read_dense::<Pos>(pos_h),
        vec![Pos { x: 99.0, y: 99.0 }]
    );

    // Run parallel typed query
    assert_eq!(world.run_system(1), FfiResult::OK);
    assert_eq!(
        world.chunks[0].read_dense::<Pos>(pos_h),
        vec![Pos { x: 3.0, y: 5.0 }, Pos { x: 20.0, y: 32.0 }]
    );
    assert_eq!(
        world.chunks[1].read_dense::<Pos>(pos_h),
        vec![Pos { x: 9.0, y: 5.0 }]
    );

    // Worker task panic caught cleanly as `PANIC_CAUGHT` without unwinding across `extern "C"`!
    assert_eq!(world.run_system(2), FfiResult::PANIC_CAUGHT);

    world.unload_instance(inst);
}

#[test]
fn state_messages_random_get_and_hooks() {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[cfg_attr(feature = "derive", derive(AbiPod))]
    struct DamageEvent {
        target_bits: u64,
        amount: u32,
    }
    #[cfg(not(feature = "derive"))]
    impl_pod_no_derive!(DamageEvent);
    impl GuestComponent for DamageEvent {
        const NAME: &'static CStr = c"game::DamageEvent";
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[cfg_attr(feature = "derive", derive(AbiPod))]
    struct HookComp {
        code: u32,
    }
    #[cfg(not(feature = "derive"))]
    impl_pod_no_derive!(HookComp);
    impl GuestComponent for HookComp {
        const NAME: &'static CStr = c"HookComp";
    }

    static SAVED_RESTORED_VALUE: AtomicUsize = AtomicUsize::new(0);
    #[derive(Default)]
    struct StatefulMod {
        counter: u64,
    }

    impl GuestPlugin for StatefulMod {
        const NAME: &'static CStr = c"stateful_mod";
        const VERSION: &'static CStr = c"0.2.0";

        fn build(&mut self, _app: &mut GuestApp<'_>) -> FfiResult {
            self.counter = 42;
            FfiResult::OK
        }

        fn save_state(&self) -> Result<Vec<u8>, FfiResult> {
            Ok(self.counter.to_le_bytes().to_vec())
        }

        fn restore_state(&mut self, bytes: &[u8]) -> FfiResult {
            let Ok(arr) = <[u8; 8]>::try_from(bytes) else {
                return FfiResult::INVALID_ARGUMENT;
            };
            self.counter = u64::from_le_bytes(arr) + 100;
            SAVED_RESTORED_VALUE.store(self.counter as usize, Ordering::SeqCst);
            FfiResult::OK
        }
    }

    let mut world = MockHostWorld::new();
    let pos_h = world.expose_component::<Pos>("game::Pos", Pos::UUID, 0, false);
    let vel_h = world.expose_component::<Vel>("game::Vel", Vel::UUID, 0, false);
    let dead_h = world.expose_component::<Dead>("game::Dead", Dead::UUID, 0, false);
    let _msg_h =
        world.expose_component::<DamageEvent>("game::DamageEvent", DamageEvent::UUID, 0, false);

    let e1 = FfiEntity::from_bits(101);
    let e_dead = FfiEntity::from_bits(102);
    let mut c0 = MockChunkData::new(vec![e1]);
    c0.insert_dense(pos_h, &[Pos { x: 10.0, y: 20.0 }]);
    c0.insert_dense(vel_h, &[Vel { dx: 1.0, dy: 2.0 }]);
    world.chunks.push(c0);

    let mut c1 = MockChunkData::new(vec![e_dead]);
    c1.insert_dense(pos_h, &[Pos { x: 50.0, y: 60.0 }]);
    c1.insert_dense(vel_h, &[Vel { dx: 3.0, dy: 4.0 }]);
    c1.insert_dense(dead_h, &[Dead]);
    world.chunks.push(c1);

    // 1. Test `GuestPlugin::save_state` & `restore_state` via `StatefulMod::MANIFEST`
    let inst_a = world
        .load_plugin::<StatefulMod>()
        .expect("load StatefulMod A");
    let mut state_blob: Vec<u8> = Vec::new();
    #[allow(unsafe_code)]
    unsafe extern "C" fn vec_writer(ctx: *mut c_void, bytes: *const u8, len: usize) -> FfiResult {
        #[allow(unsafe_code)]
        // SAFETY: `ctx` points to `state_blob`, `bytes` has `len` readable bytes.
        unsafe {
            let out = &mut *ctx.cast::<Vec<u8>>();
            out.extend_from_slice(core::slice::from_raw_parts(bytes, len));
        }
        FfiResult::OK
    }
    let save_fn = StatefulMod::MANIFEST.save_state.expect("save_state slot");
    let restore_fn = StatefulMod::MANIFEST
        .restore_state
        .expect("restore_state slot");
    #[allow(unsafe_code)]
    // SAFETY: `inst_a` is a live `StatefulMod` instance and `state_blob` is stack-allocated.
    let save_rc = unsafe {
        save_fn(
            inst_a,
            Some(vec_writer),
            (&raw mut state_blob).cast::<c_void>(),
        )
    };
    assert_eq!(save_rc, FfiResult::OK);
    assert_eq!(state_blob, 42u64.to_le_bytes());
    world.unload_instance(inst_a);

    let inst_b = world
        .load_plugin::<StatefulMod>()
        .expect("load StatefulMod B");
    #[allow(unsafe_code)]
    // SAFETY: `inst_b` is a live `StatefulMod` instance and `state_blob` is valid.
    let restore_rc = unsafe { restore_fn(inst_b, state_blob.as_ptr(), state_blob.len()) };
    assert_eq!(restore_rc, FfiResult::OK);
    assert_eq!(SAVED_RESTORED_VALUE.load(Ordering::SeqCst), 142);
    world.unload_instance(inst_b);

    // 2. Test messages, random `get`/`get_mut`, and component hooks
    let added_sum = Arc::new(AtomicUsize::new(0));
    let removed_sum = Arc::new(AtomicUsize::new(0));
    let added_sum_c = Arc::clone(&added_sum);
    let removed_sum_c = Arc::clone(&removed_sum);
    let reader_a_total = Arc::new(AtomicUsize::new(0));
    let reader_b_total = Arc::new(AtomicUsize::new(0));
    let reader_a_c = Arc::clone(&reader_a_total);
    let reader_b_c = Arc::clone(&reader_b_total);

    let mut hook_comp_handle = BamComponent::INVALID;
    let hook_comp_out = &mut hook_comp_handle;
    let inst_ext = world
        .load_fn(c"ext_mod", move |app| {
            let hc = app
                .register_component_with_hooks::<HookComp>(
                    GuestComponentHooks::<HookComp>::new()
                        .on_add(move |_e, val| {
                            added_sum_c.fetch_add(val.code as usize, Ordering::SeqCst);
                        })
                        .on_remove(move |_e, val| {
                            removed_sum_c.fetch_add(val.code as usize, Ordering::SeqCst);
                        }),
                )
                .expect("register HookComp with hooks");
            *hook_comp_out = hc;

            // System 0: Sender
            let s0 = app
                .system(c"msg_sender")
                .message_write::<DamageEvent>()
                .build(|ctx| {
                    ctx.send_message(
                        0,
                        &DamageEvent {
                            target_bits: 101,
                            amount: 15,
                        },
                    )
                });
            assert_eq!(s0, FfiResult::OK);

            // System 1: Reader A + Random Entity Access
            let s1 = app
                .system(c"msg_reader_a_and_random_get")
                .query_typed::<(&mut Pos, &Vel, Without<Dead>)>()
                .message_read::<DamageEvent>()
                .build(move |ctx| {
                    // `e_dead` has `Dead` -> fails `Without<Dead>` filter -> `get` and `get_mut` return `None`!
                    assert!(ctx.get::<Pos>(0, 0, e_dead).is_none());
                    assert!(ctx.get_mut::<Pos>(0, 0, e_dead).is_none());
                    // Term 1 (`Vel`) is `READ`-only -> `get_mut` returns `None`, `get` succeeds!
                    assert!(ctx.get_mut::<Vel>(0, 1, e1).is_none());
                    let vel = *ctx.get::<Vel>(0, 1, e1).expect("read Vel on e1");
                    assert_eq!(vel, Vel { dx: 1.0, dy: 2.0 });
                    // Wrong UUID (`ImposterPos`) -> `None`!
                    assert!(ctx.get::<ImposterPos>(0, 0, e1).is_none());

                    let msgs = ctx.read_messages::<DamageEvent>(0)?;
                    for m in msgs {
                        reader_a_c.fetch_add(m.amount as usize, Ordering::SeqCst);
                        let target = FfiEntity::from_bits(m.target_bits);
                        if let Some(pos) = ctx.get_mut::<Pos>(0, 0, target) {
                            pos.x += m.amount as f32;
                        }
                    }
                    Ok(())
                });
            assert_eq!(s1, FfiResult::OK);

            // System 2: Independent Reader B maintains its own message cursor!
            app.system(c"msg_reader_b")
                .message_read::<DamageEvent>()
                .build(move |ctx| {
                    let msgs = ctx.read_messages::<DamageEvent>(0)?;
                    for m in msgs {
                        reader_b_c.fetch_add(m.amount as usize, Ordering::SeqCst);
                    }
                    Ok(())
                })
        })
        .expect("load ext_mod");

    // Verify hook trampolines registered on mock component descriptor
    let meta = world.components[(hook_comp_handle.to_raw() as usize) - 1].clone();
    let sample = HookComp { code: 77 };
    #[allow(unsafe_code)]
    // SAFETY: Invoking registered panic-safe hook trampolines with aligned `&HookComp` and `hook_user_data`.
    unsafe {
        (meta.on_add.expect("on_add registered"))(
            e1,
            (&raw const sample).cast::<c_void>(),
            meta.hook_user_data,
        );
        (meta.on_remove.expect("on_remove registered"))(
            e1,
            (&raw const sample).cast::<c_void>(),
            meta.hook_user_data,
        );
    }
    assert_eq!(added_sum.load(Ordering::SeqCst), 77);
    assert_eq!(removed_sum.load(Ordering::SeqCst), 77);

    // Frame 1: Sender writes 15; Reader A reads 15 and mutates `e1.Pos.x` (`10 + 15 = 25`); Reader B does not run yet!
    assert_eq!(world.run_system(0), FfiResult::OK);
    assert_eq!(world.run_system(1), FfiResult::OK);
    assert_eq!(reader_a_total.load(Ordering::SeqCst), 15);
    assert_eq!(
        world.chunks[0].read_dense::<Pos>(pos_h),
        vec![Pos { x: 25.0, y: 20.0 }]
    );
    // Re-running Reader A with no new messages reads 0 (cursor advanced!).
    assert_eq!(world.run_system(1), FfiResult::OK);
    assert_eq!(reader_a_total.load(Ordering::SeqCst), 15);

    // Frame 2: Sender writes another 15; Reader B runs for the first time and sees BOTH (30 total)!
    assert_eq!(world.run_system(0), FfiResult::OK);
    assert_eq!(world.run_system(2), FfiResult::OK);
    assert_eq!(reader_b_total.load(Ordering::SeqCst), 30);

    world.unload_instance(inst_ext);
}

#[test]
fn guest_sdk_child_of_ticks_and_in_set_smoke() {
    use bevy_abi_mod_guest::prelude::*;

    assert_eq!(FfiChildOf::NAME, c"bevy_ecs::hierarchy::ChildOf");
    assert_eq!(
        FfiChildOf::UUID,
        ComponentUuid::from_parts("bevy_ecs::hierarchy::ChildOf", 8, 8, 0)
    );

    let mut val = 10u32;
    let ticks = BamChangeTicks {
        added: 5,
        changed: 8,
        last_run: 5,
        this_run: 8,
    };
    let r = GuestRes::new(&val, ticks);
    assert!(!r.is_added());
    assert!(r.is_changed());
    assert_eq!(*r, 10);

    let ticks_mut = BamChangeTicks {
        added: 10,
        changed: 10,
        last_run: 10,
        this_run: 12,
    };
    let mut rm = GuestResMut::new(&mut val, ticks_mut);
    assert!(!rm.is_added());
    assert!(!rm.is_changed());
    *rm += 5;
    assert_eq!(*rm, 15);
}
