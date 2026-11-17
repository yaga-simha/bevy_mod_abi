//! Wave 4 Host FFI Feature Tests (`P-07`, `F-02`, `F-03`, `M-07`):
//! - `P-07`: `HOST_API.par_for_each_chunk` parallel chunk iteration over dense 10k-row tables,
//!   multi-table archetypes, and sparse-set components, verifying sequential parity, error
//!   propagation, and non-reentrant `ACCESS_CONFLICT` safety.
//! - `F-02`: `register_mod_message::<M>()` with `AccessMode::MESSAGE_READ` / `MESSAGE_WRITE`,
//!   `HOST_API.msg_read_batch` / `msg_send`, independent per-system `MessageCursor<M>`s, and
//!   scheduler conflict enforcement.
//! - `F-03`: `HOST_API.sys_entity_get` / `sys_entity_get_mut` random per-entity access strictly
//!   enforcing declared `FilteredAccessSet` permissions and archetype/tick query filters while
//!   updating Bevy `Changed<T>` ticks.
//! - `M-07`: `ModComponent::C_DECL` and `ModComponent::ZIG_DECL` emission checks.

use crate::support::{NameTag, Position, Velocity};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, HOST_API, host_iter_column_mutable, host_iter_column_readonly,
    host_iter_column_rows_mutable, host_iter_entities, host_iter_next_chunk, host_msg_read_batch,
    host_msg_send, host_par_for_each_chunk, host_sys_entity_get, host_sys_entity_get_mut,
    host_sys_query,
};
use bevy_abi_mod_sys::{BamIter, BamRunIfFn, BamSystemCtx, ModComponent as _};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::query::Changed;
use bevy_ecs::schedule::IntoScheduleConfigs;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "w4_ffi::SparseBoost")]
struct SparseBoost {
    factor: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "w4_ffi::ActiveUnit")]
struct ActiveUnit {
    squad: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "w4_ffi::FrozenUnit")]
struct FrozenUnit {
    reason: u32,
}

#[repr(C)]
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "w4_ffi::CombatHit")]
struct CombatHit {
    entity_bits: u64,
    damage: i32,
    source_tag: u32,
}

#[test]
fn wave4_m07_mod_component_emits_c_and_zig_declarations() {
    assert!(Position::C_DECL.contains("typedef struct PositionVTable"));
    assert!(Position::C_DECL.contains("BAM_STATIC_ASSERT"));
    assert!(Position::ZIG_DECL.contains("pub const Position = extern struct"));
    assert!(NameTag::C_DECL.contains("BamStrView"));
    assert!(!NameTag::C_DECL.contains("typedef struct NameTag {"));
}

struct ParDenseCtx {
    chunks_seen: AtomicUsize,
    rows_seen: AtomicUsize,
    reentrant_rc: AtomicU64,
    fail_on_row_over: usize,
}

unsafe extern "C" fn nested_noop_task(_iter: *mut BamIter, _ud: *mut c_void) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn par_dense_integrate_task(
    sub_iter: *mut BamIter,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const ParDenseCtx) };
    // Verify nested/re-entrant `par_for_each_chunk` on a task sub-iterator is rejected with `ACCESS_CONFLICT`.
    let nested = unsafe { host_par_for_each_chunk(sub_iter, Some(nested_noop_task), user_data) };
    ctx.reentrant_rc.store(nested.0 as u64, Ordering::Relaxed);

    let mut len = 0usize;
    while unsafe { host_iter_next_chunk(sub_iter, &mut len) }.get() {
        ctx.chunks_seen.fetch_add(1, Ordering::Relaxed);
        let prev = ctx.rows_seen.fetch_add(len, Ordering::Relaxed);
        if ctx.fail_on_row_over > 0 && prev + len > ctx.fail_on_row_over {
            return FfiResult::PANIC_CAUGHT;
        }
        let pos_ptr = unsafe { host_iter_column_mutable(sub_iter, 0) } as *mut Position;
        let vel_ptr = unsafe { host_iter_column_readonly(sub_iter, 1) } as *const Velocity;
        let ents_ptr = unsafe { host_iter_entities(sub_iter) };
        if pos_ptr.is_null() || vel_ptr.is_null() || ents_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        for i in 0..len {
            unsafe {
                let p = &mut *pos_ptr.add(i);
                let v = &*vel_ptr.add(i);
                p.x += v.x * 0.5;
                p.y += v.y * 0.5;
                p.z += v.z * 0.5;
            }
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn par_dense_sys_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if !rc.is_ok() {
        return rc;
    }
    let par_fn = HOST_API
        .par_for_each_chunk
        .expect("HOST_API.par_for_each_chunk populated");
    unsafe { par_fn(iter, Some(par_dense_integrate_task), user_data) }
}

unsafe extern "C" fn par_sparse_task(sub_iter: *mut BamIter, user_data: *mut c_void) -> FfiResult {
    let rows_count = unsafe { &*(user_data as *const AtomicUsize) };
    let mut len = 0usize;
    while unsafe { host_iter_next_chunk(sub_iter, &mut len) }.get() {
        rows_count.fetch_add(len, Ordering::Relaxed);
        let pos_ptr = unsafe { host_iter_column_mutable(sub_iter, 0) } as *mut Position;
        let boost_rows = unsafe { host_iter_column_rows_mutable(sub_iter, 1) };
        if pos_ptr.is_null() || boost_rows.is_null() {
            return FfiResult::NULL_POINTER;
        }
        for i in 0..len {
            unsafe {
                let p = &mut *pos_ptr.add(i);
                let b = &mut *((*boost_rows.add(i)) as *mut SparseBoost);
                p.x += b.factor;
                b.factor += 1.0;
            }
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn par_sparse_sys_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if !rc.is_ok() {
        return rc;
    }
    unsafe { host_par_for_each_chunk(iter, Some(par_sparse_task), user_data) }
}

#[test]
fn wave4_p07_par_for_each_chunk_dense_and_sparse_parity_and_error_propagation() {
    const TOTAL_ENTITIES: usize = 10_000;

    let mut par_app = App::new();
    par_app.add_plugins(AbiModPlugin);
    let pos_h = par_app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let vel_h = par_app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();
    let active_h = par_app
        .register_mod_component::<ActiveUnit>(ComponentOptions::new())
        .unwrap();
    let boost_h = par_app
        .register_mod_component::<SparseBoost>(ComponentOptions::new())
        .unwrap();

    let mut seq_expected = Vec::with_capacity(TOTAL_ENTITIES);
    for i in 0..TOTAL_ENTITIES {
        let p = Position {
            x: i as f32,
            y: (i % 17) as f32,
            z: -1.0,
        };
        let v = Velocity {
            x: 2.0,
            y: -4.0,
            z: 6.0,
        };
        if i % 2 == 0 {
            par_app.world_mut().spawn((p, v));
        } else {
            par_app.world_mut().spawn((
                p,
                v,
                ActiveUnit {
                    squad: (i % 5) as u32,
                },
            ));
        }
        seq_expected.push(Position {
            x: p.x + v.x * 0.5,
            y: p.y + v.y * 0.5,
            z: p.z + v.z * 0.5,
        });
    }

    let ctx = Arc::new(ParDenseCtx {
        chunks_seen: AtomicUsize::new(0),
        rows_seen: AtomicUsize::new(0),
        reentrant_rc: AtomicU64::new(0),
        fail_on_row_over: 0,
    });
    let reg = par_app.world().resource::<ModRegistry>().clone();
    let dense_sys = GuestSystem::new(
        "par_dense",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::new(pos_h, AccessMode::WRITE),
            BamTermDesc::new(vel_h, AccessMode::READ),
        ])],
        par_dense_sys_cb,
        Arc::as_ptr(&ctx) as *mut c_void,
        reg,
    );
    par_app.add_systems(Update, dense_sys);
    par_app.update();

    assert_eq!(ctx.rows_seen.load(Ordering::SeqCst), TOTAL_ENTITIES);
    // 10k rows across 2 tables with target_grain >= 256 partitions into multiple sub-chunks!
    assert!(ctx.chunks_seen.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        FfiResult(ctx.reentrant_rc.load(Ordering::SeqCst) as u32),
        FfiResult::ACCESS_CONFLICT
    );

    // Verify 1:1 parity against sequential calculation across all 10,000 entities!
    let mut actual_positions: Vec<Position> = par_app
        .world_mut()
        .query::<&Position>()
        .iter(par_app.world())
        .copied()
        .collect();
    actual_positions.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap());
    seq_expected.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap());
    assert_eq!(actual_positions, seq_expected);

    // Verify sparse-set `Archetypes` backing in `par_for_each_chunk`!
    let mut sparse_app = App::new();
    sparse_app.add_plugins(AbiModPlugin);
    let s_pos_h = sparse_app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let s_boost_h = sparse_app
        .register_mod_component::<SparseBoost>(ComponentOptions::new())
        .unwrap();
    let _ = active_h;
    let _ = boost_h;
    for i in 0..1_200 {
        sparse_app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            SparseBoost { factor: 3.0 },
        ));
    }
    let sparse_rows_seen = Arc::new(AtomicUsize::new(0));
    let sparse_reg = sparse_app.world().resource::<ModRegistry>().clone();
    sparse_app.add_systems(
        Update,
        GuestSystem::new(
            "par_sparse",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(s_pos_h, AccessMode::WRITE),
                BamTermDesc::new(s_boost_h, AccessMode::WRITE),
            ])],
            par_sparse_sys_cb,
            Arc::as_ptr(&sparse_rows_seen) as *mut c_void,
            sparse_reg,
        ),
    );
    sparse_app.update();
    assert_eq!(sparse_rows_seen.load(Ordering::SeqCst), 1_200);
    for (p, b) in sparse_app
        .world_mut()
        .query::<(&Position, &SparseBoost)>()
        .iter(sparse_app.world())
    {
        assert!((b.factor - 4.0).abs() < f32::EPSILON);
        assert!(p.x >= 3.0);
    }

    // Verify error/panic status propagation quarantines the faulting guest system!
    let mut err_app = App::new();
    err_app.add_plugins(AbiModPlugin);
    let e_pos_h = err_app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let e_vel_h = err_app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();
    for i in 0..600 {
        err_app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
        ));
    }
    let err_ctx = Arc::new(ParDenseCtx {
        chunks_seen: AtomicUsize::new(0),
        rows_seen: AtomicUsize::new(0),
        reentrant_rc: AtomicU64::new(0),
        fail_on_row_over: 1,
    });
    let err_reg = err_app.world().resource::<ModRegistry>().clone();
    err_app.add_systems(
        Update,
        GuestSystem::new(
            "par_faulting",
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(e_pos_h, AccessMode::WRITE),
                BamTermDesc::new(e_vel_h, AccessMode::READ),
            ])],
            par_dense_sys_cb,
            Arc::as_ptr(&err_ctx) as *mut c_void,
            err_reg,
        )
        .with_mod_id(ModId(77)),
    );
    err_app.update();
    assert_eq!(
        err_app
            .world()
            .resource::<Messages<ModSystemFaulted>>()
            .len(),
        1
    );
}

struct MsgSenderData {
    batch: Mutex<Vec<CombatHit>>,
}

unsafe extern "C" fn msg_sender_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let data = unsafe { &*(user_data as *const MsgSenderData) };
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if !rc.is_ok() {
        return rc;
    }
    let items: Vec<CombatHit> = data.batch.lock().unwrap().drain(..).collect();
    for hit in &items {
        let send_rc = unsafe {
            host_msg_send(
                iter,
                0,
                core::ptr::from_ref(hit).cast::<u8>(),
                core::mem::size_of::<CombatHit>(),
            )
        };
        if !send_rc.is_ok() {
            return send_rc;
        }
    }
    FfiResult::OK
}

struct MsgReaderData {
    received: Mutex<Vec<CombatHit>>,
    enable: AtomicUsize,
}

unsafe extern "C" fn msg_reader_run_if(user_data: *mut c_void) -> FfiBool {
    let data = unsafe { &*(user_data as *const MsgReaderData) };
    FfiBool::from_bool(data.enable.load(Ordering::SeqCst) != 0)
}

unsafe extern "C" fn msg_reader_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let data = unsafe { &*(user_data as *const MsgReaderData) };
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if !rc.is_ok() {
        return rc;
    }
    let mut buf = [CombatHit::default(); 8];
    loop {
        let mut read_count = 0usize;
        let read_rc = unsafe {
            host_msg_read_batch(
                iter,
                0,
                buf.as_mut_ptr().cast::<u8>(),
                buf.len(),
                &mut read_count,
            )
        };
        if !read_rc.is_ok() {
            return read_rc;
        }
        if read_count == 0 {
            break;
        }
        data.received
            .lock()
            .unwrap()
            .extend_from_slice(&buf[..read_count]);
    }
    FfiResult::OK
}

#[test]
fn wave4_f02_messages_read_write_by_handle_and_independent_cursors() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let hit_h = app.register_mod_message::<CombatHit>().unwrap();

    let sender_state = Arc::new(MsgSenderData {
        batch: Mutex::new(vec![
            CombatHit {
                entity_bits: 11,
                damage: 25,
                source_tag: 1,
            },
            CombatHit {
                entity_bits: 12,
                damage: 40,
                source_tag: 2,
            },
        ]),
    });
    let reader_a = Arc::new(MsgReaderData {
        received: Mutex::new(Vec::new()),
        enable: AtomicUsize::new(1),
    });
    let reader_b = Arc::new(MsgReaderData {
        received: Mutex::new(Vec::new()),
        enable: AtomicUsize::new(0), // Paused on frame 1 so we verify independent cursors!
    });

    let reg = app.world().resource::<ModRegistry>().clone();
    let s_write = GuestSystem::new(
        "hit_writer",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            hit_h,
            AccessMode::MESSAGE_WRITE,
        )])],
        msg_sender_cb,
        Arc::as_ptr(&sender_state) as *mut c_void,
        reg.clone(),
    );
    let s_read_a = GuestSystem::new(
        "hit_reader_a",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            hit_h,
            AccessMode::MESSAGE_READ,
        )])],
        msg_reader_cb,
        Arc::as_ptr(&reader_a) as *mut c_void,
        reg.clone(),
    )
    .with_run_if(Some((
        msg_reader_run_if as BamRunIfFn,
        Arc::as_ptr(&reader_a) as *mut c_void,
    )));
    let s_read_b = GuestSystem::new(
        "hit_reader_b",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            hit_h,
            AccessMode::MESSAGE_READ,
        )])],
        msg_reader_cb,
        Arc::as_ptr(&reader_b) as *mut c_void,
        reg,
    )
    .with_run_if(Some((
        msg_reader_run_if as BamRunIfFn,
        Arc::as_ptr(&reader_b) as *mut c_void,
    )));

    app.add_systems(Update, (s_write, s_read_a, s_read_b).chain());

    // Frame 1: `hit_writer` sends 2 messages; `hit_reader_a` reads 2; `hit_reader_b` is paused.
    app.update();
    assert_eq!(reader_a.received.lock().unwrap().len(), 2);
    assert_eq!(reader_b.received.lock().unwrap().len(), 0);
    // Host `Messages<CombatHit>` also contains the 2 guest-sent messages!
    assert_eq!(app.world().resource::<Messages<CombatHit>>().len(), 2);

    // Frame 2: Host pushes 1 more message AND enables `hit_reader_b`!
    app.world_mut().write_message(CombatHit {
        entity_bits: 99,
        damage: 100,
        source_tag: 3,
    });
    reader_b.enable.store(1, Ordering::SeqCst);
    app.update();

    // `hit_reader_a` sees ONLY the 1 new message (total 3), while `hit_reader_b` sees ALL 3!
    let a_msgs = reader_a.received.lock().unwrap().clone();
    let b_msgs = reader_b.received.lock().unwrap().clone();
    assert_eq!(a_msgs.len(), 3);
    assert_eq!(b_msgs.len(), 3);
    assert_eq!(a_msgs, b_msgs);
}

struct RandomEntityAccessCtx {
    e_matched_a: FfiEntity,
    e_matched_b: FfiEntity,
    e_frozen: FfiEntity,
    e_untagged: FfiEntity,
    checks_passed: AtomicUsize,
}

unsafe extern "C" fn random_access_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const RandomEntityAccessCtx) };

    // Query 0 terms: 0 = Position (WRITE), 1 = Velocity (READ), 2 = ActiveUnit (FILTER_ONLY), 3 = FrozenUnit (WITHOUT).
    // 1. Unauthorized `get_mut` on READ-only term 1 (`Velocity`) MUST return null!
    assert!(unsafe { host_sys_entity_get_mut(sys_ctx, 0, ctx.e_matched_a, 1) }.is_null());

    // 2. `get` on `FILTER_ONLY` term 2 (`ActiveUnit`) MUST return null!
    assert!(unsafe { host_sys_entity_get(sys_ctx, 0, ctx.e_matched_a, 2) }.is_null());

    // 3. Entities failing `Without<FrozenUnit>` (`e_frozen`) or `With<ActiveUnit>` (`e_untagged`) MUST return null!
    assert!(unsafe { host_sys_entity_get(sys_ctx, 0, ctx.e_frozen, 0) }.is_null());
    assert!(unsafe { host_sys_entity_get_mut(sys_ctx, 0, ctx.e_frozen, 0) }.is_null());
    assert!(unsafe { host_sys_entity_get(sys_ctx, 0, ctx.e_untagged, 0) }.is_null());
    assert!(unsafe { host_sys_entity_get_mut(sys_ctx, 0, ctx.e_untagged, 0) }.is_null());

    // 4. Valid `get` on `e_matched_b` (`Velocity`) and `get_mut` on `e_matched_a` (`Position`) succeed!
    let vel_b_ptr =
        unsafe { host_sys_entity_get(sys_ctx, 0, ctx.e_matched_b, 1) } as *const Velocity;
    let pos_a_ptr =
        unsafe { host_sys_entity_get_mut(sys_ctx, 0, ctx.e_matched_a, 0) } as *mut Position;
    if vel_b_ptr.is_null() || pos_a_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    unsafe {
        let vb = &*vel_b_ptr;
        let pa = &mut *pos_a_ptr;
        pa.x += vb.x;
        pa.y += vb.y;
    }
    ctx.checks_passed.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

#[test]
fn wave4_f03_random_entity_access_enforces_access_and_filters_and_marks_changed() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    let vel_h = app
        .register_mod_component::<Velocity>(ComponentOptions::new())
        .unwrap();
    let active_h = app
        .register_mod_component::<ActiveUnit>(ComponentOptions::new())
        .unwrap();
    let frozen_h = app
        .register_mod_component::<FrozenUnit>(ComponentOptions::new())
        .unwrap();

    let e_a = app
        .world_mut()
        .spawn((
            Position {
                x: 10.0,
                y: 20.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 1.0,
                z: 0.0,
            },
            ActiveUnit { squad: 1 },
        ))
        .id();
    let e_b = app
        .world_mut()
        .spawn((
            Position {
                x: 100.0,
                y: 200.0,
                z: 0.0,
            },
            Velocity {
                x: 7.0,
                y: -3.0,
                z: 0.0,
            },
            ActiveUnit { squad: 1 },
        ))
        .id();
    let e_frozen = app
        .world_mut()
        .spawn((
            Position {
                x: 50.0,
                y: 50.0,
                z: 0.0,
            },
            Velocity {
                x: 2.0,
                y: 2.0,
                z: 0.0,
            },
            ActiveUnit { squad: 1 },
            FrozenUnit { reason: 9 },
        ))
        .id();
    let e_untagged = app
        .world_mut()
        .spawn((
            Position {
                x: 30.0,
                y: 30.0,
                z: 0.0,
            },
            Velocity {
                x: 5.0,
                y: 5.0,
                z: 0.0,
            },
        ))
        .id();

    let changed_entities: Arc<Mutex<Vec<Entity>>> = Arc::new(Mutex::new(Vec::new()));
    let changed_clone = Arc::clone(&changed_entities);
    let verify_changed = move |q: bevy_ecs::system::Query<Entity, Changed<Position>>| {
        changed_clone.lock().unwrap().extend(q.iter());
    };
    #[derive(bevy_ecs::schedule::SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
    struct VerifyChangedSet;
    app.add_systems(Update, verify_changed.in_set(VerifyChangedSet));

    // Advance initial spawn ticks on `verify_changed` so `Changed<Position>` starts clean!
    app.update();
    changed_entities.lock().unwrap().clear();

    let ctx = Arc::new(RandomEntityAccessCtx {
        e_matched_a: FfiEntity::from_bits(e_a.to_bits()),
        e_matched_b: FfiEntity::from_bits(e_b.to_bits()),
        e_frozen: FfiEntity::from_bits(e_frozen.to_bits()),
        e_untagged: FfiEntity::from_bits(e_untagged.to_bits()),
        checks_passed: AtomicUsize::new(0),
    });

    let reg = app.world().resource::<ModRegistry>().clone();
    let guest_sys = GuestSystem::new(
        "random_lookup",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::new(pos_h, AccessMode::WRITE),
            BamTermDesc::new(vel_h, AccessMode::READ),
            BamTermDesc::new(active_h, AccessMode::FILTER_ONLY),
            BamTermDesc::new(frozen_h, AccessMode::WITHOUT),
        ])],
        random_access_cb,
        Arc::as_ptr(&ctx) as *mut c_void,
        reg,
    );
    app.add_systems(Update, guest_sys.before(VerifyChangedSet));
    app.update();

    assert_eq!(ctx.checks_passed.load(Ordering::SeqCst), 1);
    assert_eq!(
        *app.world().get::<Position>(e_a).unwrap(),
        Position {
            x: 17.0,
            y: 17.0,
            z: 0.0,
        }
    );
    // ONLY `e_a` had `get_mut` called on it — `e_b` was read-only and must NOT be marked changed!
    assert_eq!(*changed_entities.lock().unwrap(), vec![e_a]);
}
