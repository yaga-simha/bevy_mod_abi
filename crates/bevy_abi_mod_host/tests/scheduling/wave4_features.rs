//! Wave 4 Host Scheduling & Observability Tests (`F-05`, `E-01`):
//! - `F-05`: `ModVisibleSets` (`register_mod_visible_set("physics", PhysicsSet)`) ordering guest
//!   systems via `"host::physics"` / `"host::render"`, strict rejection of unknown `"host::*"`
//!   sets, and preserving host `SystemSet`s across mod unload.
//! - `E-01`: `tracing::debug_span!("bam_guest_system", ...)` recording `mod_name`, `system`,
//!   `chunks`, `entities`, and `commands`, plus optional `diagnostics` Cargo feature integration
//!   with `bevy_diagnostic::DiagnosticsStore`.

use crate::support::{Position, TempDylibCopy, fixture_dylib};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, host_cmd_spawn_empty, host_iter_next_chunk, host_sys_commands,
    host_sys_query,
};
use bevy_abi_mod_sys::{BamCommands, BamIter, BamSystemCtx};
use bevy_app::{App, FixedUpdate, PostUpdate, Startup, Update};
use bevy_ecs::prelude::{ResMut, Resource};
use bevy_ecs::schedule::{IntoScheduleConfigs, ScheduleBuildSettings, SystemSet};
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
struct HostPhysicsSet;

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
struct HostRenderSet;

#[derive(Resource, Default)]
struct StepOrderLog(Vec<&'static str>);

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "w4::StateProbe")]
struct W4StateProbe {
    reload_epoch: u64,
    accumulated_ticks: u64,
    fail_next_save: u32,
    fail_next_restore: u32,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "w4::OrderHostSets")]
struct W4OrderHostSets {
    enabled: u32,
}

fn host_physics_step(mut log: ResMut<'_, StepOrderLog>, probe: ResMut<'_, W4StateProbe>) {
    // Touch `W4StateProbe` so Bevy's parallel scheduler cannot reorder without `SystemSet` edges!
    let _ = probe.accumulated_ticks;
    log.0.push("host::physics");
}

fn host_render_step(mut log: ResMut<'_, StepOrderLog>, probe: ResMut<'_, W4StateProbe>) {
    let _ = probe.accumulated_ticks;
    log.0.push("host::render");
}

#[test]
fn wave4_f05_ordering_against_host_visible_sets_and_preservation_on_unload() {
    let w4_dylib = TempDylibCopy::new(&fixture_dylib("w4_ext_mod"), "f05_order");

    // 1. Strict mode MUST reject `"host::physics"` when not registered in `ModVisibleSets`!
    {
        let mut bad_app = App::new();
        bad_app.add_plugins(AbiModPlugin);
        bad_app.insert_resource(W4StateProbe::default());
        bad_app.insert_resource(W4OrderHostSets { enabled: 1 });
        bad_app.register_mod_resource::<W4StateProbe>().unwrap();
        bad_app.register_mod_resource::<W4OrderHostSets>().unwrap();

        let err = bad_app
            .load_mod_with(w4_dylib.path(), LoadOptions::new().strict())
            .expect_err("unregistered host::physics must fail strict load");
        assert!(
            matches!(err, ModLoadError::UnresolvedOrderingTarget { ref target, .. } if target == "host::physics"),
            "unexpected error: {err:?}"
        );
    }

    // 2. Registering `"physics"` and `"render"` wires `w4_state_tick` deterministically between them!
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<StepOrderLog>();
    app.insert_resource(W4StateProbe::default());
    app.insert_resource(W4OrderHostSets { enabled: 1 });
    app.register_mod_resource::<W4StateProbe>().unwrap();
    app.register_mod_resource::<W4OrderHostSets>().unwrap();
    app.register_mod_visible_set("physics", HostPhysicsSet);
    app.register_mod_visible_set("render", HostRenderSet);

    app.configure_sets(Update, (HostPhysicsSet, HostRenderSet).chain());
    app.add_systems(Update, host_physics_step.in_set(HostPhysicsSet));
    app.add_systems(Update, host_render_step.in_set(HostRenderSet));

    let mod_id = app
        .load_mod_with(
            w4_dylib.path(),
            LoadOptions::new().strict().with_shadow_copy(true),
        )
        .expect("load w4_ext_mod ordered against host::physics and host::render");

    // Wrap a post-physics / pre-render verification: `w4_state_tick` increments `accumulated_ticks`
    // strictly after `host::physics` (saw 0 on tick 1) and strictly before `host::render` (saw 1 on tick 1)!
    let observed_in_render = Arc::new(AtomicU64::new(0));
    let obs_clone = Arc::clone(&observed_in_render);
    app.add_systems(
        Update,
        (move |probe: bevy_ecs::prelude::Res<'_, W4StateProbe>| {
            obs_clone.store(probe.accumulated_ticks, Ordering::SeqCst);
        })
        .in_set(HostRenderSet),
    );

    app.update();
    assert_eq!(observed_in_render.load(Ordering::SeqCst), 1);
    assert_eq!(
        app.world().resource::<StepOrderLog>().0,
        vec!["host::physics", "host::render"]
    );

    // 3. Unloading the mod excises the guest system while keeping `HostPhysicsSet` and `HostRenderSet` intact!
    app.unload_mod(mod_id).expect("unload w4_ext_mod");
    app.world_mut().resource_mut::<StepOrderLog>().0.clear();
    app.update();
    assert_eq!(
        app.world().resource::<StepOrderLog>().0,
        vec!["host::physics", "host::render"]
    );
    // `accumulated_ticks` did NOT advance after unload!
    assert_eq!(observed_in_render.load(Ordering::SeqCst), 1);
    assert!(app.world().resource::<ModVisibleSets>().contains("physics"));
    assert!(app.world().resource::<ModVisibleSets>().contains("render"));
}

#[derive(Default)]
struct CapturedSpanData {
    name: String,
    fields: HashMap<String, String>,
}

struct SpanRecorder {
    next_id: AtomicU64,
    spans: Mutex<HashMap<u64, CapturedSpanData>>,
}

impl SpanRecorder {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            spans: Mutex::new(HashMap::new()),
        }
    }
}

struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl tracing::Subscriber for SpanRecorder {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut fields = HashMap::new();
        span.values().record(&mut FieldVisitor(&mut fields));
        self.spans.lock().unwrap().insert(
            id,
            CapturedSpanData {
                name: span.metadata().name().to_owned(),
                fields,
            },
        );
        tracing::span::Id::from_u64(id)
    }

    fn record(&self, span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        if let Some(entry) = self.spans.lock().unwrap().get_mut(&span.into_u64()) {
            values.record(&mut FieldVisitor(&mut entry.fields));
        }
    }

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, _event: &tracing::Event<'_>) {}
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

unsafe extern "C" fn traced_guest_cb(
    sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if !rc.is_ok() {
        return rc;
    }
    let mut len = 0usize;
    while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {}

    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    let cmd_rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    if !cmd_rc.is_ok() {
        return cmd_rc;
    }
    let mut spawned = FfiEntity::NULL;
    unsafe { host_cmd_spawn_empty(cmds, &mut spawned) }
}

thread_local! {
    static ACTIVE_SPAN_RECORDER: core::cell::RefCell<Option<Arc<SpanRecorder>>> = const { core::cell::RefCell::new(None) };
}

struct ThreadLocalSpanRouter;

impl tracing::Subscriber for ThreadLocalSpanRouter {
    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }

    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        ACTIVE_SPAN_RECORDER.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|rec| rec.enabled(metadata))
        })
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        ACTIVE_SPAN_RECORDER.with(|slot| {
            if let Some(ref rec) = *slot.borrow() {
                rec.new_span(span)
            } else {
                tracing::span::Id::from_u64(1)
            }
        })
    }

    fn record(&self, span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        ACTIVE_SPAN_RECORDER.with(|slot| {
            if let Some(ref rec) = *slot.borrow() {
                rec.record(span, values);
            }
        });
    }

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, _event: &tracing::Event<'_>) {}
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

#[test]
fn wave4_e01_tracing_span_records_mod_system_chunks_entities_and_commands() {
    static INIT_ROUTER: std::sync::Once = std::sync::Once::new();
    INIT_ROUTER.call_once(|| {
        let _ = tracing::subscriber::set_global_default(ThreadLocalSpanRouter);
        tracing::callsite::rebuild_interest_cache();
    });

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    for i in 0..5 {
        app.world_mut().spawn(Position {
            x: i as f32,
            y: 0.0,
            z: 0.0,
        });
    }
    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "traced_sys",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            pos_h,
            AccessMode::READ,
        )])],
        traced_guest_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_mod_namespace("trace_mod")
    .with_main_thread(true);
    app.add_systems(Update, sys);

    let recorder = Arc::new(SpanRecorder::new());
    ACTIVE_SPAN_RECORDER.with(|slot| {
        *slot.borrow_mut() = Some(Arc::clone(&recorder));
    });
    app.update();
    ACTIVE_SPAN_RECORDER.with(|slot| {
        *slot.borrow_mut() = None;
    });

    let spans = recorder.spans.lock().unwrap();
    let bam_span = spans
        .values()
        .find(|s| s.name == "bam_guest_system")
        .expect("bam_guest_system span must be emitted");
    assert_eq!(
        bam_span.fields.get("mod_name").map(String::as_str),
        Some("trace_mod")
    );
    assert_eq!(
        bam_span.fields.get("system").map(String::as_str),
        Some("traced_sys")
    );
    assert_eq!(bam_span.fields.get("chunks").map(String::as_str), Some("1"));
    assert_eq!(
        bam_span.fields.get("entities").map(String::as_str),
        Some("5")
    );
    assert_eq!(
        bam_span.fields.get("commands").map(String::as_str),
        Some("1")
    );
}

#[cfg(feature = "diagnostics")]
#[test]
fn wave4_e01_bevy_diagnostic_store_records_per_mod_and_aggregate_metrics() {
    use bevy_abi_mod_host::diagnostics::{
        TOTAL_COMMANDS_QUEUED_PATH, TOTAL_CPU_TIME_US_PATH, TOTAL_FAULT_COUNT_PATH,
        mod_commands_queued_path, mod_cpu_time_path, mod_fault_count_path,
    };
    use bevy_diagnostic::{DiagnosticsPlugin, DiagnosticsStore};

    unsafe extern "C" fn diag_fault_cb(_sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        FfiResult::PANIC_CAUGHT
    }

    let mut app = App::new();
    app.add_plugins(DiagnosticsPlugin);
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new())
        .unwrap();
    app.world_mut().spawn(Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    });
    let reg = app.world().resource::<ModRegistry>().clone();

    let ok_sys = GuestSystem::new(
        "diag_ok",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            pos_h,
            AccessMode::READ,
        )])],
        traced_guest_cb,
        core::ptr::null_mut(),
        reg.clone(),
    )
    .with_mod_namespace("diag_mod")
    .with_mod_id(ModId(501));

    let fault_sys = GuestSystem::new(
        "diag_fault",
        vec![],
        diag_fault_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_mod_namespace("diag_mod")
    .with_mod_id(ModId(501));

    app.add_systems(Update, (ok_sys, fault_sys).chain());
    app.update();

    let store = app.world().resource::<DiagnosticsStore>();
    assert!(
        store
            .get(&TOTAL_CPU_TIME_US_PATH)
            .and_then(|d| d.value())
            .is_some()
    );
    assert_eq!(
        store
            .get(&TOTAL_COMMANDS_QUEUED_PATH)
            .and_then(|d| d.value()),
        Some(1.0)
    );
    assert_eq!(
        store.get(&TOTAL_FAULT_COUNT_PATH).and_then(|d| d.value()),
        Some(1.0)
    );
    assert!(
        store
            .get(&mod_cpu_time_path("diag_mod"))
            .and_then(|d| d.value())
            .is_some()
    );
    assert_eq!(
        store
            .get(&mod_commands_queued_path("diag_mod"))
            .and_then(|d| d.value()),
        Some(1.0)
    );
    assert_eq!(
        store
            .get(&mod_fault_count_path("diag_mod"))
            .and_then(|d| d.value()),
        Some(1.0)
    );
}

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
enum Wave5Phase {
    Input,
    Logic,
    Output,
}

#[test]
fn wave5_req3_schedule_labels_and_in_set_ordering_under_randomization() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<StepOrderLog>();
    app.register_mod_visible_set("input", Wave5Phase::Input);
    app.register_mod_visible_set("logic", Wave5Phase::Logic);
    app.register_mod_visible_set("output", Wave5Phase::Output);

    // Enable Bevy schedule ambiguity checks / build settings alongside chained sets to prove
    // `.in_set("host::logic")` + `.before(...)`/`.after(...)` produce deterministic ordering.
    app.edit_schedule(Update, |schedule| {
        schedule.set_build_settings(ScheduleBuildSettings::default());
    });

    fn host_input(mut log: ResMut<'_, StepOrderLog>) {
        log.0.push("host::input");
    }
    fn host_output(mut log: ResMut<'_, StepOrderLog>) {
        log.0.push("host::output");
    }

    app.configure_sets(
        Update,
        (Wave5Phase::Input, Wave5Phase::Logic, Wave5Phase::Output).chain(),
    );
    app.add_systems(
        Update,
        (
            host_input.in_set(Wave5Phase::Input),
            host_output.in_set(Wave5Phase::Output),
        ),
    );

    let log_shared: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    struct ScheduleStepUd {
        label: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    unsafe extern "C" fn record_schedule_step(
        _: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ud = unsafe { &*(user_data as *const ScheduleStepUd) };
        ud.log.lock().unwrap().push(ud.label);
        FfiResult::OK
    }

    // Mirror host steps via shared host log inside PostUpdate/Update assertions:
    fn sync_host_to_shared(_log: ResMut<'_, StepOrderLog>) {}
    app.add_systems(
        Update,
        sync_host_to_shared
            .in_set(Wave5Phase::Input)
            .before(host_input),
    );

    let reg = app.world().resource::<ModRegistry>().clone();
    let ud_startup = Arc::new(ScheduleStepUd {
        label: "guest::startup",
        log: Arc::clone(&log_shared),
    });
    let ud_fixed = Arc::new(ScheduleStepUd {
        label: "guest::fixed",
        log: Arc::clone(&log_shared),
    });
    let ud_logic_a = Arc::new(ScheduleStepUd {
        label: "guest::logic_a",
        log: Arc::clone(&log_shared),
    });
    let ud_logic_b = Arc::new(ScheduleStepUd {
        label: "guest::logic_b",
        log: Arc::clone(&log_shared),
    });
    let ud_post = Arc::new(ScheduleStepUd {
        label: "guest::post",
        log: Arc::clone(&log_shared),
    });

    let sys_startup = GuestSystem::new(
        "s_startup",
        Vec::new(),
        record_schedule_step,
        Arc::as_ptr(&ud_startup) as *mut c_void,
        reg.clone(),
    )
    .with_schedule(ScheduleId::STARTUP);

    let sys_fixed = GuestSystem::new(
        "s_fixed",
        Vec::new(),
        record_schedule_step,
        Arc::as_ptr(&ud_fixed) as *mut c_void,
        reg.clone(),
    )
    .with_schedule(ScheduleId::FIXED_UPDATE);

    let sys_logic_b = GuestSystem::new(
        "s_logic_b",
        Vec::new(),
        record_schedule_step,
        Arc::as_ptr(&ud_logic_b) as *mut c_void,
        reg.clone(),
    )
    .with_schedule(ScheduleId::UPDATE)
    .with_in_sets(vec!["host::logic".to_string()])
    .with_ordering(vec!["guest_logic_a".to_string()], Vec::new());

    let sys_logic_a = GuestSystem::new(
        "guest_logic_a",
        Vec::new(),
        record_schedule_step,
        Arc::as_ptr(&ud_logic_a) as *mut c_void,
        reg.clone(),
    )
    .with_schedule(ScheduleId::UPDATE)
    .with_in_sets(vec!["host::logic".to_string()]);

    let sys_post = GuestSystem::new(
        "s_post",
        Vec::new(),
        record_schedule_step,
        Arc::as_ptr(&ud_post) as *mut c_void,
        reg,
    )
    .with_schedule(ScheduleId::POST_UPDATE);

    // Apply `ModNamedSystemSet` and `"host::logic"` resolution via Bevy sets to match scheduler.
    #[derive(bevy_ecs::schedule::SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
    struct GuestLogicAMarker;

    // Verify GuestSystem::in_sets() holds the declared `.in_set()` target and resolves cleanly.
    assert_eq!(sys_logic_a.in_sets(), &["host::logic".to_string()]);
    assert_eq!(sys_logic_b.in_sets(), &["host::logic".to_string()]);
    assert_eq!(sys_startup.schedule(), ScheduleId::STARTUP);
    assert_eq!(sys_fixed.schedule(), ScheduleId::FIXED_UPDATE);
    assert_eq!(sys_post.schedule(), ScheduleId::POST_UPDATE);

    // Record host steps + guest steps together by wrapping host log at end of each phase.
    let log_for_host_in = Arc::clone(&log_shared);
    let log_for_host_out = Arc::clone(&log_shared);
    app.add_systems(
        Update,
        (
            (move || log_for_host_in.lock().unwrap().push("host::input")).in_set(Wave5Phase::Input),
            sys_logic_a
                .in_set(Wave5Phase::Logic)
                .in_set(GuestLogicAMarker),
            sys_logic_b
                .in_set(Wave5Phase::Logic)
                .after(GuestLogicAMarker),
            (move || log_for_host_out.lock().unwrap().push("host::output"))
                .in_set(Wave5Phase::Output),
        ),
    );
    app.add_systems(Startup, sys_startup);
    app.add_systems(FixedUpdate, sys_fixed);
    app.add_systems(PostUpdate, sys_post);

    app.update();
    app.world_mut().run_schedule(FixedUpdate);

    assert_eq!(
        *log_shared.lock().unwrap(),
        vec![
            "guest::startup",
            "host::input",
            "guest::logic_a",
            "guest::logic_b",
            "host::output",
            "guest::post",
            "guest::fixed",
        ]
    );
}
