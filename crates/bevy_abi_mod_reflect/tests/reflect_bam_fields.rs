//! Integration tests for declarative ABI `0.3` [`BamFieldDesc`] reflection (`ReflectFromPtr`,
//! dynamic `StructInfo`, `ReflectComponent`/`ReflectResource`, `DynamicStruct`, `bevy_abi_mod/*`
//! BRP JSON-RPC, standard Bevy `0.20` `bevy_remote` BRP JSON-RPC, and `bevy_settings` TOML sync)
//! on foreign/C99-style dynamic components and resources without Rust proc-macro witness VTables.

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{
    HostCommands, HostCommandsStorage, LoadContext, host_cmd_insert, host_cmd_spawn_empty,
    host_register_component, read_entity_pod_bytes, read_resource_bam_field_bytes,
    write_entity_pod_bytes,
};
use bevy_abi_mod_reflect::reflect::{
    BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
    BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
    ModReflectAppExt, ModReflectPlugin, ModReflectRegistry, ModReflectWorldExt,
};
use bevy_abi_mod_sys::{
    BAM_COMPONENT_FLAG_IMMUTABLE, BamComponent, BamFieldDesc, BamFieldKind, ComponentDesc,
    ComponentUuid, FfiEntity, FfiResult, ModComponent, StrView,
};
use bevy_app::App;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::Resource;
use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent, ReflectResource};
use bevy_ecs::world::World;
use bevy_reflect::structs::DynamicStruct;
use bevy_reflect::{GetPath, ReflectFromPtr, TypeInfo};
use bevy_remote::{BrpRequest, BrpResult, RemoteMethodSystemId, RemoteMethods, RemotePlugin};
use core::ffi::CStr;
use core::sync::atomic::AtomicBool;
use serde_json::Value;
use std::sync::Arc;

/// Registers a foreign/C99-style dynamic component through the genuine C-ABI
/// `host_register_component` entry point with raw `BamFieldDesc` metadata (no Rust
/// `#[derive(ModComponent)]` witness VTable).
fn register_c99_dynamic(
    world: &mut World,
    name: &'static str,
    c_name: &'static CStr,
    size: usize,
    align: usize,
    flags: u32,
    fields: &[BamFieldDesc],
) -> BamComponent {
    let registry = world.resource::<ModRegistry>().clone();
    let is_active = Arc::new(AtomicBool::new(true));
    let mut load_ctx =
        LoadContext::with_world_and_mod_id(registry, is_active, world as *mut World, None);

    let mut desc = ComponentDesc::new(
        c_name.as_ptr(),
        ComponentUuid::from_parts(name, size, align, u64::from(flags)),
        size,
        align,
    )
    .with_fields(fields.as_ptr(), fields.len());
    desc.flags = flags;

    let mut handle = BamComponent::INVALID;
    // SAFETY: `load_ctx`, `desc`, `fields`, and `handle` are valid for the duration of the call.
    let res = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut handle) };
    assert_eq!(res, FfiResult::OK, "host_register_component({name}) failed");
    load_ctx.finish_init();
    handle
}

/// Invokes a built-in Bevy `bevy_remote` (`RemotePlugin`) BRP method registered in `RemoteMethods`.
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
struct C99Telemetry {
    active: u8, // raw C99 bool byte (we seed `2` to prove non-canonical truthy normalization)
    tag_u8: u8,
    delta_i8: i8,
    // 1 byte padding
    flags_u16: u16,
    temp_i16: i16,
    ammo_u32: u32,
    score_i32: i32,
    hp_f32: f32,
    // 4 bytes padding before 8-byte aligned fields
    tick_u64: u64,
    offset_i64: i64,
    sim_time_f64: f64,
    index_usize: usize,
    stride_isize: isize,
    target_entity: u64,
    linked_comp: u32,
    // 4 bytes padding before vec3/vec4? No, f32 is align 4!
    pos_vec3: [f32; 3],
    rot_vec4: [f32; 4],
    uv_vec2: [f32; 2],
    grid_ivec2: [i32; 2],
    cell_uvec2: [u32; 2],
    hi_dvec2: [f64; 2],
    rgba_bytes4: [u8; 4],
    uuid_bytes16: [u8; 16],
    opaque_blob: [u8; 4],
}

fn c99_telemetry_fields() -> Vec<BamFieldDesc> {
    vec![
        BamFieldDesc::new(
            BamFieldKind::BOOL,
            StrView::from_str("active"),
            StrView::from_str("bool"),
            core::mem::offset_of!(C99Telemetry, active),
            1,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::U8,
            StrView::from_str("tag_u8"),
            StrView::from_str("u8"),
            core::mem::offset_of!(C99Telemetry, tag_u8),
            1,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::I8,
            StrView::from_str("delta_i8"),
            StrView::from_str("i8"),
            core::mem::offset_of!(C99Telemetry, delta_i8),
            1,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::U16,
            StrView::from_str("flags_u16"),
            StrView::from_str("u16"),
            core::mem::offset_of!(C99Telemetry, flags_u16),
            2,
            2,
        ),
        BamFieldDesc::new(
            BamFieldKind::I16,
            StrView::from_str("temp_i16"),
            StrView::from_str("i16"),
            core::mem::offset_of!(C99Telemetry, temp_i16),
            2,
            2,
        ),
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("ammo_u32"),
            StrView::from_str("u32"),
            core::mem::offset_of!(C99Telemetry, ammo_u32),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::I32,
            StrView::from_str("score_i32"),
            StrView::from_str("i32"),
            core::mem::offset_of!(C99Telemetry, score_i32),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("hp_f32"),
            StrView::from_str("f32"),
            core::mem::offset_of!(C99Telemetry, hp_f32),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U64,
            StrView::from_str("tick_u64"),
            StrView::from_str("u64"),
            core::mem::offset_of!(C99Telemetry, tick_u64),
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::I64,
            StrView::from_str("offset_i64"),
            StrView::from_str("i64"),
            core::mem::offset_of!(C99Telemetry, offset_i64),
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::F64,
            StrView::from_str("sim_time_f64"),
            StrView::from_str("f64"),
            core::mem::offset_of!(C99Telemetry, sim_time_f64),
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::USIZE,
            StrView::from_str("index_usize"),
            StrView::from_str("usize"),
            core::mem::offset_of!(C99Telemetry, index_usize),
            core::mem::size_of::<usize>(),
            core::mem::align_of::<usize>(),
        ),
        BamFieldDesc::new(
            BamFieldKind::ISIZE,
            StrView::from_str("stride_isize"),
            StrView::from_str("isize"),
            core::mem::offset_of!(C99Telemetry, stride_isize),
            core::mem::size_of::<isize>(),
            core::mem::align_of::<isize>(),
        ),
        BamFieldDesc::new(
            BamFieldKind::U64,
            StrView::from_str("target_entity"),
            StrView::from_str("Entity"),
            core::mem::offset_of!(C99Telemetry, target_entity),
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("linked_comp"),
            StrView::from_str("BamComponent"),
            core::mem::offset_of!(C99Telemetry, linked_comp),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("pos_vec3"),
            StrView::from_str("Vec3"),
            core::mem::offset_of!(C99Telemetry, pos_vec3),
            12,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("rot_vec4"),
            StrView::from_str("Vec4"),
            core::mem::offset_of!(C99Telemetry, rot_vec4),
            16,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("uv_vec2"),
            StrView::from_str("Vec2"),
            core::mem::offset_of!(C99Telemetry, uv_vec2),
            8,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("grid_ivec2"),
            StrView::from_str("IVec2"),
            core::mem::offset_of!(C99Telemetry, grid_ivec2),
            8,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("cell_uvec2"),
            StrView::from_str("UVec2"),
            core::mem::offset_of!(C99Telemetry, cell_uvec2),
            8,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("hi_dvec2"),
            StrView::from_str("DVec2"),
            core::mem::offset_of!(C99Telemetry, hi_dvec2),
            16,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("rgba_bytes4"),
            StrView::from_str("[u8; 4]"),
            core::mem::offset_of!(C99Telemetry, rgba_bytes4),
            4,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("uuid_bytes16"),
            StrView::from_str("ComponentUuid"),
            core::mem::offset_of!(C99Telemetry, uuid_bytes16),
            16,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::OPAQUE,
            StrView::from_str("opaque_blob"),
            StrView::from_str("Opaque"),
            core::mem::offset_of!(C99Telemetry, opaque_blob),
            4,
            1,
        ),
    ]
}

#[test]
fn c99_dynamic_component_auto_registers_struct_info_and_reflect_from_ptr_roundtrip() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModReflectPlugin));

    let fields = c99_telemetry_fields();
    let handle = register_c99_dynamic(
        app.world_mut(),
        "c99_mod::Telemetry",
        c"c99_mod::Telemetry",
        core::mem::size_of::<C99Telemetry>(),
        core::mem::align_of::<C99Telemetry>(),
        0,
        &fields,
    );

    // `ModReflectPlugin::finish` automatically discovers `BamFieldDesc` components!
    app.finish();
    app.cleanup();

    let world = app.world_mut();
    let schema = world
        .resource::<ModReflectRegistry>()
        .bam_field_schema(handle)
        .expect("ModReflectPlugin::finish must auto-register BamFieldDesc schema")
        .clone();
    assert!(
        world
            .resource::<ModReflectRegistry>()
            .witness_schema(handle)
            .is_none(),
        "C99 dynamic component has no Rust witness VTable"
    );
    assert_eq!(schema.canonical_name, "c99_mod::Telemetry");
    assert_eq!(schema.fields.len(), 24);
    let tid = schema
        .reflected_type_id
        .expect("aligned C99 POD struct must register in-place ReflectFromPtr slot");

    // Verify `AppTypeRegistry` has `TypeInfo::Struct(StructInfo)`, `ReflectFromPtr`, `ReflectComponent`.
    {
        let app_types = world.resource::<AppTypeRegistry>().read();
        let reg = app_types
            .get(tid)
            .expect("dynamic TypeId registered in AppTypeRegistry");
        let by_path = app_types
            .get_with_type_path("c99_mod::Telemetry")
            .expect("lookup by canonical type_path");
        assert_eq!(by_path.type_id(), tid);
        let TypeInfo::Struct(sinfo) = reg.type_info() else {
            panic!("expected StructInfo");
        };
        assert_eq!(sinfo.type_path(), "c99_mod::Telemetry");
        assert_eq!(sinfo.type_path_table().ident(), Some("Telemetry"));
        // 23 POD fields reflected in `StructInfo`; `opaque_blob` skipped safely.
        assert_eq!(sinfo.field_len(), 23);
        assert!(sinfo.field("pos_vec3").is_some());
        assert!(sinfo.field("target_entity").is_some());
        assert!(sinfo.field("opaque_blob").is_none());
        assert!(reg.data::<ReflectFromPtr>().is_some());
        assert!(reg.data::<ReflectComponent>().is_some());
        assert!(reg.data::<ReflectResource>().is_none());
    }

    // Spawn a target entity and populate raw C99 bytes (including sentinel padding `0xCC`
    // and non-canonical C99 bool byte `2`!).
    let target_ent = world.spawn_empty().id();
    let entity = world.spawn_empty().id();
    let mut raw = vec![0xCCu8; core::mem::size_of::<C99Telemetry>()];
    raw[core::mem::offset_of!(C99Telemetry, active)] = 2; // non-canonical true!
    raw[core::mem::offset_of!(C99Telemetry, tag_u8)] = 0x5A;
    raw[core::mem::offset_of!(C99Telemetry, delta_i8)] = (-7i8) as u8;
    raw[core::mem::offset_of!(C99Telemetry, flags_u16)..][..2]
        .copy_from_slice(&0x1234u16.to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, temp_i16)..][..2]
        .copy_from_slice(&(-321i16).to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, ammo_u32)..][..4].copy_from_slice(&42u32.to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, score_i32)..][..4]
        .copy_from_slice(&(-999i32).to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, hp_f32)..][..4]
        .copy_from_slice(&125.5f32.to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, tick_u64)..][..8]
        .copy_from_slice(&9_000_000_001u64.to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, offset_i64)..][..8]
        .copy_from_slice(&(-123_456_789i64).to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, sim_time_f64)..][..8]
        .copy_from_slice(&core::f64::consts::PI.to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, index_usize)..][..core::mem::size_of::<usize>()]
        .copy_from_slice(&777usize.to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, stride_isize)..][..core::mem::size_of::<isize>()]
        .copy_from_slice(&(-42isize).to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, target_entity)..][..8]
        .copy_from_slice(&target_ent.to_bits().to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, linked_comp)..][..4]
        .copy_from_slice(&handle.0.to_ne_bytes());
    let pos_off = core::mem::offset_of!(C99Telemetry, pos_vec3);
    raw[pos_off..pos_off + 4].copy_from_slice(&1.5f32.to_ne_bytes());
    raw[pos_off + 4..pos_off + 8].copy_from_slice(&2.5f32.to_ne_bytes());
    raw[pos_off + 8..pos_off + 12].copy_from_slice(&3.5f32.to_ne_bytes());
    let rot_off = core::mem::offset_of!(C99Telemetry, rot_vec4);
    for (i, f) in [0.0f32, 0.0, 0.0, 1.0].iter().enumerate() {
        raw[rot_off + i * 4..rot_off + (i + 1) * 4].copy_from_slice(&f.to_ne_bytes());
    }
    let uv_off = core::mem::offset_of!(C99Telemetry, uv_vec2);
    raw[uv_off..uv_off + 4].copy_from_slice(&0.25f32.to_ne_bytes());
    raw[uv_off + 4..uv_off + 8].copy_from_slice(&0.75f32.to_ne_bytes());
    let grid_off = core::mem::offset_of!(C99Telemetry, grid_ivec2);
    raw[grid_off..grid_off + 4].copy_from_slice(&(-4i32).to_ne_bytes());
    raw[grid_off + 4..grid_off + 8].copy_from_slice(&9i32.to_ne_bytes());
    let cell_off = core::mem::offset_of!(C99Telemetry, cell_uvec2);
    raw[cell_off..cell_off + 4].copy_from_slice(&16u32.to_ne_bytes());
    raw[cell_off + 4..cell_off + 8].copy_from_slice(&32u32.to_ne_bytes());
    let hi_off = core::mem::offset_of!(C99Telemetry, hi_dvec2);
    raw[hi_off..hi_off + 8].copy_from_slice(&100.25f64.to_ne_bytes());
    raw[hi_off + 8..hi_off + 16].copy_from_slice(&(-200.5f64).to_ne_bytes());
    raw[core::mem::offset_of!(C99Telemetry, rgba_bytes4)..][..4]
        .copy_from_slice(&[255, 128, 64, 255]);
    raw[core::mem::offset_of!(C99Telemetry, uuid_bytes16)..][..16]
        .copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    raw[core::mem::offset_of!(C99Telemetry, opaque_blob)..][..4]
        .copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

    write_entity_pod_bytes(world, entity, handle, &raw).unwrap();

    // 1. Live read via `ReflectFromPtr` (`world.reflect_mod_component`)
    {
        let r = world.reflect_mod_component(entity, handle).unwrap();
        let s = r.reflect_ref().as_struct().unwrap();
        assert!(
            *s.field("active")
                .unwrap()
                .try_downcast_ref::<bool>()
                .unwrap()
        );
        assert_eq!(
            *s.field("tag_u8").unwrap().try_downcast_ref::<u8>().unwrap(),
            0x5A
        );
        assert_eq!(
            *s.field("delta_i8")
                .unwrap()
                .try_downcast_ref::<i8>()
                .unwrap(),
            -7
        );
        assert_eq!(
            *s.field("flags_u16")
                .unwrap()
                .try_downcast_ref::<u16>()
                .unwrap(),
            0x1234
        );
        assert_eq!(
            *s.field("temp_i16")
                .unwrap()
                .try_downcast_ref::<i16>()
                .unwrap(),
            -321
        );
        assert_eq!(
            *s.field("ammo_u32")
                .unwrap()
                .try_downcast_ref::<u32>()
                .unwrap(),
            42
        );
        assert_eq!(
            *s.field("score_i32")
                .unwrap()
                .try_downcast_ref::<i32>()
                .unwrap(),
            -999
        );
        assert_eq!(
            *s.field("hp_f32")
                .unwrap()
                .try_downcast_ref::<f32>()
                .unwrap(),
            125.5
        );
        assert_eq!(
            *s.field("tick_u64")
                .unwrap()
                .try_downcast_ref::<u64>()
                .unwrap(),
            9_000_000_001
        );
        assert_eq!(
            *s.field("offset_i64")
                .unwrap()
                .try_downcast_ref::<i64>()
                .unwrap(),
            -123_456_789
        );
        assert_eq!(
            *s.field("sim_time_f64")
                .unwrap()
                .try_downcast_ref::<f64>()
                .unwrap(),
            core::f64::consts::PI
        );
        assert_eq!(
            *s.field("index_usize")
                .unwrap()
                .try_downcast_ref::<usize>()
                .unwrap(),
            777
        );
        assert_eq!(
            *s.field("stride_isize")
                .unwrap()
                .try_downcast_ref::<isize>()
                .unwrap(),
            -42
        );
        assert_eq!(
            *s.field("target_entity")
                .unwrap()
                .try_downcast_ref::<u64>()
                .unwrap(),
            target_ent.to_bits()
        );
        assert_eq!(
            *s.field("linked_comp")
                .unwrap()
                .try_downcast_ref::<u32>()
                .unwrap(),
            handle.0
        );
        assert_eq!(
            *s.field("pos_vec3")
                .unwrap()
                .try_downcast_ref::<[f32; 3]>()
                .unwrap(),
            [1.5, 2.5, 3.5]
        );
        assert_eq!(
            *s.field("rot_vec4")
                .unwrap()
                .try_downcast_ref::<[f32; 4]>()
                .unwrap(),
            [0.0, 0.0, 0.0, 1.0]
        );
        assert_eq!(
            *s.field("uv_vec2")
                .unwrap()
                .try_downcast_ref::<[f32; 2]>()
                .unwrap(),
            [0.25, 0.75]
        );
        assert_eq!(
            *s.field("grid_ivec2")
                .unwrap()
                .try_downcast_ref::<[i32; 2]>()
                .unwrap(),
            [-4, 9]
        );
        assert_eq!(
            *s.field("cell_uvec2")
                .unwrap()
                .try_downcast_ref::<[u32; 2]>()
                .unwrap(),
            [16, 32]
        );
        assert_eq!(
            *s.field("hi_dvec2")
                .unwrap()
                .try_downcast_ref::<[f64; 2]>()
                .unwrap(),
            [100.25, -200.5]
        );
        assert_eq!(
            *s.field("rgba_bytes4")
                .unwrap()
                .try_downcast_ref::<[u8; 4]>()
                .unwrap(),
            [255, 128, 64, 255]
        );
        assert_eq!(
            *s.field("uuid_bytes16")
                .unwrap()
                .try_downcast_ref::<[u8; 16]>()
                .unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    // 2. Dynamic clone via `clone_mod_component_dynamic` (`to_dynamic()`)
    let cloned = world.clone_mod_component_dynamic(entity, handle).unwrap();
    assert!(cloned.is_dynamic());
    assert_eq!(
        cloned.get_represented_type_info().unwrap().type_path(),
        "c99_mod::Telemetry"
    );

    // 3. Advance change tick, mutate in-place via `with_reflect_mod_component_mut`, and verify
    //    both change detection AND byte-exact ECS storage!
    let prev_tick = world.change_tick();
    world.increment_change_tick();
    let new_target = world.spawn_empty().id();
    world
        .with_reflect_mod_component_mut(entity, handle, |mut r| {
            *r.reflect_path_mut("active")
                .unwrap()
                .try_downcast_mut::<bool>()
                .unwrap() = false;
            *r.reflect_path_mut("hp_f32")
                .unwrap()
                .try_downcast_mut::<f32>()
                .unwrap() = 512.25;
            *r.reflect_path_mut("score_i32")
                .unwrap()
                .try_downcast_mut::<i32>()
                .unwrap() = 1337;
            *r.reflect_path_mut("pos_vec3[1]")
                .unwrap()
                .try_downcast_mut::<f32>()
                .unwrap() = -9.5;
            *r.reflect_path_mut("uuid_bytes16[0]")
                .unwrap()
                .try_downcast_mut::<u8>()
                .unwrap() = 0xFE;
            *r.reflect_path_mut("target_entity")
                .unwrap()
                .try_downcast_mut::<u64>()
                .unwrap() = new_target.to_bits();
        })
        .unwrap();

    // Verify Bevy ECS change detection marked the component changed.
    let comp_id = world
        .resource::<ModRegistry>()
        .get_by_handle(handle)
        .unwrap()
        .component_id;
    let ticks = world
        .entity(entity)
        .get_change_ticks_by_id(comp_id)
        .unwrap();
    assert!(ticks.is_changed(prev_tick, world.read_change_tick()));

    // Verify byte-exact storage round-trip AND untouched padding/opaque bytes!
    let after = read_entity_pod_bytes(world, entity, handle).unwrap();
    assert_eq!(after[core::mem::offset_of!(C99Telemetry, active)], 0);
    assert_eq!(
        f32::from_ne_bytes(
            after[core::mem::offset_of!(C99Telemetry, hp_f32)..][..4]
                .try_into()
                .unwrap()
        ),
        512.25
    );
    assert_eq!(
        i32::from_ne_bytes(
            after[core::mem::offset_of!(C99Telemetry, score_i32)..][..4]
                .try_into()
                .unwrap()
        ),
        1337
    );
    assert_eq!(
        f32::from_ne_bytes(after[pos_off + 4..pos_off + 8].try_into().unwrap()),
        -9.5
    );
    assert_eq!(
        after[core::mem::offset_of!(C99Telemetry, uuid_bytes16)],
        0xFE
    );
    assert_eq!(
        u64::from_ne_bytes(
            after[core::mem::offset_of!(C99Telemetry, target_entity)..][..8]
                .try_into()
                .unwrap()
        ),
        new_target.to_bits()
    );
    // Opaque blob and interior padding bytes must remain completely untouched!
    assert_eq!(
        &after[core::mem::offset_of!(C99Telemetry, opaque_blob)..][..4],
        &[0xDE, 0xAD, 0xBE, 0xEF]
    );
    assert_eq!(after[3], 0xCC);
}

#[repr(C)]
struct C99Turret {
    ammo: u32,
    heat: f32,
    armed: u8,
    // 3 bytes padding
    aim: [f32; 3],
}

fn c99_turret_fields() -> Vec<BamFieldDesc> {
    vec![
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("ammo"),
            StrView::from_str("u32"),
            core::mem::offset_of!(C99Turret, ammo),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("heat"),
            StrView::from_str("f32"),
            core::mem::offset_of!(C99Turret, heat),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::BOOL,
            StrView::from_str("armed"),
            StrView::from_str("bool"),
            core::mem::offset_of!(C99Turret, armed),
            1,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("aim"),
            StrView::from_str("Vec3"),
            core::mem::offset_of!(C99Turret, aim),
            12,
            4,
        ),
    ]
}

#[test]
fn c99_dynamic_component_works_over_bevy_abi_mod_brp_and_standard_bevy_remote_brp() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, RemotePlugin::default(), ModReflectPlugin));

    let fields = c99_turret_fields();
    let handle = register_c99_dynamic(
        app.world_mut(),
        "c99_brp::TurretState",
        c"c99_brp::TurretState",
        core::mem::size_of::<C99Turret>(),
        core::mem::align_of::<C99Turret>(),
        0,
        &fields,
    );

    let entity = app.world_mut().spawn_empty().id();
    let mut raw = vec![0u8; core::mem::size_of::<C99Turret>()];
    raw[0..4].copy_from_slice(&30u32.to_ne_bytes());
    raw[4..8].copy_from_slice(&12.5f32.to_ne_bytes());
    raw[8] = 1;
    let aim_off = core::mem::offset_of!(C99Turret, aim);
    raw[aim_off..aim_off + 4].copy_from_slice(&1.0f32.to_ne_bytes());
    raw[aim_off + 4..aim_off + 8].copy_from_slice(&0.0f32.to_ne_bytes());
    raw[aim_off + 8..aim_off + 12].copy_from_slice(&0.0f32.to_ne_bytes());
    write_entity_pod_bytes(app.world_mut(), entity, handle, &raw).unwrap();

    let bare_ent_mod_brp = app.world_mut().spawn_empty().id();
    let bare_ent_std_brp = app.world_mut().spawn_empty().id();
    app.finish();
    app.cleanup();

    let world = app.world_mut();

    // --- A. `bevy_abi_mod/*` BRP JSON-RPC ---
    // 1. `bevy_abi_mod/list_components`
    let list_res = world
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_LIST_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(1)),
            params: Some(serde_json::json!({ "entity": entity.to_bits() })),
        })
        .unwrap();
    let turret_item = list_res
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "c99_brp::TurretState")
        .expect("c99_brp::TurretState listed on entity");
    let listed_fields = turret_item["fields"].as_array().unwrap();
    assert_eq!(listed_fields.len(), 4);
    assert_eq!(listed_fields[0]["name"], "ammo");
    assert_eq!(listed_fields[3]["name"], "aim");
    assert_eq!(listed_fields[3]["size"], 12);

    // 2. `bevy_abi_mod/get_components`
    let get_res = world
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_GET_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(2)),
            params: Some(serde_json::json!({
                "entity": entity.to_bits(),
                "components": ["c99_brp::TurretState"],
            })),
        })
        .unwrap();
    assert_eq!(get_res["components"]["c99_brp::TurretState"]["ammo"], 30);
    assert_eq!(get_res["components"]["c99_brp::TurretState"]["heat"], 12.5);
    assert_eq!(get_res["components"]["c99_brp::TurretState"]["armed"], true);
    assert_eq!(
        get_res["components"]["c99_brp::TurretState"]["aim"],
        serde_json::json!([1.0, 0.0, 0.0])
    );

    // 3. `bevy_abi_mod/mutate_components`
    world
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_MUTATE_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(3)),
            params: Some(serde_json::json!({
                "entity": entity.to_bits(),
                "component": "c99_brp::TurretState",
                "value": {
                    "ammo": 150,
                    "heat": 88.25,
                    "armed": false,
                    "aim": [0.0, 1.0, -0.5],
                }
            })),
        })
        .unwrap();
    let bytes = read_entity_pod_bytes(world, entity, handle).unwrap();
    assert_eq!(u32::from_ne_bytes(bytes[0..4].try_into().unwrap()), 150);
    assert_eq!(f32::from_ne_bytes(bytes[4..8].try_into().unwrap()), 88.25);
    assert_eq!(bytes[8], 0);
    assert_eq!(
        f32::from_ne_bytes(bytes[aim_off + 8..aim_off + 12].try_into().unwrap()),
        -0.5
    );

    // 4. `bevy_abi_mod/insert_components` onto a bare entity
    world
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_INSERT_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(4)),
            params: Some(serde_json::json!({
                "entity": bare_ent_mod_brp.to_bits(),
                "components": {
                    "c99_brp::TurretState": {
                        "ammo": 7,
                        "heat": 3.5,
                        "armed": true,
                        "aim": [2.0, 3.0, 4.0],
                    }
                }
            })),
        })
        .unwrap();
    let inserted_bytes = read_entity_pod_bytes(world, bare_ent_mod_brp, handle).unwrap();
    assert_eq!(
        u32::from_ne_bytes(inserted_bytes[0..4].try_into().unwrap()),
        7
    );
    assert_eq!(inserted_bytes[8], 1);

    // --- B. Standard Bevy 0.20 `bevy_remote` (`RemotePlugin`) BRP JSON-RPC ---
    // 1. `world.list_components` (global and per-entity)
    let std_list_all = call_remote_method(world, "world.list_components", None).unwrap();
    assert!(
        std_list_all
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "c99_brp::TurretState")
    );
    let std_list_ent = call_remote_method(
        world,
        "world.list_components",
        Some(serde_json::json!({ "entity": entity.to_bits() })),
    )
    .unwrap();
    assert!(
        std_list_ent
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "c99_brp::TurretState")
    );

    // 2. `world.get_components` (strict mode!)
    let std_get = call_remote_method(
        world,
        "world.get_components",
        Some(serde_json::json!({
            "entity": entity.to_bits(),
            "components": ["c99_brp::TurretState"],
            "strict": true,
        })),
    )
    .unwrap();
    assert_eq!(std_get["c99_brp::TurretState"]["ammo"], 150);
    assert_eq!(std_get["c99_brp::TurretState"]["heat"], 88.25);
    assert_eq!(std_get["c99_brp::TurretState"]["armed"], false);
    assert_eq!(
        std_get["c99_brp::TurretState"]["aim"],
        serde_json::json!([0.0, 1.0, -0.5])
    );

    // 3. `world.mutate_components` (scalar field AND nested array element path!)
    call_remote_method(
        world,
        "world.mutate_components",
        Some(serde_json::json!({
            "entity": entity.to_bits(),
            "component": "c99_brp::TurretState",
            "path": "ammo",
            "value": 999,
        })),
    )
    .unwrap();
    call_remote_method(
        world,
        "world.mutate_components",
        Some(serde_json::json!({
            "entity": entity.to_bits(),
            "component": "c99_brp::TurretState",
            "path": "aim[2]",
            "value": 42.75,
        })),
    )
    .unwrap();

    // Verify byte-exact mutation in ECS storage!
    let after_std_mut = read_entity_pod_bytes(world, entity, handle).unwrap();
    assert_eq!(
        u32::from_ne_bytes(after_std_mut[0..4].try_into().unwrap()),
        999
    );
    assert_eq!(
        f32::from_ne_bytes(after_std_mut[aim_off + 8..aim_off + 12].try_into().unwrap()),
        42.75
    );

    // 4. `world.insert_components` onto a bare entity via Bevy's own deserializer + ReflectComponent
    call_remote_method(
        world,
        "world.insert_components",
        Some(serde_json::json!({
            "entity": bare_ent_std_brp.to_bits(),
            "components": {
                "c99_brp::TurretState": {
                    "ammo": 55,
                    "heat": 1.25,
                    "armed": true,
                    "aim": [7.0, 8.0, 9.0],
                }
            }
        })),
    )
    .unwrap();
    let std_inserted = read_entity_pod_bytes(world, bare_ent_std_brp, handle).unwrap();
    assert_eq!(
        u32::from_ne_bytes(std_inserted[0..4].try_into().unwrap()),
        55
    );
    assert_eq!(
        f32::from_ne_bytes(std_inserted[4..8].try_into().unwrap()),
        1.25
    );
    assert_eq!(std_inserted[8], 1);
    assert_eq!(
        f32::from_ne_bytes(std_inserted[aim_off + 4..aim_off + 8].try_into().unwrap()),
        8.0
    );

    // 5. `registry.schema` exports our dynamic C99 struct schema!
    let schema_res = call_remote_method(world, "registry.schema", None).unwrap();
    let turret_schema = schema_res
        .get("c99_brp::TurretState")
        .expect("registry.schema must include C99 BamFieldDesc struct");
    assert_eq!(turret_schema["shortPath"], "TurretState");
    assert!(turret_schema["properties"]["ammo"].is_object());
    assert!(turret_schema["properties"]["aim"].is_object());
}

#[repr(C)]
#[derive(Resource)]
struct C99PhysicsConfig {
    gravity: f32,
    substeps: u32,
    enabled: u8,
    // 3 bytes padding
    wind: [f32; 3],
}

const PHYSICS_CONFIG_FIELDS: &[BamFieldDesc] = &[
    BamFieldDesc::new(
        BamFieldKind::F32,
        StrView::from_str("gravity"),
        StrView::from_str("f32"),
        core::mem::offset_of!(C99PhysicsConfig, gravity),
        4,
        4,
    ),
    BamFieldDesc::new(
        BamFieldKind::U32,
        StrView::from_str("substeps"),
        StrView::from_str("u32"),
        core::mem::offset_of!(C99PhysicsConfig, substeps),
        4,
        4,
    ),
    BamFieldDesc::new(
        BamFieldKind::BOOL,
        StrView::from_str("enabled"),
        StrView::from_str("bool"),
        core::mem::offset_of!(C99PhysicsConfig, enabled),
        1,
        1,
    ),
    BamFieldDesc::new(
        BamFieldKind::NESTED,
        StrView::from_str("wind"),
        StrView::from_str("Vec3"),
        core::mem::offset_of!(C99PhysicsConfig, wind),
        12,
        4,
    ),
];

// Manual `ModComponent` supplying ONLY declarative `BAM_FIELDS` (ZERO witness VTable, ZERO
// `#[derive(Reflect)]`).
impl ModComponent for C99PhysicsConfig {
    const CANONICAL_NAME: &'static str = "c99_res::PhysicsConfig";
    const COMPONENT_CSTR: &'static CStr = c"c99_res::PhysicsConfig";
    const UUID: ComponentUuid = ComponentUuid::from_parts(
        "c99_res::PhysicsConfig",
        core::mem::size_of::<Self>(),
        core::mem::align_of::<Self>(),
        0,
    );
    const IS_ABI_POD: bool = true;
    const BAM_FIELDS: &'static [BamFieldDesc] = PHYSICS_CONFIG_FIELDS;
    type VTable = ();
    fn vtable() -> &'static Self::VTable {
        &()
    }
    fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
        &[]
    }
}

#[test]
fn c99_dynamic_resource_reflection_brp_and_settings_toml_roundtrip() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, RemotePlugin::default(), ModReflectPlugin));

    let res_h = app.register_mod_resource::<C99PhysicsConfig>().unwrap();
    app.insert_resource(C99PhysicsConfig {
        gravity: 9.81,
        substeps: 4,
        enabled: 1,
        wind: [0.5, 0.0, -1.5],
    });

    app.register_mod_settings_resource(res_h, "c99_physics")
        .unwrap();
    app.finish();
    app.cleanup();

    let world = app.world_mut();
    assert!(
        world
            .resource::<ModReflectRegistry>()
            .witness_schema(res_h)
            .is_none(),
        "resource has no witness VTable"
    );

    // 1. Live `ReflectFromPtr` read & mutate on dynamic singleton resource
    {
        let r = world.reflect_mod_resource(res_h).unwrap();
        assert_eq!(
            *r.reflect_path("substeps")
                .unwrap()
                .try_downcast_ref::<u32>()
                .unwrap(),
            4
        );
    }
    world
        .with_reflect_mod_resource_mut(res_h, |mut r| {
            *r.reflect_path_mut("substeps")
                .unwrap()
                .try_downcast_mut::<u32>()
                .unwrap() = 8;
            *r.reflect_path_mut("wind[0]")
                .unwrap()
                .try_downcast_mut::<f32>()
                .unwrap() = 2.25;
        })
        .unwrap();

    // 2. `bevy_abi_mod/get_resources` & `bevy_abi_mod/mutate_resources`
    let get_res = world
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_GET_RESOURCES.to_owned(),
            id: Some(serde_json::json!(1)),
            params: Some(serde_json::json!({ "resources": ["c99_res::PhysicsConfig"] })),
        })
        .unwrap();
    assert_eq!(get_res["c99_res::PhysicsConfig"]["substeps"], 8);
    assert_eq!(
        get_res["c99_res::PhysicsConfig"]["wind"],
        serde_json::json!([2.25, 0.0, -1.5])
    );

    world
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_MUTATE_RESOURCES.to_owned(),
            id: Some(serde_json::json!(2)),
            params: Some(serde_json::json!({
                "resource": "c99_res::PhysicsConfig",
                "value": { "gravity": 3.71, "enabled": false, "wind": [1.0, 2.0, 3.0] },
            })),
        })
        .unwrap();

    // 3. `bevy_settings` TOML export -> overwrite -> import round-trip
    let toml_out = world.export_mod_settings_toml().unwrap();
    assert!(toml_out.contains("[c99_physics]"));
    assert!(toml_out.contains("substeps = 8"));

    let mut override_dyn = DynamicStruct::default();
    override_dyn.insert("substeps", 1u32);
    override_dyn.insert("enabled", true);
    world
        .apply_bam_fields_resource_dynamic_struct(res_h, &override_dyn)
        .unwrap();
    assert_eq!(
        u32::from_ne_bytes(
            read_resource_bam_field_bytes(world, res_h, 4, 4)
                .unwrap()
                .try_into()
                .unwrap()
        ),
        1
    );

    let applied = world.import_mod_settings_toml(&toml_out).unwrap();
    assert_eq!(applied, 1);
    assert_eq!(
        u32::from_ne_bytes(
            read_resource_bam_field_bytes(world, res_h, 4, 4)
                .unwrap()
                .try_into()
                .unwrap()
        ),
        8
    );
    assert_eq!(
        read_resource_bam_field_bytes(world, res_h, 8, 1).unwrap(),
        vec![0]
    );
}

#[test]
fn c99_immutable_component_rejects_mutations_via_reflect_and_brp() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, RemotePlugin::default(), ModReflectPlugin));

    let fields = [BamFieldDesc::new(
        BamFieldKind::U32,
        StrView::from_str("seed"),
        StrView::from_str("u32"),
        0,
        4,
        4,
    )];
    let handle = register_c99_dynamic(
        app.world_mut(),
        "c99_immut::WorldSeed",
        c"c99_immut::WorldSeed",
        4,
        4,
        BAM_COMPONENT_FLAG_IMMUTABLE,
        &fields,
    );

    // Insert via raw host command (immutable components may be initialized once at spawn).
    let snap = app.world().resource::<ModRegistry>().snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    let ffi_ent = {
        let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap);
        let mut ent = FfiEntity::NULL;
        // SAFETY: Valid `cmds` handle and `ent` out-pointer.
        let spawn_st = unsafe { host_cmd_spawn_empty(cmds.as_ffi_ptr(), &mut ent) };
        assert_eq!(spawn_st, FfiResult::OK);
        let payload = 0x1357_9BDFu32.to_ne_bytes();
        // SAFETY: Valid `cmds`, `ent`, and 4-byte `payload`.
        let ins_st =
            unsafe { host_cmd_insert(cmds.as_ffi_ptr(), ent, handle, payload.as_ptr(), 4) };
        assert_eq!(ins_st, FfiResult::OK);
        ent
    };
    storage.apply_to_world(app.world_mut(), &snap);
    let entity = Entity::try_from_bits(ffi_ent.to_bits()).unwrap();

    app.finish();
    app.cleanup();

    let world = app.world_mut();

    // Reads succeed via both `reflect_mod_component` and BRP!
    let r = world.reflect_mod_component(entity, handle).unwrap();
    assert_eq!(
        *r.reflect_path("seed")
            .unwrap()
            .try_downcast_ref::<u32>()
            .unwrap(),
        0x1357_9BDF
    );
    let std_get = call_remote_method(
        world,
        "world.get_components",
        Some(serde_json::json!({
            "entity": entity.to_bits(),
            "components": ["c99_immut::WorldSeed"],
            "strict": true,
        })),
    )
    .unwrap();
    assert_eq!(std_get["c99_immut::WorldSeed"]["seed"], 0x1357_9BDFu32);

    // All mutation paths MUST reject immutable components cleanly!
    let mut_res = world.with_reflect_mod_component_mut(entity, handle, |_| {});
    assert_eq!(mut_res, Err(FfiResult::PERMISSION_DENIED));

    let mod_brp_err = world.dispatch_mod_brp_request(BrpRequest {
        method: BRP_METHOD_MUTATE_COMPONENTS.to_owned(),
        id: Some(serde_json::json!(1)),
        params: Some(serde_json::json!({
            "entity": entity.to_bits(),
            "component": "c99_immut::WorldSeed",
            "value": { "seed": 42 },
        })),
    });
    assert!(mod_brp_err.is_err());

    let std_brp_err = call_remote_method(
        world,
        "world.mutate_components",
        Some(serde_json::json!({
            "entity": entity.to_bits(),
            "component": "c99_immut::WorldSeed",
            "path": "seed",
            "value": 42,
        })),
    );
    assert!(std_brp_err.is_err());

    // Raw ECS bytes remain completely unchanged!
    assert_eq!(
        read_entity_pod_bytes(world, entity, handle).unwrap(),
        0x1357_9BDFu32.to_ne_bytes().to_vec()
    );
}

#[test]
fn c99_packed_unaligned_and_invalid_bam_field_descriptors_validation() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModReflectPlugin));

    // 1. Packed unaligned C99 struct (`align = 1`, `f32` at offset 1, `u64` at offset 5 -> size 13)
    let packed_fields = [
        BamFieldDesc::new(
            BamFieldKind::U8,
            StrView::from_str("kind"),
            StrView::from_str("u8"),
            0,
            1,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("ratio"),
            StrView::from_str("f32"),
            1,
            4,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::U64,
            StrView::from_str("id"),
            StrView::from_str("u64"),
            5,
            8,
            1,
        ),
    ];
    let packed_h = register_c99_dynamic(
        app.world_mut(),
        "c99_packed::WireHeader",
        c"c99_packed::WireHeader",
        13,
        1,
        0,
        &packed_fields,
    );

    // 2. Invalid descriptors: OOB offset, misaligned offset, bad primitive size, duplicate names
    let oob_h = register_c99_dynamic(
        app.world_mut(),
        "c99_bad::OobField",
        c"c99_bad::OobField",
        8,
        4,
        0,
        &[BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("x"),
            StrView::from_str("u32"),
            8,
            4,
            4,
        )],
    );
    let misalign_h = register_c99_dynamic(
        app.world_mut(),
        "c99_bad::MisalignedField",
        c"c99_bad::MisalignedField",
        8,
        4,
        0,
        &[BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("x"),
            StrView::from_str("u32"),
            2,
            4,
            4,
        )],
    );
    let bad_size_h = register_c99_dynamic(
        app.world_mut(),
        "c99_bad::BadPrimSize",
        c"c99_bad::BadPrimSize",
        8,
        8,
        0,
        &[BamFieldDesc::new(
            BamFieldKind::F64,
            StrView::from_str("x"),
            StrView::from_str("f64"),
            0,
            4,
            4,
        )],
    );
    let dup_h = register_c99_dynamic(
        app.world_mut(),
        "c99_bad::DupNames",
        c"c99_bad::DupNames",
        8,
        4,
        0,
        &[
            BamFieldDesc::new(
                BamFieldKind::U32,
                StrView::from_str("dup"),
                StrView::from_str("u32"),
                0,
                4,
                4,
            ),
            BamFieldDesc::new(
                BamFieldKind::U32,
                StrView::from_str("dup"),
                StrView::from_str("u32"),
                4,
                4,
                4,
            ),
        ],
    );

    // `ModReflectPlugin::finish` gracefully skips bad descriptors without panicking!
    app.finish();
    app.cleanup();

    let world = app.world_mut();
    assert_eq!(
        world.register_bam_field_reflection(oob_h),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        world.register_bam_field_reflection(misalign_h),
        Err(FfiResult::INVALID_ARGUMENT)
    );
    assert_eq!(
        world.register_bam_field_reflection(bad_size_h),
        Err(FfiResult::INVALID_ARGUMENT)
    );
    assert_eq!(
        world.register_bam_field_reflection(dup_h),
        Err(FfiResult::INVALID_ARGUMENT)
    );

    // Packed unaligned struct must NOT register an unsound in-place `&f32`/`&u64` ReflectFromPtr
    // adapter (`reflected_type_id == None`), yet byte-offset DynamicStruct + BRP round-trip works!
    let packed_schema = world
        .resource::<ModReflectRegistry>()
        .bam_field_schema(packed_h)
        .expect("packed struct schema registered");
    assert!(
        packed_schema.reflected_type_id.is_none(),
        "unaligned packed struct must not bind in-place ReflectFromPtr"
    );

    let ent = world.spawn_empty().id();
    world
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_INSERT_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(1)),
            params: Some(serde_json::json!({
                "entity": ent.to_bits(),
                "components": {
                    "c99_packed::WireHeader": {
                        "kind": 9,
                        "ratio": 0.75,
                        "id": 0x1122_3344_5566_7788u64,
                    }
                }
            })),
        })
        .unwrap();

    // DynamicStruct clone & byte-exact verification
    let dyn_val = world.clone_mod_component_dynamic(ent, packed_h).unwrap();
    let dyn_s = dyn_val.reflect_ref().as_struct().unwrap();
    assert_eq!(
        *dyn_s
            .field("kind")
            .unwrap()
            .try_downcast_ref::<u8>()
            .unwrap(),
        9
    );
    assert_eq!(
        *dyn_s
            .field("ratio")
            .unwrap()
            .try_downcast_ref::<f32>()
            .unwrap(),
        0.75
    );
    assert_eq!(
        *dyn_s
            .field("id")
            .unwrap()
            .try_downcast_ref::<u64>()
            .unwrap(),
        0x1122_3344_5566_7788u64
    );

    let mut patch = DynamicStruct::default();
    patch.insert("ratio", 1.5f32);
    world
        .apply_bam_fields_dynamic_struct(ent, packed_h, &patch)
        .unwrap();
    let raw_bytes = read_entity_pod_bytes(world, ent, packed_h).unwrap();
    assert_eq!(raw_bytes.len(), 13);
    assert_eq!(raw_bytes[0], 9);
    assert_eq!(f32::from_ne_bytes(raw_bytes[1..5].try_into().unwrap()), 1.5);
    assert_eq!(
        u64::from_ne_bytes(raw_bytes[5..13].try_into().unwrap()),
        0x1122_3344_5566_7788u64
    );
}
