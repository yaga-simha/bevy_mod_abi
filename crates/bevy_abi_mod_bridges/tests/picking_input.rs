#![cfg(all(feature = "bridge-picking", feature = "bridge-std-types"))]
#![allow(
    clippy::undocumented_unsafe_blocks,
    clippy::ptr_as_ptr,
    clippy::cast_ptr_alignment
)]

//! Integration tests for `bridge-picking` + `input_ext`:
//! - All 7 flattened Bevy `0.20` `bevy_picking::events::Pointer*` entity events forwarded into
//!   both Rust guest runtime observers (`cmd_observe`) and a dynamically compiled & loaded ISO C99
//!   shared-library guest mod built directly against `ModComponent::C_DECL`.
//! - `PointerTraversal` bubbling propagation (`target` vs `hit_entity`).
//! - Bidirectional `EntityCursor` <-> `FfiEntityCursor` sync and removal.
//! - Per-frame `AccumulatedMouseMotion` / `AccumulatedMouseScroll` resource sync with `InputPlugin`.
//! - Entity-based `Gamepad` button/axis/disconnect sync and `FfiGamepadRumble` -> `GamepadRumbleRequest`.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy_abi_mod_bridges::input_ext::{
    CANONICAL_GAMEPAD_RUMBLE_NAME, CANONICAL_GAMEPAD_STATE_NAME, CANONICAL_MOUSE_MOTION_NAME,
    CANONICAL_MOUSE_SCROLL_NAME, FfiGamepadButton, FfiGamepadRumble, FfiGamepadState,
    FfiMouseMotion, FfiMouseScroll, ModInputExtBridgePlugin,
};
use bevy_abi_mod_bridges::picking::{
    CANONICAL_ENTITY_CURSOR_NAME, CANONICAL_POINTER_CLICK_NAME, CANONICAL_POINTER_DRAG_NAME,
    CANONICAL_POINTER_MOVE_NAME, CANONICAL_POINTER_OUT_NAME, CANONICAL_POINTER_OVER_NAME,
    CANONICAL_POINTER_PRESS_NAME, CANONICAL_POINTER_RELEASE_NAME, FfiEntityCursor,
    FfiPointerButton, FfiPointerClick, FfiPointerDrag, FfiPointerKind, FfiPointerMove,
    FfiPointerOut, FfiPointerOver, FfiPointerPress, FfiPointerRelease, ModPickingBridgePlugin,
};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, host_cmd_insert, host_cmd_observe, host_cmd_remove,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_entities, host_iter_next_chunk,
    host_msg_send, host_res_ptr, host_sys_commands, host_sys_query,
};
use bevy_abi_mod_sys::{
    AccessMode, BamComponent, BamIter, BamObserverDesc, BamSystemCtx, BamTermDesc, FfiBool,
    FfiEntity, FfiResult, ModComponent,
};
use bevy_app::{App, Update};
use bevy_camera::NormalizedRenderTarget;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::message::Messages;
use bevy_input::InputPlugin;
use bevy_input::gamepad::{
    Gamepad, GamepadAxis, GamepadButton, GamepadRumbleIntensity, GamepadRumbleRequest,
};
use bevy_input::mouse::{MouseMotion, MouseScrollUnit, MouseWheel};
use bevy_input::touch::TouchPhase;
use bevy_math::{Vec2, Vec3};
use bevy_picking::backend::HitData;
use bevy_picking::cursor::EntityCursor;
use bevy_picking::events::{
    Pointer, PointerClick, PointerDrag, PointerMove, PointerOut, PointerOver, PointerPress,
    PointerRelease,
};
use bevy_picking::pointer::{Location, PointerButton, PointerId};
use bevy_window::{SystemCursorIcon, Window};

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "picking_test::PickedTag")]
struct PickedTag {
    clicks: u32,
    button: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "picking_test::C99PickingEcho")]
struct C99PickingEcho {
    click_count: u32,
    button: u32,
}

fn synthetic_pointer(id: PointerId, position: Vec2) -> Pointer {
    Pointer::new(
        id,
        Location {
            target: NormalizedRenderTarget::None {
                width: 1280,
                height: 720,
            },
            position,
        },
    )
}

fn synthetic_hit(camera: Entity, depth: f32) -> HitData {
    HitData::new(
        camera,
        depth,
        Some(Vec3::new(1.5, 2.5, -3.5)),
        Some(Vec3::new(0.0, 1.0, 0.0)),
    )
}

#[derive(Debug, Default)]
struct RustPointerLog {
    presses: Vec<FfiPointerPress>,
    releases: Vec<FfiPointerRelease>,
    moves: Vec<FfiPointerMove>,
    clicks: Vec<FfiPointerClick>,
    overs: Vec<FfiPointerOver>,
    outs: Vec<FfiPointerOut>,
    drags: Vec<FfiPointerDrag>,
}

struct RustObserverCtx {
    log: Arc<Mutex<RustPointerLog>>,
    picked_tag_comp: BamComponent,
}

unsafe extern "C" fn rust_on_press(
    target: FfiEntity,
    event_data: *const c_void,
    _cmds: *mut bevy_abi_mod_sys::BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const RustObserverCtx) };
    let ev = unsafe { *(event_data as *const FfiPointerPress) };
    assert_eq!(target, ev.target.0);
    ctx.log.lock().unwrap().presses.push(ev);
    FfiResult::OK
}

unsafe extern "C" fn rust_on_release(
    target: FfiEntity,
    event_data: *const c_void,
    _cmds: *mut bevy_abi_mod_sys::BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const RustObserverCtx) };
    let ev = unsafe { *(event_data as *const FfiPointerRelease) };
    assert_eq!(target, ev.target.0);
    ctx.log.lock().unwrap().releases.push(ev);
    FfiResult::OK
}

unsafe extern "C" fn rust_on_move(
    target: FfiEntity,
    event_data: *const c_void,
    _cmds: *mut bevy_abi_mod_sys::BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const RustObserverCtx) };
    let ev = unsafe { *(event_data as *const FfiPointerMove) };
    assert_eq!(target, ev.target.0);
    ctx.log.lock().unwrap().moves.push(ev);
    FfiResult::OK
}

unsafe extern "C" fn rust_on_click(
    target: FfiEntity,
    event_data: *const c_void,
    cmds: *mut bevy_abi_mod_sys::BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const RustObserverCtx) };
    let ev = unsafe { *(event_data as *const FfiPointerClick) };
    assert_eq!(target, ev.target.0);
    ctx.log.lock().unwrap().clicks.push(ev);
    let tag = PickedTag {
        clicks: ev.count as u32,
        button: ev.button as u32,
    };
    unsafe {
        host_cmd_insert(
            cmds,
            target,
            ctx.picked_tag_comp,
            core::ptr::from_ref(&tag).cast::<u8>(),
            core::mem::size_of::<PickedTag>(),
        )
    }
}

unsafe extern "C" fn rust_on_over(
    target: FfiEntity,
    event_data: *const c_void,
    _cmds: *mut bevy_abi_mod_sys::BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const RustObserverCtx) };
    let ev = unsafe { *(event_data as *const FfiPointerOver) };
    assert_eq!(target, ev.target.0);
    ctx.log.lock().unwrap().overs.push(ev);
    FfiResult::OK
}

unsafe extern "C" fn rust_on_out(
    target: FfiEntity,
    event_data: *const c_void,
    _cmds: *mut bevy_abi_mod_sys::BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const RustObserverCtx) };
    let ev = unsafe { *(event_data as *const FfiPointerOut) };
    assert_eq!(target, ev.target.0);
    ctx.log.lock().unwrap().outs.push(ev);
    FfiResult::OK
}

unsafe extern "C" fn rust_on_drag(
    target: FfiEntity,
    event_data: *const c_void,
    _cmds: *mut bevy_abi_mod_sys::BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &*(user_data as *const RustObserverCtx) };
    let ev = unsafe { *(event_data as *const FfiPointerDrag) };
    assert_eq!(target, ev.target.0);
    ctx.log.lock().unwrap().drags.push(ev);
    FfiResult::OK
}

#[test]
fn rust_guest_observers_receive_all_seven_flattened_pointer_events_and_bubbling() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModPickingBridgePlugin));
    let picked_tag_h = app
        .register_mod_component::<PickedTag>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )
        .expect("register PickedTag");

    let reg = app.world().resource::<ModRegistry>().clone();
    let press_h = reg
        .get_by_name(CANONICAL_POINTER_PRESS_NAME)
        .unwrap()
        .handle;
    let release_h = reg
        .get_by_name(CANONICAL_POINTER_RELEASE_NAME)
        .unwrap()
        .handle;
    let move_h = reg.get_by_name(CANONICAL_POINTER_MOVE_NAME).unwrap().handle;
    let click_h = reg
        .get_by_name(CANONICAL_POINTER_CLICK_NAME)
        .unwrap()
        .handle;
    let over_h = reg.get_by_name(CANONICAL_POINTER_OVER_NAME).unwrap().handle;
    let out_h = reg.get_by_name(CANONICAL_POINTER_OUT_NAME).unwrap().handle;
    let drag_h = reg.get_by_name(CANONICAL_POINTER_DRAG_NAME).unwrap().handle;
    assert!(reg.get_by_name(CANONICAL_ENTITY_CURSOR_NAME).is_some());

    let camera = app.world_mut().spawn_empty().id();
    let parent = app.world_mut().spawn_empty().id();
    let child = app.world_mut().spawn(ChildOf(parent)).id();
    let unwatched = app.world_mut().spawn_empty().id();

    let log = Arc::new(Mutex::new(RustPointerLog::default()));
    let obs_ctx = Arc::new(RustObserverCtx {
        log: Arc::clone(&log),
        picked_tag_comp: picked_tag_h,
    });

    struct InstallState {
        installed: bool,
        child: FfiEntity,
        parent: FfiEntity,
        handles: [BamComponent; 7],
        obs_ctx_ptr: *const RustObserverCtx,
    }
    unsafe impl Send for InstallState {}
    unsafe impl Sync for InstallState {}

    let install_state = Arc::new(Mutex::new(InstallState {
        installed: false,
        child: FfiEntity::from_bits(child.to_bits()),
        parent: FfiEntity::from_bits(parent.to_bits()),
        handles: [press_h, release_h, move_h, click_h, over_h, out_h, drag_h],
        obs_ctx_ptr: Arc::as_ptr(&obs_ctx),
    }));

    unsafe extern "C" fn install_observers_sys(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let mut st = unsafe { &*(user_data as *const Mutex<InstallState>) }
            .lock()
            .unwrap();
        if st.installed {
            return FfiResult::OK;
        }
        st.installed = true;
        let mut cmds = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_commands(sys_ctx, &mut cmds) },
            FfiResult::OK
        );
        let ud = st.obs_ctx_ptr.cast_mut().cast::<c_void>();
        let specs: [(BamComponent, FfiEntity, bevy_abi_mod_sys::BamObserverFn); 8] = [
            (st.handles[0], st.child, rust_on_press),
            (st.handles[0], st.parent, rust_on_press),
            (st.handles[1], st.child, rust_on_release),
            (st.handles[2], st.child, rust_on_move),
            (st.handles[3], st.child, rust_on_click),
            (st.handles[4], st.child, rust_on_over),
            (st.handles[5], st.child, rust_on_out),
            (st.handles[6], st.child, rust_on_drag),
        ];
        for (ev_handle, target, cb) in specs {
            let desc = BamObserverDesc::new(ev_handle, cb, ud).with_entity(target);
            assert_eq!(
                unsafe { host_cmd_observe(cmds, &desc, core::ptr::null_mut()) },
                FfiResult::OK
            );
        }
        FfiResult::OK
    }

    let sys = GuestSystem::new(
        "picking_rust_installer",
        vec![BamQuerySpecOwned::new(vec![])],
        install_observers_sys,
        Arc::as_ptr(&install_state) as *mut c_void,
        reg,
    );
    app.add_systems(Update, sys);

    // Frame 1 installs all 8 runtime observers via `cmd_observe`.
    app.update();

    let touch_ptr = synthetic_pointer(PointerId::Touch(42), Vec2::new(320.5, 180.25));
    let mouse_ptr = synthetic_pointer(PointerId::Mouse, Vec2::new(100.0, 200.0));
    let hit = synthetic_hit(camera, 4.75);

    // Trigger unwatched entity -> must NOT fire observers watching `child` or `parent`.
    app.world_mut().trigger(PointerPress {
        entity: unwatched,
        pointer: mouse_ptr.clone(),
        button: PointerButton::Primary,
        hit: hit.clone(),
        count: 1,
    });

    // Trigger all 7 flattened Bevy 0.20 picking events on `child`.
    app.world_mut().trigger(PointerOver {
        entity: child,
        pointer: mouse_ptr.clone(),
        hit: hit.clone(),
    });
    app.world_mut().trigger(PointerMove {
        entity: child,
        pointer: mouse_ptr.clone(),
        hit: hit.clone(),
        delta: Vec2::new(5.5, -2.25),
    });
    app.world_mut().trigger(PointerPress {
        entity: child,
        pointer: touch_ptr.clone(),
        button: PointerButton::Secondary,
        hit: hit.clone(),
        count: 2,
    });
    app.world_mut().trigger(PointerDrag {
        entity: child,
        pointer: touch_ptr.clone(),
        button: PointerButton::Secondary,
        distance: Vec2::new(14.0, -6.0),
        delta: Vec2::new(3.5, -1.5),
    });
    app.world_mut().trigger(PointerRelease {
        entity: child,
        pointer: touch_ptr.clone(),
        button: PointerButton::Secondary,
        hit: hit.clone(),
    });
    app.world_mut().trigger(PointerClick {
        entity: child,
        pointer: touch_ptr,
        button: PointerButton::Middle,
        hit: hit.clone(),
        duration: Duration::from_millis(125),
        count: 3,
    });
    app.world_mut().trigger(PointerOut {
        entity: child,
        pointer: mouse_ptr,
        hit,
    });

    // Flush deferred observer commands (`commands.trigger(FfiPointer*)` and guest `cmd_insert`).
    app.update();

    let child_ffi = FfiEntity::from_bits(child.to_bits());
    let parent_ffi = FfiEntity::from_bits(parent.to_bits());
    let camera_ffi = FfiEntity::from_bits(camera.to_bits());

    let recorded = log.lock().unwrap();

    // `PointerPress` bubbled via `PointerTraversal` from `child` -> `parent`, preserving
    // `hit_entity == child` at both levels!
    assert_eq!(recorded.presses.len(), 2);
    assert_eq!(recorded.presses[0].target.0, child_ffi);
    assert_eq!(recorded.presses[0].hit_entity, child_ffi);
    assert_eq!(recorded.presses[0].hit_camera, camera_ffi);
    assert_eq!(
        recorded.presses[0].pointer_kind_typed(),
        FfiPointerKind::TOUCH
    );
    assert_eq!(recorded.presses[0].pointer_id, 42);
    assert_eq!(
        recorded.presses[0].button_bevy(),
        Some(PointerButton::Secondary)
    );
    assert_eq!(recorded.presses[0].count, 2);
    assert_eq!(recorded.presses[0].position, [320.5, 180.25]);
    assert!((recorded.presses[0].hit_depth - 4.75).abs() < 1e-6);
    assert_eq!(
        recorded.presses[0].hit_position_vec3(),
        Some(Vec3::new(1.5, 2.5, -3.5))
    );
    assert_eq!(recorded.presses[0].hit_normal_vec3(), Some(Vec3::Y));

    assert_eq!(recorded.presses[1].target.0, parent_ffi);
    assert_eq!(recorded.presses[1].hit_entity, child_ffi);
    assert_eq!(recorded.presses[1].pointer_id, 42);

    assert_eq!(recorded.releases.len(), 1);
    assert_eq!(recorded.releases[0].target.0, child_ffi);
    assert_eq!(recorded.releases[0].hit_entity, child_ffi);
    assert_eq!(
        recorded.releases[0].button_bevy(),
        Some(PointerButton::Secondary)
    );
    assert!((recorded.releases[0].hit_depth - 4.75).abs() < 1e-6);

    assert_eq!(recorded.moves.len(), 1);
    assert_eq!(recorded.moves[0].delta, [5.5, -2.25]);
    assert_eq!(recorded.moves[0].position, [100.0, 200.0]);
    assert_eq!(recorded.moves[0].pointer_kind, FfiPointerKind::MOUSE_RAW);

    assert_eq!(recorded.clicks.len(), 1);
    assert_eq!(recorded.clicks[0].count, 3);
    assert_eq!(
        recorded.clicks[0].button_bevy(),
        Some(PointerButton::Middle)
    );
    assert!((recorded.clicks[0].duration_secs - 0.125).abs() < 1e-6);
    assert_eq!(recorded.clicks[0].hit_entity_bevy(), Some(child));

    assert_eq!(recorded.overs.len(), 1);
    assert_eq!(recorded.overs[0].hit_entity, child_ffi);
    assert_eq!(recorded.overs[0].hit_camera, camera_ffi);

    assert_eq!(recorded.outs.len(), 1);
    assert_eq!(recorded.outs[0].hit_entity, child_ffi);

    assert_eq!(recorded.drags.len(), 1);
    assert_eq!(recorded.drags[0].distance, [14.0, -6.0]);
    assert_eq!(recorded.drags[0].delta, [3.5, -1.5]);
    assert_eq!(
        recorded.drags[0].button_bevy(),
        Some(PointerButton::Secondary)
    );

    // Observer's deferred `cmd_insert` attached `PickedTag` to `child`!
    assert_eq!(
        app.world().get::<PickedTag>(child),
        Some(&PickedTag {
            clicks: 3,
            button: FfiPointerButton::MIDDLE_RAW as u32,
        })
    );
    assert!(app.world().get::<PickedTag>(unwatched).is_none());
}

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).expect("create temp dir for C99 fixture");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn find_c99_compiler() -> Option<String> {
    if let Ok(cc) = std::env::var("CC")
        && !cc.trim().is_empty()
    {
        return Some(cc);
    }
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["clang", "gcc", "cl"]
    } else {
        &["cc", "clang", "gcc"]
    };
    for &cand in candidates {
        if Command::new(cand).arg("--version").output().is_ok() {
            return Some(cand.to_owned());
        }
    }
    None
}

fn compile_c99_shared_library(
    cc: &str,
    include_dir: &Path,
    gen_header_dir: &Path,
    src_path: &Path,
    out_path: &Path,
) {
    let mut cmd = Command::new(cc);
    let lower = cc.to_ascii_lowercase();
    if cfg!(target_os = "windows") && (lower.ends_with("cl") || lower.ends_with("cl.exe")) {
        let obj_path = out_path.with_extension("obj");
        let implib_path = out_path.with_extension("lib");
        cmd.args(["/nologo", "/LD", "/TC", "/std:c11", "/W4", "/WX"])
            .arg("/I")
            .arg(include_dir)
            .arg("/I")
            .arg(gen_header_dir)
            .arg(format!("/Fo:{}", obj_path.display()))
            .arg(format!("/Fe:{}", out_path.display()))
            .arg(src_path)
            .arg("/link")
            .arg(format!("/IMPLIB:{}", implib_path.display()));
    } else {
        cmd.args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-I{}", include_dir.display()))
            .arg(format!("-I{}", gen_header_dir.display()))
            .arg("-O2")
            .arg("-shared");
        if !cfg!(target_os = "windows") {
            cmd.arg("-fPIC");
        }
        cmd.arg(src_path).arg("-o").arg(out_path);
    }
    let output = cmd.output().expect("spawn C99 compiler");
    assert!(
        output.status.success(),
        "C99 guest fixture compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[derive(Debug, Clone, PartialEq)]
struct C99RecordedEvent {
    kind: u32,
    target: u64,
    hit_entity: u64,
    pointer_id: u64,
    button: u32,
    aux_u32: u32,
    f0: f32,
    f1: f32,
    f2: f32,
}

static C99_EVENT_LOG: Mutex<Vec<C99RecordedEvent>> = Mutex::new(Vec::new());

unsafe extern "C" fn record_c99_event(
    kind: u32,
    target: FfiEntity,
    hit_entity: FfiEntity,
    pointer_id: u64,
    button: u32,
    aux_u32: u32,
    f0: f32,
    f1: f32,
    f2: f32,
) {
    C99_EVENT_LOG.lock().unwrap().push(C99RecordedEvent {
        kind,
        target: target.to_bits(),
        hit_entity: hit_entity.to_bits(),
        pointer_id,
        button,
        aux_u32,
        f0,
        f1,
        f2,
    });
}

#[test]
fn compiled_c99_shared_library_guest_receives_all_seven_pointer_events_and_input_bridges() {
    let cc = find_c99_compiler().expect("C99 compiler (`cc`/`clang`/`gcc`) must be available");
    let temp = TempDirGuard::new("bam_picking_input_c99");
    let include_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../include");
    assert!(
        include_dir.join("bevy_abi_mod.h").is_file(),
        "expected include/bevy_abi_mod.h at {}",
        include_dir.display()
    );

    // Emit a real ISO C99 header directly from `ModComponent::C_DECL` for all 12 wire structs + echo component!
    let mut header = String::from(
        "#ifndef BAM_PICKING_INPUT_TEST_H\n\
         #define BAM_PICKING_INPUT_TEST_H\n\
         #include \"bevy_abi_mod.h\"\n\n",
    );
    for decl in [
        FfiPointerPress::C_DECL,
        FfiPointerRelease::C_DECL,
        FfiPointerMove::C_DECL,
        FfiPointerClick::C_DECL,
        FfiPointerOver::C_DECL,
        FfiPointerOut::C_DECL,
        FfiPointerDrag::C_DECL,
        FfiEntityCursor::C_DECL,
        FfiMouseMotion::C_DECL,
        FfiMouseScroll::C_DECL,
        FfiGamepadState::C_DECL,
        FfiGamepadRumble::C_DECL,
        C99PickingEcho::C_DECL,
    ] {
        assert!(!decl.is_empty(), "ModComponent::C_DECL must be non-empty");
        header.push_str(decl);
        header.push('\n');
    }
    header.push_str("#endif\n");
    std::fs::write(temp.path().join("picking_input_generated.h"), &header)
        .expect("write generated C99 header");

    let c_source = r#"
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "picking_input_generated.h"

typedef void (*RecordC99EventFn)(
    uint32_t kind,
    BamEntity target,
    BamEntity hit_entity,
    uint64_t pointer_id,
    uint32_t button,
    uint32_t aux_u32,
    float f0,
    float f1,
    float f2
);

typedef struct PickingC99Mod {
    BamHostApi api;
    const BamHostCtx* host_ctx;
    RecordC99EventFn record;
    BamComponent cursor_comp;
    BamComponent echo_comp;
    BamComponent mouse_motion_res;
    BamComponent mouse_scroll_res;
    BamComponent gamepad_comp;
    BamComponent rumble_msg;
    uint32_t frame_count;
} PickingC99Mod;

static BamResult on_c99_press(BamEntity target, const void* data, BamCommands* cmds, void* ud) {
    PickingC99Mod* m = (PickingC99Mod*)ud;
    const FfiPointerPress* ev = (const FfiPointerPress*)data;
    (void)cmds;
    if (m != NULL && m->record != NULL && ev != NULL) {
        m->record(1u, target, ev->hit_entity, ev->pointer_id, (uint32_t)ev->button, (uint32_t)ev->count, ev->position[0], ev->position[1], ev->hit_depth);
    }
    return BAM_RESULT_OK;
}

static BamResult on_c99_release(BamEntity target, const void* data, BamCommands* cmds, void* ud) {
    PickingC99Mod* m = (PickingC99Mod*)ud;
    const FfiPointerRelease* ev = (const FfiPointerRelease*)data;
    (void)cmds;
    if (m != NULL && m->record != NULL && ev != NULL) {
        m->record(2u, target, ev->hit_entity, ev->pointer_id, (uint32_t)ev->button, (uint32_t)ev->pointer_kind, ev->position[0], ev->position[1], ev->hit_depth);
    }
    return BAM_RESULT_OK;
}

static BamResult on_c99_move(BamEntity target, const void* data, BamCommands* cmds, void* ud) {
    PickingC99Mod* m = (PickingC99Mod*)ud;
    const FfiPointerMove* ev = (const FfiPointerMove*)data;
    (void)cmds;
    if (m != NULL && m->record != NULL && ev != NULL) {
        m->record(3u, target, ev->hit_entity, ev->pointer_id, 0u, (uint32_t)ev->pointer_kind, ev->delta[0], ev->delta[1], ev->hit_depth);
    }
    return BAM_RESULT_OK;
}

static BamResult on_c99_click(BamEntity target, const void* data, BamCommands* cmds, void* ud) {
    PickingC99Mod* m = (PickingC99Mod*)ud;
    const FfiPointerClick* ev = (const FfiPointerClick*)data;
    if (m != NULL && m->record != NULL && ev != NULL) {
        m->record(4u, target, ev->hit_entity, ev->pointer_id, (uint32_t)ev->button, (uint32_t)ev->count, ev->position[0], (float)ev->duration_secs, ev->hit_depth);
        if (cmds != NULL && bam_component_is_valid(m->echo_comp) && target != BAM_ENTITY_NULL) {
            C99PickingEcho echo;
            echo.click_count = (uint32_t)ev->count;
            echo.button = (uint32_t)ev->button;
            m->api.cmd_insert(cmds, target, m->echo_comp, (const uint8_t*)&echo, sizeof(echo));
        }
    }
    return BAM_RESULT_OK;
}

static BamResult on_c99_over(BamEntity target, const void* data, BamCommands* cmds, void* ud) {
    PickingC99Mod* m = (PickingC99Mod*)ud;
    const FfiPointerOver* ev = (const FfiPointerOver*)data;
    if (m != NULL && m->record != NULL && ev != NULL) {
        m->record(5u, target, ev->hit_entity, ev->pointer_id, 0u, (uint32_t)ev->has_hit_position, ev->hit_position[0], ev->hit_position[1], ev->hit_depth);
        if (cmds != NULL && bam_component_is_valid(m->cursor_comp) && target != BAM_ENTITY_NULL) {
            FfiEntityCursor cur;
            cur.icon = 3u; /* SystemCursorIcon::Pointer */
            m->api.cmd_insert(cmds, target, m->cursor_comp, (const uint8_t*)&cur, sizeof(cur));
        }
    }
    return BAM_RESULT_OK;
}

static BamResult on_c99_out(BamEntity target, const void* data, BamCommands* cmds, void* ud) {
    PickingC99Mod* m = (PickingC99Mod*)ud;
    const FfiPointerOut* ev = (const FfiPointerOut*)data;
    (void)cmds;
    if (m != NULL && m->record != NULL && ev != NULL) {
        m->record(6u, target, ev->hit_entity, ev->pointer_id, 0u, (uint32_t)ev->has_hit_normal, ev->hit_normal[0], ev->hit_normal[1], ev->hit_depth);
    }
    return BAM_RESULT_OK;
}

static BamResult on_c99_drag(BamEntity target, const void* data, BamCommands* cmds, void* ud) {
    PickingC99Mod* m = (PickingC99Mod*)ud;
    const FfiPointerDrag* ev = (const FfiPointerDrag*)data;
    (void)cmds;
    if (m != NULL && m->record != NULL && ev != NULL) {
        m->record(7u, target, ev->hit_entity, ev->pointer_id, (uint32_t)ev->button, (uint32_t)ev->pointer_kind, ev->distance[0], ev->delta[0], ev->delta[1]);
    }
    return BAM_RESULT_OK;
}

static BamResult c99_input_probe_sys(BamSystemCtx* sys_ctx, void* ud) {
    PickingC99Mod* m = (PickingC99Mod*)ud;
    BamIter* res_iter = NULL;
    BamIter* gp_iter = NULL;
    const FfiMouseMotion* motion;
    const FfiMouseScroll* scroll;
    size_t chunk_len = 0u;

    if (m == NULL || m->record == NULL) {
        return BAM_RESULT_OK;
    }
    m->frame_count += 1u;

    if (m->api.sys_query(sys_ctx, 0u, &res_iter) != BAM_RESULT_OK || res_iter == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    motion = (const FfiMouseMotion*)m->api.res_ptr(res_iter, 0u);
    scroll = (const FfiMouseScroll*)m->api.res_ptr(res_iter, 1u);
    if (motion != NULL && scroll != NULL) {
        m->record(100u, BAM_ENTITY_NULL, BAM_ENTITY_NULL, 0ull, (uint32_t)scroll->unit, m->frame_count, motion->delta[0], motion->delta[1], scroll->delta[1]);
    }

    if (m->api.sys_query(sys_ctx, 1u, &gp_iter) != BAM_RESULT_OK || gp_iter == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    while (m->api.iter_next_chunk(gp_iter, &chunk_len) == BAM_TRUE) {
        const BamEntity* entities = m->api.iter_entities(gp_iter);
        const FfiGamepadState* states = (const FfiGamepadState*)m->api.iter_column_readonly(gp_iter, 0u);
        size_t i;
        for (i = 0u; i < chunk_len; ++i) {
            m->record(
                101u,
                entities[i],
                BAM_ENTITY_NULL,
                states[i].pressed,
                (uint32_t)states[i].connected,
                (uint32_t)states[i].just_pressed,
                states[i].left_stick[0],
                states[i].right_trigger,
                states[i].dpad[0]
            );
            if ((states[i].pressed & 1ull) != 0ull) {
                FfiGamepadRumble rumble;
                memset(&rumble, 0, sizeof(rumble));
                rumble.gamepad = entities[i];
                rumble.duration_secs = 0.25f;
                rumble.strong_motor = 0.75f;
                rumble.weak_motor = 0.25f;
                rumble.kind = 0u; /* KIND_ADD */
                m->api.msg_send(res_iter, 2u, (const uint8_t*)&rumble, sizeof(rumble));
            }
        }
    }
    return BAM_RESULT_OK;
}

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "c99_picking_input_mod",
    .version = "0.2.0",
    .flags = 0u,
    .reserved0 = 0u,
    .save_state = NULL,
    .restore_state = NULL
};

BAM_EXPORT const BamManifest* bevy_abi_mod_manifest(void) {
    return &MANIFEST;
}

static BamResult register_ptr_obs(PickingC99Mod* m, BamLoadCtx* load_ctx, const char* canonical, const char* obs_name, BamObserverFn cb) {
    BamComponent ev = BAM_COMPONENT_INVALID;
    BamObserverDesc desc;
    BamResult rc = m->api.resolve_component(m->host_ctx, canonical, NULL, &ev);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }
    desc = bam_observer_desc(ev, BAM_ENTITY_NULL, cb, m);
    desc.name = obs_name;
    return m->api.register_observer(load_ctx, &desc);
}

BAM_EXPORT BamResult bevy_abi_mod_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {
    PickingC99Mod* m;
    BamResult rc;
    BamTermDesc res_terms[3];
    BamTermDesc gp_term;
    BamQueryDesc queries[2];
    BamSystemDesc sys_desc;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;
    m = (PickingC99Mod*)calloc(1u, sizeof(PickingC99Mod));
    if (m == NULL) {
        return BAM_RESULT_INTERNAL_ERROR;
    }
    rc = bam_host_api_copy_bounded(&m->api, sizeof(m->api), api);
    if (rc != BAM_RESULT_OK) {
        free(m);
        return rc;
    }
    m->host_ctx = m->api.load_host_ctx(load_ctx);
    m->record = (RecordC99EventFn)m->api.get_proc_address(m->host_ctx, "bam.v0.picking_test.record_c99");

    if (m->api.resolve_component(m->host_ctx, "bevy_picking::cursor::EntityCursor", NULL, &m->cursor_comp) != BAM_RESULT_OK ||
        m->api.resolve_component(m->host_ctx, "picking_test::C99PickingEcho", NULL, &m->echo_comp) != BAM_RESULT_OK ||
        m->api.resolve_component(m->host_ctx, "bevy_input::mouse::AccumulatedMouseMotion", NULL, &m->mouse_motion_res) != BAM_RESULT_OK ||
        m->api.resolve_component(m->host_ctx, "bevy_input::mouse::AccumulatedMouseScroll", NULL, &m->mouse_scroll_res) != BAM_RESULT_OK ||
        m->api.resolve_component(m->host_ctx, "bevy_input::gamepad::Gamepad", NULL, &m->gamepad_comp) != BAM_RESULT_OK ||
        m->api.resolve_component(m->host_ctx, "bevy_input::gamepad::GamepadRumbleRequest", NULL, &m->rumble_msg) != BAM_RESULT_OK) {
        free(m);
        return BAM_RESULT_NOT_FOUND;
    }

    if ((rc = register_ptr_obs(m, load_ctx, "bevy_picking::events::PointerPress",   "c99_obs_press",   on_c99_press))   != BAM_RESULT_OK ||
        (rc = register_ptr_obs(m, load_ctx, "bevy_picking::events::PointerRelease", "c99_obs_release", on_c99_release)) != BAM_RESULT_OK ||
        (rc = register_ptr_obs(m, load_ctx, "bevy_picking::events::PointerMove",    "c99_obs_move",    on_c99_move))    != BAM_RESULT_OK ||
        (rc = register_ptr_obs(m, load_ctx, "bevy_picking::events::PointerClick",   "c99_obs_click",   on_c99_click))   != BAM_RESULT_OK ||
        (rc = register_ptr_obs(m, load_ctx, "bevy_picking::events::PointerOver",    "c99_obs_over",    on_c99_over))    != BAM_RESULT_OK ||
        (rc = register_ptr_obs(m, load_ctx, "bevy_picking::events::PointerOut",     "c99_obs_out",     on_c99_out))     != BAM_RESULT_OK ||
        (rc = register_ptr_obs(m, load_ctx, "bevy_picking::events::PointerDrag",    "c99_obs_drag",    on_c99_drag))    != BAM_RESULT_OK) {
        free(m);
        return rc;
    }

    res_terms[0] = bam_term_desc(m->mouse_motion_res, BAM_ACCESS_RESOURCE_READ, 0u);
    res_terms[1] = bam_term_desc(m->mouse_scroll_res, BAM_ACCESS_RESOURCE_READ, 0u);
    res_terms[2] = bam_term_desc(m->rumble_msg, BAM_ACCESS_MESSAGE_WRITE, 0u);
    gp_term = bam_term_desc(m->gamepad_comp, BAM_ACCESS_READ, 0u);

    memset(queries, 0, sizeof(queries));
    queries[0].struct_size = (uint32_t)sizeof(BamQueryDesc);
    queries[0].terms = res_terms;
    queries[0].term_count = 3u;
    queries[1].struct_size = (uint32_t)sizeof(BamQueryDesc);
    queries[1].terms = &gp_term;
    queries[1].term_count = 1u;

    memset(&sys_desc, 0, sizeof(sys_desc));
    sys_desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sys_desc.name = "c99_input_probe";
    sys_desc.queries = queries;
    sys_desc.query_count = 2u;
    sys_desc.callback = c99_input_probe_sys;
    sys_desc.user_data = m;
    sys_desc.schedule = BAM_SCHEDULE_UPDATE;

    rc = m->api.register_system(load_ctx, &sys_desc);
    if (rc != BAM_RESULT_OK) {
        free(m);
        return rc;
    }

    *instance_out = (BamInstance*)m;
    return BAM_RESULT_OK;
}

BAM_EXPORT void bevy_abi_mod_shutdown(BamInstance* instance) {
    free(instance);
}
"#;

    let src_path = temp.path().join("c99_picking_guest.c");
    std::fs::write(&src_path, c_source).expect("write C99 guest source");
    let lib_name = format!(
        "{}c99_picking_guest.{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_EXTENSION
    );
    let lib_path = temp.path().join(lib_name);
    compile_c99_shared_library(&cc, &include_dir, temp.path(), &src_path, &lib_path);

    C99_EVENT_LOG.lock().unwrap().clear();

    let mut app = App::new();
    app.add_plugins((
        InputPlugin,
        AbiModPlugin,
        ModPickingBridgePlugin,
        ModInputExtBridgePlugin,
    ));
    app.register_mod_component::<C99PickingEcho>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register C99PickingEcho");

    unsafe {
        app.world_mut()
            .resource_mut::<ModRegistry>()
            .register_custom_symbol(
                "bam.v0.picking_test.record_c99",
                record_c99_event as *const c_void,
            );
    }

    let mod_id = app.load_mod(&lib_path).expect("load C99 picking guest mod");

    let camera = app.world_mut().spawn_empty().id();
    let target = app.world_mut().spawn_empty().id();
    let mut gp = Gamepad::default();
    gp.digital_mut().press(GamepadButton::South);
    gp.analog_mut().set(GamepadAxis::LeftStickX, 0.5);
    gp.analog_mut().set(GamepadButton::RightTrigger2, 0.9);
    gp.analog_mut().set(GamepadButton::DPadRight, 1.0);
    let gp_entity = app.world_mut().spawn(gp).id();

    let ptr = synthetic_pointer(PointerId::Touch(99), Vec2::new(64.0, 128.0));
    let hit = synthetic_hit(camera, 2.25);

    // Dispatch all 7 flattened Bevy 0.20 picking events.
    app.world_mut().trigger(PointerOver {
        entity: target,
        pointer: ptr.clone(),
        hit: hit.clone(),
    });
    app.world_mut().trigger(PointerMove {
        entity: target,
        pointer: ptr.clone(),
        hit: hit.clone(),
        delta: Vec2::new(8.0, -4.0),
    });
    app.world_mut().trigger(PointerPress {
        entity: target,
        pointer: ptr.clone(),
        button: PointerButton::Primary,
        hit: hit.clone(),
        count: 1,
    });
    app.world_mut().trigger(PointerDrag {
        entity: target,
        pointer: ptr.clone(),
        button: PointerButton::Primary,
        distance: Vec2::new(20.0, 10.0),
        delta: Vec2::new(4.0, 2.0),
    });
    app.world_mut().trigger(PointerRelease {
        entity: target,
        pointer: ptr.clone(),
        button: PointerButton::Primary,
        hit: hit.clone(),
    });
    app.world_mut().trigger(PointerClick {
        entity: target,
        pointer: ptr.clone(),
        button: PointerButton::Secondary,
        hit: hit.clone(),
        duration: Duration::from_millis(250),
        count: 2,
    });
    app.world_mut().trigger(PointerOut {
        entity: target,
        pointer: ptr,
        hit,
    });

    // Feed mouse motion and scroll messages before `app.update()`.
    let win = app.world_mut().spawn(Window::default()).id();
    app.world_mut()
        .resource_mut::<Messages<MouseMotion>>()
        .write(MouseMotion {
            delta: Vec2::new(9.5, -3.5),
        });
    app.world_mut()
        .resource_mut::<Messages<MouseWheel>>()
        .write(MouseWheel {
            unit: MouseScrollUnit::Pixel,
            x: 1.0,
            y: -16.0,
            window: win,
            phase: TouchPhase::Moved,
        });

    app.update();

    let target_bits = target.to_bits();
    let gp_bits = gp_entity.to_bits();
    let events = C99_EVENT_LOG.lock().unwrap().clone();

    // Verify all 7 flattened pointer events arrived in exact order with exact C99 wire payloads!
    let find_kind = |k: u32| -> C99RecordedEvent {
        events
            .iter()
            .find(|e| e.kind == k)
            .cloned()
            .unwrap_or_else(|| panic!("missing C99 event kind {k}; logged = {events:?}"))
    };

    let over = find_kind(5);
    assert_eq!(over.target, target_bits);
    assert_eq!(over.hit_entity, target_bits);
    assert_eq!(over.pointer_id, 99);
    assert_eq!(over.aux_u32, 1); // has_hit_position == true
    assert!((over.f0 - 1.5).abs() < 1e-6);
    assert!((over.f1 - 2.5).abs() < 1e-6);
    assert!((over.f2 - 2.25).abs() < 1e-6);

    let mv = find_kind(3);
    assert!((mv.f0 - 8.0).abs() < 1e-6);
    assert!((mv.f1 - (-4.0)).abs() < 1e-6);
    assert!((mv.f2 - 2.25).abs() < 1e-6);

    let press = find_kind(1);
    assert_eq!(press.button, FfiPointerButton::PRIMARY_RAW as u32);
    assert_eq!(press.aux_u32, 1);
    assert!((press.f0 - 64.0).abs() < 1e-6);
    assert!((press.f1 - 128.0).abs() < 1e-6);
    assert!((press.f2 - 2.25).abs() < 1e-6);

    let drag = find_kind(7);
    assert_eq!(drag.button, FfiPointerButton::PRIMARY_RAW as u32);
    assert!((drag.f0 - 20.0).abs() < 1e-6);
    assert!((drag.f1 - 4.0).abs() < 1e-6);
    assert!((drag.f2 - 2.0).abs() < 1e-6);

    let rel = find_kind(2);
    assert_eq!(rel.button, FfiPointerButton::PRIMARY_RAW as u32);

    let click = find_kind(4);
    assert_eq!(click.button, FfiPointerButton::SECONDARY_RAW as u32);
    assert_eq!(click.aux_u32, 2);
    assert!((click.f1 - 0.25).abs() < 1e-5);

    let out = find_kind(6);
    assert_eq!(out.aux_u32, 1); // has_hit_normal == true
    assert!((out.f1 - 1.0).abs() < 1e-6);

    // Verify C99 observer `cmd_insert` mutations synced to Bevy host!
    assert_eq!(
        app.world().get::<C99PickingEcho>(target),
        Some(&C99PickingEcho {
            click_count: 2,
            button: FfiPointerButton::SECONDARY_RAW as u32,
        })
    );
    assert_eq!(
        app.world().get::<EntityCursor>(target),
        Some(&EntityCursor::System(SystemCursorIcon::Pointer))
    );

    // Verify C99 Update system read MouseMotion, MouseScroll, GamepadState & sent GamepadRumble!
    let mouse_ev = find_kind(100);
    assert_eq!(mouse_ev.button, FfiMouseScroll::UNIT_PIXEL as u32);
    assert!((mouse_ev.f0 - 9.5).abs() < 1e-6);
    assert!((mouse_ev.f1 - (-3.5)).abs() < 1e-6);
    assert!((mouse_ev.f2 - (-16.0)).abs() < 1e-6);

    let gp_ev = find_kind(101);
    assert_eq!(gp_ev.target, gp_bits);
    assert_eq!(gp_ev.pointer_id, FfiGamepadButton::SOUTH.bit());
    assert_eq!(gp_ev.button, 1); // connected
    assert!((gp_ev.f0 - 0.5).abs() < 1e-6);
    assert!((gp_ev.f1 - 0.9).abs() < 1e-6);
    assert!((gp_ev.f2 - 1.0).abs() < 1e-6);

    // Verify C99 `FfiGamepadRumble` was forwarded into Bevy's `Messages<GamepadRumbleRequest>`.
    let rumbles = app.world().resource::<Messages<GamepadRumbleRequest>>();
    let mut cursor = rumbles.get_cursor();
    let forwarded: Vec<GamepadRumbleRequest> = cursor.read(rumbles).cloned().collect();
    assert_eq!(forwarded.len(), 1);
    match &forwarded[0] {
        GamepadRumbleRequest::Add {
            duration,
            intensity,
            gamepad,
        } => {
            assert_eq!(*gamepad, gp_entity);
            assert!((duration.as_secs_f32() - 0.25).abs() < 1e-6);
            assert_eq!(
                *intensity,
                GamepadRumbleIntensity {
                    strong_motor: 0.75,
                    weak_motor: 0.25,
                }
            );
        }
        GamepadRumbleRequest::Stop { .. } => panic!("expected Add rumble request from C99 guest"),
    }

    app.unload_mod(mod_id)
        .expect("unload C99 picking guest mod");
}

#[test]
fn entity_cursor_bidirectional_sync_and_removal_round_trip() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModPickingBridgePlugin));

    let host_entity = app
        .world_mut()
        .spawn(EntityCursor::System(SystemCursorIcon::Grab))
        .id();
    let guest_target = app.world_mut().spawn_empty().id();

    let reg = app.world().resource::<ModRegistry>().clone();
    let cursor_h = reg
        .get_by_name(CANONICAL_ENTITY_CURSOR_NAME)
        .unwrap()
        .handle;

    struct CursorGuestState {
        tick: u32,
        cursor_h: BamComponent,
        guest_target: FfiEntity,
        host_entity: FfiEntity,
        seen_on_host_entity: Vec<FfiEntityCursor>,
    }

    let state = Arc::new(Mutex::new(CursorGuestState {
        tick: 0,
        cursor_h,
        guest_target: FfiEntity::from_bits(guest_target.to_bits()),
        host_entity: FfiEntity::from_bits(host_entity.to_bits()),
        seen_on_host_entity: Vec::new(),
    }));

    unsafe extern "C" fn cursor_guest_sys(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let mut st = unsafe { &*(user_data as *const Mutex<CursorGuestState>) }
            .lock()
            .unwrap();
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );
        let mut cmds = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_commands(sys_ctx, &mut cmds) },
            FfiResult::OK
        );

        let mut chunk_len = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut chunk_len) } == FfiBool::TRUE {
            let entities =
                unsafe { core::slice::from_raw_parts(host_iter_entities(iter), chunk_len) };
            let col = unsafe {
                core::slice::from_raw_parts_mut(
                    host_iter_column_mutable(iter, 0) as *mut FfiEntityCursor,
                    chunk_len,
                )
            };
            for (ent, cur) in entities.iter().zip(col.iter_mut()) {
                if *ent == st.host_entity {
                    st.seen_on_host_entity.push(*cur);
                    if st.tick == 1 {
                        // Mutate host-owned entity's cursor to `Crosshair`.
                        *cur = FfiEntityCursor::CROSSHAIR;
                    } else if st.tick == 2 {
                        // Write an invalid discriminant — must NOT corrupt host `EntityCursor`.
                        *cur = FfiEntityCursor { icon: 250 };
                    }
                }
            }
        }

        if st.tick == 0 {
            let zoom_in = FfiEntityCursor::ZOOM_IN;
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        cmds,
                        st.guest_target,
                        st.cursor_h,
                        core::ptr::from_ref(&zoom_in).cast::<u8>(),
                        core::mem::size_of::<FfiEntityCursor>(),
                    )
                },
                FfiResult::OK
            );
        } else if st.tick == 3 {
            assert_eq!(
                unsafe { host_cmd_remove(cmds, st.guest_target, st.cursor_h) },
                FfiResult::OK
            );
        }

        st.tick += 1;
        FfiResult::OK
    }

    let sys = GuestSystem::new(
        "cursor_sync_guest",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            cursor_h,
            AccessMode::WRITE,
        )])],
        cursor_guest_sys,
        Arc::as_ptr(&state) as *mut c_void,
        reg,
    );
    app.add_systems(Update, sys);

    // Frame 0: Host `EntityCursor::Grab` synced to `FfiEntityCursor::GRAB` before guest `Update`;
    // guest inserts `FfiEntityCursor::ZOOM_IN` on `guest_target` -> synced to `EntityCursor::ZoomIn`.
    app.update();
    assert_eq!(
        state.lock().unwrap().seen_on_host_entity[0],
        FfiEntityCursor::GRAB
    );
    assert_eq!(
        app.world().get::<EntityCursor>(guest_target),
        Some(&EntityCursor::System(SystemCursorIcon::ZoomIn))
    );

    // Frame 1: Guest mutates `host_entity` to `CROSSHAIR` -> synced to `EntityCursor::Crosshair`.
    app.update();
    assert_eq!(
        app.world().get::<EntityCursor>(host_entity),
        Some(&EntityCursor::System(SystemCursorIcon::Crosshair))
    );

    // Frame 2: Guest writes invalid icon `250` -> ignored safely; host `EntityCursor` stays `Crosshair`.
    app.update();
    assert_eq!(
        app.world().get::<EntityCursor>(host_entity),
        Some(&EntityCursor::System(SystemCursorIcon::Crosshair))
    );

    // Frame 3: Guest removes `FfiEntityCursor` from `guest_target` -> `EntityCursor` removed too;
    // host removes `EntityCursor` from `host_entity` before Frame 4 -> `FfiEntityCursor` removed!
    app.update();
    assert!(app.world().get::<EntityCursor>(guest_target).is_none());

    app.world_mut()
        .entity_mut(host_entity)
        .remove::<EntityCursor>();
    app.update();
    assert!(app.world().get::<FfiEntityCursor>(host_entity).is_none());
}

#[test]
fn mouse_motion_and_scroll_sync_across_frames_with_input_plugin() {
    let mut app = App::new();
    app.add_plugins((InputPlugin, AbiModPlugin, ModInputExtBridgePlugin));

    let reg = app.world().resource::<ModRegistry>().clone();
    let motion_h = reg.get_by_name(CANONICAL_MOUSE_MOTION_NAME).unwrap().handle;
    let scroll_h = reg.get_by_name(CANONICAL_MOUSE_SCROLL_NAME).unwrap().handle;
    let win = app.world_mut().spawn(Window::default()).id();

    let observed: Arc<Mutex<Vec<(FfiMouseMotion, FfiMouseScroll)>>> =
        Arc::new(Mutex::new(Vec::new()));

    unsafe extern "C" fn read_mouse_ext_sys(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let log = unsafe { &*(user_data as *const Mutex<Vec<(FfiMouseMotion, FfiMouseScroll)>>) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );
        let motion = unsafe { *(host_res_ptr(iter, 0) as *const FfiMouseMotion) };
        let scroll = unsafe { *(host_res_ptr(iter, 1) as *const FfiMouseScroll) };
        log.lock().unwrap().push((motion, scroll));
        FfiResult::OK
    }

    let sys = GuestSystem::new(
        "mouse_ext_reader",
        vec![BamQuerySpecOwned::new(vec![
            BamTermDesc::new(motion_h, AccessMode::RESOURCE_READ),
            BamTermDesc::new(scroll_h, AccessMode::RESOURCE_READ),
        ])],
        read_mouse_ext_sys,
        Arc::as_ptr(&observed) as *mut c_void,
        reg,
    );
    app.add_systems(Update, sys);

    // Frame 1: two MouseMotion messages sum + Pixel scroll.
    {
        let mut mm = app.world_mut().resource_mut::<Messages<MouseMotion>>();
        mm.write(MouseMotion {
            delta: Vec2::new(7.5, -2.0),
        });
        mm.write(MouseMotion {
            delta: Vec2::new(5.0, -5.25),
        });
        app.world_mut()
            .resource_mut::<Messages<MouseWheel>>()
            .write(MouseWheel {
                unit: MouseScrollUnit::Pixel,
                x: 3.0,
                y: -18.0,
                window: win,
                phase: TouchPhase::Moved,
            });
    }
    app.update();

    // Frame 2: no motion (resets to 0), Line scroll.
    app.world_mut()
        .resource_mut::<Messages<MouseWheel>>()
        .write(MouseWheel {
            unit: MouseScrollUnit::Line,
            x: 0.0,
            y: 2.0,
            window: win,
            phase: TouchPhase::Moved,
        });
    app.update();

    // Frame 3: idle frame (both reset to zero).
    app.update();

    let log = observed.lock().unwrap();
    assert_eq!(log.len(), 3);

    assert_eq!(log[0].0.delta_vec2(), Vec2::new(12.5, -7.25));
    assert_eq!(log[0].1.delta_vec2(), Vec2::new(3.0, -18.0));
    assert_eq!(log[0].1.scroll_unit(), Some(MouseScrollUnit::Pixel));

    assert_eq!(log[1].0.delta_vec2(), Vec2::ZERO);
    assert_eq!(log[1].1.delta_vec2(), Vec2::new(0.0, 2.0));
    assert_eq!(log[1].1.scroll_unit(), Some(MouseScrollUnit::Line));

    assert_eq!(log[2].0.delta_vec2(), Vec2::ZERO);
    assert_eq!(log[2].1.delta_vec2(), Vec2::ZERO);
}

#[test]
fn gamepad_entity_buttons_axes_disconnect_and_rumble_round_trip() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModInputExtBridgePlugin));

    let reg = app.world().resource::<ModRegistry>().clone();
    let gp_entry = reg.get_by_name(CANONICAL_GAMEPAD_STATE_NAME).unwrap();
    assert!(gp_entry.is_immutable());
    let gp_h = gp_entry.handle;
    let rumble_h = reg
        .get_by_name(CANONICAL_GAMEPAD_RUMBLE_NAME)
        .unwrap()
        .handle;

    let mut gp1 = Gamepad::default();
    gp1.digital_mut().press(GamepadButton::South);
    gp1.digital_mut().press(GamepadButton::North);
    gp1.digital_mut().release(GamepadButton::North);
    gp1.digital_mut().press(GamepadButton::LeftTrigger);
    gp1.analog_mut().set(GamepadAxis::LeftStickX, 0.75);
    gp1.analog_mut().set(GamepadAxis::LeftStickY, -0.5);
    gp1.analog_mut().set(GamepadAxis::RightStickX, -0.25);
    gp1.analog_mut().set(GamepadAxis::RightStickY, 0.9);
    gp1.analog_mut().set(GamepadButton::LeftTrigger2, 0.6);
    gp1.analog_mut().set(GamepadButton::RightTrigger2, 0.85);
    gp1.analog_mut().set(GamepadAxis::LeftZ, 0.2);
    gp1.analog_mut().set(GamepadAxis::RightZ, -0.3);
    gp1.analog_mut().set(GamepadButton::DPadUp, 1.0);
    gp1.analog_mut().set(GamepadButton::DPadLeft, 1.0);

    let gp1_entity = app.world_mut().spawn(gp1).id();
    let gp2_entity = app.world_mut().spawn(Gamepad::default()).id();
    let non_gamepad_entity = app.world_mut().spawn_empty().id();

    struct GamepadTestState {
        tick: u32,
        gp1: FfiEntity,
        non_gp: FfiEntity,
        snapshots: Vec<(FfiEntity, FfiGamepadState)>,
    }

    let state = Arc::new(Mutex::new(GamepadTestState {
        tick: 0,
        gp1: FfiEntity::from_bits(gp1_entity.to_bits()),
        non_gp: FfiEntity::from_bits(non_gamepad_entity.to_bits()),
        snapshots: Vec::new(),
    }));

    unsafe extern "C" fn gamepad_guest_sys(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let mut st = unsafe { &*(user_data as *const Mutex<GamepadTestState>) }
            .lock()
            .unwrap();
        let mut gp_iter: *mut BamIter = core::ptr::null_mut();
        let mut msg_iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut gp_iter) },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 1, &mut msg_iter) },
            FfiResult::OK
        );

        let mut chunk_len = 0usize;
        while unsafe { host_iter_next_chunk(gp_iter, &mut chunk_len) } == FfiBool::TRUE {
            let entities =
                unsafe { core::slice::from_raw_parts(host_iter_entities(gp_iter), chunk_len) };
            let col = unsafe {
                core::slice::from_raw_parts(
                    host_iter_column_readonly(gp_iter, 0) as *const FfiGamepadState,
                    chunk_len,
                )
            };
            for (&ent, &gp_state) in entities.iter().zip(col.iter()) {
                st.snapshots.push((ent, gp_state));
            }
        }

        if st.tick == 0 {
            let requests = [
                FfiGamepadRumble::stop(st.gp1),
                FfiGamepadRumble::add(st.gp1, 0.4, 0.8, 0.3),
                // Should be ignored (entity has no `Gamepad` component).
                FfiGamepadRumble::add(st.non_gp, 0.5, 1.0, 1.0),
                // Should be ignored (negative duration).
                FfiGamepadRumble::add(st.gp1, -0.5, 1.0, 1.0),
            ];
            for req in requests {
                assert_eq!(
                    unsafe {
                        host_msg_send(
                            msg_iter,
                            0,
                            core::ptr::from_ref(&req).cast::<u8>(),
                            core::mem::size_of::<FfiGamepadRumble>(),
                        )
                    },
                    FfiResult::OK
                );
            }
        }
        st.tick += 1;
        FfiResult::OK
    }

    let sys = GuestSystem::new(
        "gamepad_guest",
        vec![
            BamQuerySpecOwned::new(vec![BamTermDesc::new(gp_h, AccessMode::READ)]),
            BamQuerySpecOwned::new(vec![BamTermDesc::new(rumble_h, AccessMode::MESSAGE_WRITE)]),
        ],
        gamepad_guest_sys,
        Arc::as_ptr(&state) as *mut c_void,
        reg,
    );
    app.add_systems(Update, sys);

    app.update();

    let gp1_ffi = FfiEntity::from_bits(gp1_entity.to_bits());
    let gp2_ffi = FfiEntity::from_bits(gp2_entity.to_bits());

    {
        let st = state.lock().unwrap();
        assert_eq!(st.snapshots.len(), 2);

        let gp1_snap = st
            .snapshots
            .iter()
            .find(|(e, _)| *e == gp1_ffi)
            .expect("gp1 synced")
            .1;
        assert!(gp1_snap.connected.get());
        assert!(gp1_snap.pressed(FfiGamepadButton::SOUTH));
        assert!(gp1_snap.just_pressed(FfiGamepadButton::SOUTH));
        assert!(gp1_snap.pressed(FfiGamepadButton::LEFT_TRIGGER));
        assert!(!gp1_snap.pressed(FfiGamepadButton::NORTH));
        assert!(gp1_snap.just_released(FfiGamepadButton::NORTH));
        assert_eq!(gp1_snap.left_stick, [0.75, -0.5]);
        assert_eq!(gp1_snap.right_stick, [-0.25, 0.9]);
        assert!((gp1_snap.left_trigger - 0.6).abs() < 1e-6);
        assert!((gp1_snap.right_trigger - 0.85).abs() < 1e-6);
        assert!((gp1_snap.left_z - 0.2).abs() < 1e-6);
        assert!((gp1_snap.right_z - (-0.3)).abs() < 1e-6);
        assert_eq!(gp1_snap.dpad, [-1.0, 1.0]);

        let gp2_snap = st
            .snapshots
            .iter()
            .find(|(e, _)| *e == gp2_ffi)
            .expect("gp2 synced")
            .1;
        assert!(gp2_snap.connected.get());
        assert_eq!(gp2_snap.pressed, 0);
        assert_eq!(gp2_snap.left_stick, [0.0, 0.0]);
    }

    // Verify forwarded Bevy `GamepadRumbleRequest` messages (only the 2 valid ones in FIFO order).
    let rumbles = app.world().resource::<Messages<GamepadRumbleRequest>>();
    let mut cursor = rumbles.get_cursor();
    let forwarded: Vec<GamepadRumbleRequest> = cursor.read(rumbles).cloned().collect();
    assert_eq!(forwarded.len(), 2);
    assert!(matches!(
        forwarded[0],
        GamepadRumbleRequest::Stop { gamepad } if gamepad == gp1_entity
    ));
    match &forwarded[1] {
        GamepadRumbleRequest::Add {
            duration,
            intensity,
            gamepad,
        } => {
            assert_eq!(*gamepad, gp1_entity);
            assert!((duration.as_secs_f32() - 0.4).abs() < 1e-6);
            assert_eq!(
                *intensity,
                GamepadRumbleIntensity {
                    strong_motor: 0.8,
                    weak_motor: 0.3,
                }
            );
        }
        GamepadRumbleRequest::Stop { .. } => panic!("expected Add as second rumble request"),
    }

    // Remove `Gamepad` from `gp2_entity` -> `FfiGamepadState` must be removed on next frame.
    app.world_mut().entity_mut(gp2_entity).remove::<Gamepad>();
    app.update();
    assert!(app.world().get::<FfiGamepadState>(gp2_entity).is_none());
    assert!(app.world().get::<FfiGamepadState>(gp1_entity).is_some());
}
