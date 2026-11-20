//! Integration tests for `ModTelemetryPlugin`: all four `bam/*` BRP JSON-RPC methods
//! (`bam/list_mods`, `bam/system_metrics`, `bam/component_schemas`, `bam/hot_reload`), fixed
//! 64-byte little-endian [`SystemTelemetryDigest64`] wire frames, real Bevy `0.20` `summary_tick`
//! skip reporting, and non-blocking Unix domain socket / loopback TCP telemetry streaming.

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{LoadContext, host_register_component, write_entity_pod_bytes};
use bevy_abi_mod_reflect::reflect::ModReflectPlugin;
use bevy_abi_mod_reflect::telemetry::{
    BRP_METHOD_BAM_COMPONENT_SCHEMAS, BRP_METHOD_BAM_HOT_RELOAD, BRP_METHOD_BAM_LIST_MODS,
    BRP_METHOD_BAM_SYSTEM_METRICS, HotReloadOutcome, ModHotReloadQueue, ModTelemetryEndpoint,
    ModTelemetryPlugin, ModTelemetryRecorder, ModTelemetryWorldExt, SYSTEM_TELEMETRY_DIGEST_SIZE,
    SYSTEM_TELEMETRY_DIGEST_VERSION, SYSTEM_TELEMETRY_MAGIC, SummaryTickTableStats,
    SystemExecutionSample, SystemMetricSnapshot, SystemTelemetryDigest64,
    TELEMETRY_FLAG_QUARANTINED, TELEMETRY_FLAG_SUMMARY_TICK_ACTIVE, fnv1a_64_hash_system,
    probe_summary_tick_tables,
};
use bevy_abi_mod_sys::{
    BAM_COMPONENT_FLAG_SUMMARY_TICK, BamComponent, BamFieldDesc, BamFieldKind, ComponentDesc,
    ComponentUuid, FfiResult, StrView,
};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::name::Name;
use bevy_ecs::world::World;
use bevy_remote::{
    BrpRequest, BrpResult, RemoteMethodSystemId, RemoteMethods, RemotePlugin, error_codes,
};
use core::ffi::CStr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use serde_json::Value;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
static SOCK_SEQ: AtomicUsize = AtomicUsize::new(0);

#[cfg(unix)]
fn temp_socket_path(tag: &str) -> PathBuf {
    let seq = SOCK_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bam_tel_{tag}_{}_{}.sock", std::process::id(), seq))
}

fn call_remote_method(world: &mut World, method: &str, params: Option<Value>) -> BrpResult {
    let sys_id = match *world
        .resource::<RemoteMethods>()
        .get(method)
        .unwrap_or_else(|| panic!("missing RemoteMethods entry '{method}'"))
    {
        RemoteMethodSystemId::Instant(id) => id,
        RemoteMethodSystemId::Watching(_) => panic!("expected Instant method '{method}'"),
    };
    world
        .run_system_with(sys_id, params)
        .expect("run_system_with BRP method")
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct RigidBodyC99 {
    mass: f32,
    velocity: [f32; 3],
    target_entity: u64,
    sleeping: u8,
    _pad: [u8; 7],
}

fn rigid_body_fields() -> Vec<BamFieldDesc> {
    vec![
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("mass"),
            StrView::from_str("f32"),
            core::mem::offset_of!(RigidBodyC99, mass),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("velocity"),
            StrView::from_str("Vec3"),
            core::mem::offset_of!(RigidBodyC99, velocity),
            12,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U64,
            StrView::from_str("target_entity"),
            StrView::from_str("Entity"),
            core::mem::offset_of!(RigidBodyC99, target_entity),
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::BOOL,
            StrView::from_str("sleeping"),
            StrView::from_str("bool"),
            core::mem::offset_of!(RigidBodyC99, sleeping),
            1,
            1,
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn register_dynamic_c99_component(
    world: &mut World,
    mod_id: ModId,
    name: &'static str,
    c_name: &'static CStr,
    size: usize,
    align: usize,
    flags: u32,
    schema_fingerprint: u64,
    fields: &[BamFieldDesc],
) -> BamComponent {
    let registry = world.resource::<ModRegistry>().clone();
    let is_active = Arc::new(AtomicBool::new(true));
    let mut load_ctx =
        LoadContext::with_world_and_mod_id(registry, is_active, world as *mut World, Some(mod_id));

    let desc = ComponentDesc::new(
        c_name.as_ptr(),
        ComponentUuid::from_parts(name, size, align, schema_fingerprint),
        size,
        align,
    )
    .with_flags(flags)
    .with_schema_fingerprint(schema_fingerprint)
    .with_fields(fields.as_ptr(), fields.len());

    let mut handle = BamComponent::INVALID;
    // SAFETY: `load_ctx`, `desc`, `fields`, and `handle` are valid for this synchronous call.
    let status = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut handle) };
    assert_eq!(status, FfiResult::OK, "failed registering {name}");
    load_ctx.finish_init();
    handle
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Kinetic {
    vx: f32,
    vy: f32,
}

fn kinetic_bytes(v: Kinetic) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&v.vx.to_ne_bytes());
    out[4..8].copy_from_slice(&v.vy.to_ne_bytes());
    out
}

fn kinetic_fields() -> Vec<BamFieldDesc> {
    vec![
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("vx"),
            StrView::from_str("f32"),
            core::mem::offset_of!(Kinetic, vx),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("vy"),
            StrView::from_str("f32"),
            core::mem::offset_of!(Kinetic, vy),
            4,
            4,
        ),
    ]
}

#[derive(Component)]
struct TableTag0;
#[derive(Component)]
struct TableTag1;
#[derive(Component)]
struct TableTag2;
#[derive(Component)]
struct TableTag3;
#[derive(Component)]
struct TableTag4;

#[test]
fn system_telemetry_digest64_is_strictly_64_bytes_little_endian() {
    assert_eq!(core::mem::size_of::<SystemTelemetryDigest64>(), 64);
    assert_eq!(SYSTEM_TELEMETRY_DIGEST_SIZE, 64);
    assert_eq!(core::mem::align_of::<SystemTelemetryDigest64>(), 8);

    let digest = SystemTelemetryDigest64 {
        magic: SYSTEM_TELEMETRY_MAGIC,
        version: SYSTEM_TELEMETRY_DIGEST_VERSION,
        flags: TELEMETRY_FLAG_SUMMARY_TICK_ACTIVE | TELEMETRY_FLAG_QUARANTINED,
        mod_id: 42,
        system_hash: fnv1a_64_hash_system("mod_physics", "integrate"),
        frame_seq: 9001,
        matched_entities: 1234,
        matched_tables: 8,
        skipped_summary_tick_tables: 6,
        chunks: 4,
        p50_us: 140,
        p99_us: 512,
        commands_queued: 19,
        reserved: 0,
    };

    let bytes = digest.to_le_bytes();
    assert_eq!(bytes.len(), 64);
    assert_eq!(&bytes[0..4], b"BAMT");
    assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 1);
    assert_eq!(
        u16::from_le_bytes([bytes[6], bytes[7]]),
        TELEMETRY_FLAG_SUMMARY_TICK_ACTIVE | TELEMETRY_FLAG_QUARANTINED
    );
    assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 42);
    assert_eq!(
        u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
        6,
        "skipped_summary_tick_tables must sit at byte offset 40"
    );
    assert_eq!(u32::from_le_bytes(bytes[48..52].try_into().unwrap()), 140);
    assert_eq!(u32::from_le_bytes(bytes[52..56].try_into().unwrap()), 512);
    assert_eq!(
        SystemTelemetryDigest64::from_le_bytes(&bytes).unwrap(),
        digest
    );
}

#[test]
fn brp_all_four_bam_methods_end_to_end_via_remote_methods() {
    const RIGID_FP: u64 = 0xCAFE_BABE_1234_5678;

    let mut app = App::new();
    app.add_plugins((
        AbiModPlugin,
        RemotePlugin::default(),
        ModReflectPlugin,
        ModTelemetryPlugin::new(),
    ));

    let mod_phys = ModId(11);
    let mod_ai = ModId(22);
    app.world_mut().spawn((
        Name::new("mod:mod_physics"),
        LoadedModInfo {
            id: mod_phys,
            name: "mod_physics".to_owned(),
            version: "1.4.2".to_owned(),
            path: PathBuf::from("/mods/libmod_physics.dylib"),
            system_count: 2,
            fault_count: 0,
        },
    ));
    app.world_mut().spawn((
        Name::new("mod:mod_ai"),
        LoadedModInfo {
            id: mod_ai,
            name: "mod_ai".to_owned(),
            version: "0.9.0".to_owned(),
            path: PathBuf::from("/mods/libmod_ai.dylib"),
            system_count: 1,
            fault_count: 3,
        },
    ));

    // Register a quarantined AI system in ModQuarantine.
    app.world_mut()
        .resource_mut::<ModQuarantine>()
        .register_system(
            mod_ai,
            Arc::from("bad_ai_tick"),
            Arc::new(AtomicBool::new(false)),
        );

    // Register C99 dynamic component with exact BamFieldDesc offsets & schema_fingerprint.
    let rb_fields = rigid_body_fields();
    register_dynamic_c99_component(
        app.world_mut(),
        mod_phys,
        "mod_physics::RigidBody",
        c"mod_physics::RigidBody",
        core::mem::size_of::<RigidBodyC99>(),
        core::mem::align_of::<RigidBodyC99>(),
        BAM_COMPONENT_FLAG_SUMMARY_TICK,
        RIGID_FP,
        &rb_fields,
    );

    // Seed per-system metrics (both raw sliding-window samples and pre-aggregated budget snapshot).
    for i in 1u64..=100u64 {
        app.world_mut()
            .record_system_telemetry(SystemExecutionSample {
                mod_id: Some(mod_phys),
                mod_name: "mod_physics".to_owned(),
                system_name: "integrate_bodies".to_owned(),
                duration_us: i * 5,
                matched_entities: 2_048,
                matched_tables: 10,
                skipped_summary_tick_tables: 7,
                chunks: 3,
                commands_queued: 12,
                faulted: false,
                budget_breached: false,
            });
    }
    app.world_mut()
        .resource_mut::<ModTelemetryRecorder>()
        .record_snapshot(SystemMetricSnapshot {
            mod_id: Some(mod_ai),
            mod_name: "mod_ai".to_owned(),
            system_name: "bad_ai_tick".to_owned(),
            p50_us: 800,
            p99_us: 12_500,
            matched_entities: 64,
            matched_tables: 2,
            skipped_summary_tick_tables: 0,
            chunks: 1,
            commands_queued: 0,
            quarantined: true,
            faulted: true,
            budget_breached: true,
        });

    app.finish();
    app.cleanup();

    // 1. `bam/list_mods` via live RemoteMethods.
    let list_res = call_remote_method(app.world_mut(), BRP_METHOD_BAM_LIST_MODS, None).unwrap();
    assert_eq!(list_res["total_mods"], 2);
    assert_eq!(list_res["total_quarantined_systems"], 1);
    let mods = list_res["mods"].as_array().unwrap();
    let phys_entry = mods.iter().find(|m| m["name"] == "mod_physics").unwrap();
    assert_eq!(phys_entry["mod_id"], 11);
    assert_eq!(phys_entry["version"], "1.4.2");
    assert_eq!(phys_entry["system_count"], 2);
    assert_eq!(phys_entry["quarantined"], false);
    let ai_entry = mods.iter().find(|m| m["name"] == "mod_ai").unwrap();
    assert_eq!(ai_entry["mod_id"], 22);
    assert_eq!(ai_entry["fault_count"], 3);
    assert_eq!(ai_entry["quarantined"], true);
    assert_eq!(
        ai_entry["quarantined_systems"],
        serde_json::json!(["bad_ai_tick"])
    );

    // 2. `bam/system_metrics` (unfiltered & filtered).
    let metrics_res =
        call_remote_method(app.world_mut(), BRP_METHOD_BAM_SYSTEM_METRICS, None).unwrap();
    let systems = metrics_res["systems"].as_array().unwrap();
    assert_eq!(systems.len(), 2);
    let phys_sys = systems
        .iter()
        .find(|s| s["system_name"] == "integrate_bodies")
        .unwrap();
    assert_eq!(phys_sys["matched_entities"], 2_048);
    assert_eq!(phys_sys["matched_tables"], 10);
    assert_eq!(phys_sys["skipped_summary_tick_tables"], 7);
    assert_eq!(phys_sys["p50_us"], 250);
    assert_eq!(phys_sys["p99_us"], 495);
    assert_eq!(phys_sys["chunks"], 3);
    assert_eq!(phys_sys["commands_queued"], 12);
    assert_eq!(phys_sys["quarantined"], false);

    let ai_filtered = call_remote_method(
        app.world_mut(),
        BRP_METHOD_BAM_SYSTEM_METRICS,
        Some(serde_json::json!({ "mod_name": "mod_ai" })),
    )
    .unwrap();
    let ai_systems = ai_filtered["systems"].as_array().unwrap();
    assert_eq!(ai_systems.len(), 1);
    assert_eq!(ai_systems[0]["p50_us"], 800);
    assert_eq!(ai_systems[0]["p99_us"], 12_500);
    assert_eq!(ai_systems[0]["quarantined"], true);
    assert_eq!(ai_systems[0]["budget_breached"], true);

    // 3. `bam/component_schemas` with exact BamFieldDesc byte offsets & schema_fingerprint.
    let schemas_res = call_remote_method(
        app.world_mut(),
        BRP_METHOD_BAM_COMPONENT_SCHEMAS,
        Some(serde_json::json!({ "component": "mod_physics::RigidBody" })),
    )
    .unwrap();
    let comps = schemas_res["components"].as_array().unwrap();
    assert_eq!(comps.len(), 1);
    let rb = &comps[0];
    assert_eq!(rb["name"], "mod_physics::RigidBody");
    assert_eq!(rb["size"], core::mem::size_of::<RigidBodyC99>());
    assert_eq!(rb["align"], core::mem::align_of::<RigidBodyC99>());
    assert_eq!(rb["schema_fingerprint"], RIGID_FP);
    assert_eq!(rb["is_summary_tick"], true);
    assert_eq!(rb["owner_mod_id"], 11);
    assert!(ComponentUuid::try_parse_hex(rb["uuid"].as_str().unwrap()).is_some());

    let fields = rb["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 4);
    assert_eq!(fields[0]["name"], "mass");
    assert_eq!(fields[0]["type_tag"], "f32");
    assert_eq!(
        fields[0]["offset"],
        core::mem::offset_of!(RigidBodyC99, mass)
    );
    assert_eq!(fields[1]["name"], "velocity");
    assert_eq!(fields[1]["type_tag"], "Vec3");
    assert_eq!(
        fields[1]["offset"],
        core::mem::offset_of!(RigidBodyC99, velocity)
    );
    assert_eq!(fields[2]["name"], "target_entity");
    assert_eq!(fields[2]["type_tag"], "Entity");
    assert_eq!(
        fields[2]["offset"],
        core::mem::offset_of!(RigidBodyC99, target_entity)
    );
    assert_eq!(fields[3]["name"], "sleeping");
    assert_eq!(fields[3]["type_tag"], "bool");
    assert_eq!(
        fields[3]["offset"],
        core::mem::offset_of!(RigidBodyC99, sleeping)
    );

    // 4. `bam/hot_reload` by mod_name AND by mod_id + error handling for unknown mods.
    let reloaded_counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&reloaded_counter);
    app.world_mut()
        .resource_mut::<ModHotReloadQueue>()
        .set_custom_handler(move |_world, req| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            Ok(ModId(req.mod_id + 1000))
        });

    let hr_by_name = call_remote_method(
        app.world_mut(),
        BRP_METHOD_BAM_HOT_RELOAD,
        Some(serde_json::json!({ "mod_name": "mod_physics" })),
    )
    .unwrap();
    assert_eq!(hr_by_name["status"], "scheduled");
    assert_eq!(hr_by_name["mod_id"], 11);
    assert_eq!(hr_by_name["mod_name"], "mod_physics");

    let hr_by_id = call_remote_method(
        app.world_mut(),
        BRP_METHOD_BAM_HOT_RELOAD,
        Some(serde_json::json!({ "mod_id": 22 })),
    )
    .unwrap();
    assert_eq!(hr_by_id["status"], "scheduled");
    assert_eq!(hr_by_id["mod_id"], 22);
    assert_eq!(hr_by_id["mod_name"], "mod_ai");

    assert_eq!(app.world().resource::<ModHotReloadQueue>().pending_len(), 2);

    // Running one frame executes both queued hot-reloads cleanly in `Last`.
    app.update();
    assert_eq!(reloaded_counter.load(Ordering::SeqCst), 2);
    assert_eq!(app.world().resource::<ModHotReloadQueue>().pending_len(), 0);
    let history = app.world().resource::<ModHotReloadQueue>().history();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[0],
        HotReloadOutcome::Reloaded {
            old_mod_id: 11,
            new_mod_id: 1011,
            mod_name: "mod_physics".to_owned(),
        }
    );
    assert_eq!(
        history[1],
        HotReloadOutcome::Reloaded {
            old_mod_id: 22,
            new_mod_id: 1022,
            mod_name: "mod_ai".to_owned(),
        }
    );

    // Unknown mod rejected cleanly with RESOURCE_NOT_PRESENT; missing params -> INVALID_PARAMS.
    let missing_err = app
        .world_mut()
        .dispatch_telemetry_brp_request(BrpRequest {
            method: BRP_METHOD_BAM_HOT_RELOAD.to_owned(),
            id: Some(serde_json::json!(99)),
            params: Some(serde_json::json!({ "mod_name": "nonexistent_mod" })),
        })
        .unwrap_err();
    assert_eq!(missing_err.code, error_codes::RESOURCE_NOT_PRESENT);

    let bad_params = call_remote_method(
        app.world_mut(),
        BRP_METHOD_BAM_HOT_RELOAD,
        Some(serde_json::json!({})),
    )
    .unwrap_err();
    assert_eq!(bad_params.code, error_codes::INVALID_PARAMS);
}

fn run_summary_tick_probe(app: &mut App, mod_id: ModId) -> SummaryTickTableStats {
    let k_fields = kinetic_fields();
    let kinetic_h = register_dynamic_c99_component(
        app.world_mut(),
        mod_id,
        "sim_mod::Kinetic",
        c"sim_mod::Kinetic",
        core::mem::size_of::<Kinetic>(),
        core::mem::align_of::<Kinetic>(),
        BAM_COMPONENT_FLAG_SUMMARY_TICK,
        0x5151_0001,
        &k_fields,
    );

    // Spawn 5 distinct ECS tables (10 entities each = 50 entities total) carrying the dynamic
    // `BAM_COMPONENT_FLAG_SUMMARY_TICK` component.
    let mut table2_entities = Vec::new();
    for i in 0..10u32 {
        let bytes = kinetic_bytes(Kinetic {
            vx: i as f32,
            vy: 1.0,
        });
        let e0 = app.world_mut().spawn(TableTag0).id();
        let e1 = app.world_mut().spawn(TableTag1).id();
        let e2 = app.world_mut().spawn(TableTag2).id();
        let e3 = app.world_mut().spawn(TableTag3).id();
        let e4 = app.world_mut().spawn(TableTag4).id();
        for e in [e0, e1, e2, e3, e4] {
            write_entity_pod_bytes(app.world_mut(), e, kinetic_h, &bytes).unwrap();
        }
        table2_entities.push(e2);
    }

    // Advance change tick so initial spawns are older than `last_run`, then mutate ONLY Table 2.
    app.world_mut().increment_change_tick();
    let last_run = app.world().read_change_tick();
    app.world_mut().increment_change_tick();
    let updated_bytes = kinetic_bytes(Kinetic { vx: 99.0, vy: 2.0 });
    for &e in &table2_entities {
        write_entity_pod_bytes(app.world_mut(), e, kinetic_h, &updated_bytes).unwrap();
    }
    let this_run = app.world().read_change_tick();

    // Probe real Bevy 0.20 `summary_tick` table skips: 4 of 5 tables MUST be skipped!
    let stats = probe_summary_tick_tables(app.world(), kinetic_h, last_run, this_run);
    assert_eq!(stats.matched_tables, 5);
    assert_eq!(stats.skipped_summary_tick_tables, 4);
    assert_eq!(stats.visited_tables, 1);
    assert_eq!(stats.matched_entities, 10);
    stats
}

#[test]
fn summary_tick_probe_reports_real_table_skips() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModTelemetryPlugin::new()));
    let stats = run_summary_tick_probe(&mut app, ModId(77));
    assert_eq!(stats.visited_tables, 1);
}

#[cfg(unix)]
#[test]
fn live_unix_socket_streams_64b_digests_with_real_summary_tick_skips_and_nonblocking_backpressure()
{
    let sock_path = temp_socket_path("live_stream");

    // 1. Verify stale socket cleanup on bind: create a UnixListener then drop it leaving the
    //    stale socket file behind on disk.
    {
        let _stale = UnixListener::bind(&sock_path).expect("bind stale socket");
    }
    assert!(
        sock_path.exists(),
        "stale socket file should exist before bind_unix cleanup"
    );

    // Also verify bind_unix refuses to clobber a regular non-socket file.
    let reg_file = temp_socket_path("regular_file");
    std::fs::write(&reg_file, b"keep-me").unwrap();
    let clobber_err = ModTelemetryEndpoint::bind_unix(&reg_file).unwrap_err();
    assert_eq!(clobber_err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&reg_file).unwrap(), b"keep-me");
    let _ = std::fs::remove_file(&reg_file);

    // 2. Build App with ModTelemetryPlugin bound to the cleaned-up Unix socket.
    let mut app = App::new();
    app.add_plugins((
        AbiModPlugin,
        ModTelemetryPlugin::new()
            .with_unix_socket(&sock_path)
            .with_percentile_window(100),
    ));

    let stats = run_summary_tick_probe(&mut app, ModId(77));

    // Seed 100 duration samples (10..=1000 us -> exact p50=500, p99=990) paired with live stats.
    for d in 1u64..=100u64 {
        app.world_mut()
            .record_system_telemetry(SystemExecutionSample {
                mod_id: Some(ModId(77)),
                mod_name: "sim_mod".to_owned(),
                system_name: "kinetic_step".to_owned(),
                duration_us: d * 10,
                matched_entities: stats.matched_entities,
                matched_tables: stats.matched_tables,
                skipped_summary_tick_tables: stats.skipped_summary_tick_tables,
                chunks: stats.visited_tables,
                commands_queued: 5,
                faulted: false,
                budget_breached: false,
            });
    }

    // Connect live UnixStream client and trigger one Bevy frame (`Last` flushes 64-byte digest).
    let mut client = UnixStream::connect(&sock_path).expect("connect to telemetry Unix socket");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    app.update();

    let mut frame_buf = [0u8; SYSTEM_TELEMETRY_DIGEST_SIZE];
    client
        .read_exact(&mut frame_buf)
        .expect("read 64-byte telemetry digest frame");
    let digest = SystemTelemetryDigest64::from_le_bytes(&frame_buf).unwrap();
    assert_eq!(digest.magic, SYSTEM_TELEMETRY_MAGIC);
    assert_eq!(digest.version, SYSTEM_TELEMETRY_DIGEST_VERSION);
    assert_eq!(digest.mod_id, 77);
    assert_eq!(
        digest.system_hash,
        fnv1a_64_hash_system("sim_mod", "kinetic_step")
    );
    assert_eq!(digest.matched_tables, 5);
    assert_eq!(digest.skipped_summary_tick_tables, 4);
    assert_eq!(digest.matched_entities, 10);
    assert_eq!(digest.chunks, 1);
    assert_eq!(digest.p50_us, 500);
    assert_eq!(digest.p99_us, 990);
    assert_eq!(digest.commands_queued, 5);
    assert_ne!(digest.flags & TELEMETRY_FLAG_SUMMARY_TICK_ACTIVE, 0);
    assert!(digest.frame_seq >= 1);

    // 3. Test non-blocking slow client backpressure & disconnected client reaping:
    //    Disconnect `client`, connect a stalled client that never reads, shrink queue cap, and
    //    run 200 updates in a tight loop proving frame execution NEVER blocks.
    drop(client);
    let _stalled_client = UnixStream::connect(&sock_path).expect("connect stalled client");
    app.world()
        .resource::<ModTelemetryEndpoint>()
        .set_max_pending_frames(4);

    let start = Instant::now();
    for _ in 0..200 {
        app.update();
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "200 updates with a slow/unreading client must never stall"
    );
    let ep = app.world().resource::<ModTelemetryEndpoint>();
    assert_eq!(
        ep.connected_clients(),
        1,
        "disconnected client must be reaped leaving only the stalled client"
    );
    assert!(ep.frames_sent() >= 1);

    // 4. Dropping the App drops ModTelemetryEndpoint and automatically unlinks the socket file.
    drop(_stalled_client);
    drop(app);
    assert!(
        !sock_path.exists(),
        "telemetry Unix socket file must be automatically unlinked on Drop"
    );
}

#[test]
fn loopback_tcp_transport_streams_64b_digests_and_rejects_non_loopback() {
    // Non-loopback bind MUST be rejected for local-only security.
    let non_loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let err = ModTelemetryEndpoint::bind_loopback_tcp(non_loopback).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModTelemetryPlugin::new()));
    let bound_addr = app
        .world_mut()
        .bind_telemetry_loopback_tcp(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .expect("bind loopback TCP telemetry endpoint");
    assert!(bound_addr.ip().is_loopback());
    assert_ne!(bound_addr.port(), 0);

    app.world_mut()
        .record_system_telemetry(SystemExecutionSample {
            mod_id: Some(ModId(9)),
            mod_name: "win_mod".to_owned(),
            system_name: "net_tick".to_owned(),
            duration_us: 333,
            matched_entities: 40,
            matched_tables: 3,
            skipped_summary_tick_tables: 2,
            chunks: 1,
            commands_queued: 4,
            faulted: false,
            budget_breached: false,
        });

    let mut client = TcpStream::connect(bound_addr).expect("connect to loopback TCP endpoint");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    app.update();

    let mut buf = [0u8; SYSTEM_TELEMETRY_DIGEST_SIZE];
    client
        .read_exact(&mut buf)
        .expect("read 64-byte digest over loopback TCP");
    let digest = SystemTelemetryDigest64::from_le_bytes(&buf).unwrap();
    assert_eq!(digest.mod_id, 9);
    assert_eq!(
        digest.system_hash,
        fnv1a_64_hash_system("win_mod", "net_tick")
    );
    assert_eq!(digest.matched_tables, 3);
    assert_eq!(digest.skipped_summary_tick_tables, 2);
    assert_eq!(digest.p50_us, 333);
    assert_eq!(digest.p99_us, 333);
}

#[test]
fn mod_budget_telemetry_syncs_automatically_into_telemetry_recorder_and_brp() {
    use bevy_abi_mod_host_sys::GuestSystem;
    use bevy_abi_mod_sys::BamSystemCtx;
    use bevy_app::Update;
    use core::ffi::c_void;

    unsafe extern "C" fn live_budget_noop(
        _ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        if !user_data.is_null() {
            // SAFETY: `user_data` points to `runs: AtomicUsize` valid for the duration of `app.update()`.
            let counter = unsafe { &*user_data.cast::<AtomicUsize>() };
            counter.fetch_add(1, Ordering::SeqCst);
        }
        FfiResult::OK
    }

    let mut app = App::new();
    app.add_plugins((
        AbiModPlugin,
        RemotePlugin::default(),
        ModTelemetryPlugin::new(),
    ));

    let runs = Box::new(AtomicUsize::new(0));
    let reg = app.world().resource::<ModRegistry>().clone();
    let mut sys = GuestSystem::new(
        "live_budget_tick",
        Vec::new(),
        live_budget_noop,
        (&raw const *runs).cast_mut().cast::<c_void>(),
        reg,
    );
    sys.set_mod_id(ModId(77));
    sys.set_mod_namespace(Some(Arc::from("budget_sync_mod")));
    app.add_systems(Update, sys);

    // Run one full frame so `Update` executes `GuestSystem` (recording into `ModBudgetTelemetry`)
    // and `Last` flushes synchronized budget metrics into `ModTelemetryRecorder`.
    app.finish();
    app.cleanup();
    app.update();
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    let brp = call_remote_method(
        app.world_mut(),
        BRP_METHOD_BAM_SYSTEM_METRICS,
        Some(serde_json::json!({ "mod_id": 77 })),
    )
    .expect("bam/system_metrics succeeds");
    let systems = brp["systems"].as_array().unwrap();
    assert_eq!(systems.len(), 1);
    assert_eq!(systems[0]["mod_name"], "budget_sync_mod");
    assert_eq!(systems[0]["system_name"], "live_budget_tick");
    assert!(systems[0]["sample_count"].as_u64().unwrap() >= 1);
    assert_eq!(systems[0]["quarantined"], false);

    let rec = app.world().resource::<ModTelemetryRecorder>();
    assert!(rec.system_metrics().iter().any(|m| {
        m.mod_id == 77 && m.mod_name == "budget_sync_mod" && m.system_name == "live_budget_tick"
    }));

    drop(app);
    drop(runs);
}
