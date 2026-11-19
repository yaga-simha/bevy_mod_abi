//! Integration tests for versioned persistent (`!ModScoped`) save-game archiving, recovery
//! policies (`StrictRequireMods`, `StripMissingModComponents`, `MigrateOnLoad`), and schema
//! evolution (`ModSaveGamePlugin`).

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::{
    LoadContext, host_register_component, read_entity_pod_bytes, write_entity_pod_bytes,
};
use bevy_abi_mod_reflect::savegame::{
    MOD_SAVE_ARCHIVE_VERSION, MigratedComponentPayload, ModSaveAppExt, ModSaveArchive,
    ModSaveFormatPreference, ModSaveGamePlugin, ModSaveLoadError, ModSaveLoadPolicy,
    ModSaveOptions, SavedComponentFormat,
};
use bevy_abi_mod_sys::{
    BamComponent, BamFieldDesc, BamFieldKind, ComponentDesc, ComponentUuid, FfiOption, FfiResult,
    StrView,
};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::name::Name;
use bevy_ecs::world::World;
use core::ffi::CStr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

fn temp_save_path(tag: &str, ext: &str) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "bam_savegame_{tag}_{}_{}.{ext}",
        std::process::id(),
        seq
    ))
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "game::HostPosition")]
struct HostPosition {
    x: f32,
    y: f32,
    entity_tag: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct ModAlphaFaction {
    faction_id: u32,
    reputation: i32,
    power_scaled: f32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct ModBetaAura {
    spell_id: u32,
    intensity: f32,
    remaining_ticks: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct UnitStatsV1 {
    hp: f32,
    level: u32,
    deprecated_tag: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct UnitStatsV2 {
    level: u64,
    hp: f32,
    shield: f32,
}

#[derive(Component, Clone, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "rpg::Profile")]
struct ProfileV1 {
    hp: f32,
    title: String,
    loot_ids: Vec<u32>,
    guild_id: FfiOption<u32>,
    obsolete_flag: bool,
}

#[derive(Component, Clone, Debug, PartialEq, ModComponent)]
#[mod_component(name = "rpg::Profile")]
struct ProfileV2 {
    hp: f32,
    title: String,
    loot_ids: Vec<u32>,
    guild_id: FfiOption<u32>,
    prestige_rank: u32,
}

impl Default for ProfileV2 {
    fn default() -> Self {
        Self {
            hp: 0.0,
            title: String::new(),
            loot_ids: Vec::new(),
            guild_id: FfiOption::none(),
            prestige_rank: 77,
        }
    }
}

fn spawn_active_mod_info(world: &mut World, id: ModId, name: &str, version: &str) {
    world.spawn((
        Name::new(format!("mod:{name}")),
        LoadedModInfo {
            id,
            name: name.to_owned(),
            version: version.to_owned(),
            path: PathBuf::from(format!("/mods/lib{name}.so")),
            system_count: 1,
            fault_count: 0,
        },
    ));
}

#[allow(clippy::too_many_arguments)]
fn register_dynamic_mod_component(
    world: &mut World,
    mod_id: ModId,
    name: &'static str,
    c_name: &'static CStr,
    size: usize,
    align: usize,
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
    .with_schema_fingerprint(schema_fingerprint)
    .with_fields(fields.as_ptr(), fields.len());

    let mut handle = BamComponent::INVALID;
    // SAFETY: `load_ctx`, `desc`, `fields`, and `handle` are valid for this synchronous call.
    let status = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut handle) };
    assert_eq!(status, FfiResult::OK, "failed registering {name}");
    load_ctx.finish_init();
    handle
}

fn mod_alpha_fields() -> Vec<BamFieldDesc> {
    vec![
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("faction_id"),
            StrView::from_str("u32"),
            core::mem::offset_of!(ModAlphaFaction, faction_id),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::I32,
            StrView::from_str("reputation"),
            StrView::from_str("i32"),
            core::mem::offset_of!(ModAlphaFaction, reputation),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("power_scaled"),
            StrView::from_str("f32"),
            core::mem::offset_of!(ModAlphaFaction, power_scaled),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("flags"),
            StrView::from_str("u32"),
            core::mem::offset_of!(ModAlphaFaction, flags),
            4,
            4,
        ),
    ]
}

fn mod_beta_fields() -> Vec<BamFieldDesc> {
    vec![
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("spell_id"),
            StrView::from_str("u32"),
            core::mem::offset_of!(ModBetaAura, spell_id),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("intensity"),
            StrView::from_str("f32"),
            core::mem::offset_of!(ModBetaAura, intensity),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("remaining_ticks"),
            StrView::from_str("u32"),
            core::mem::offset_of!(ModBetaAura, remaining_ticks),
            4,
            4,
        ),
    ]
}

fn stats_v1_fields() -> Vec<BamFieldDesc> {
    vec![
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("hp"),
            StrView::from_str("f32"),
            core::mem::offset_of!(UnitStatsV1, hp),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("level"),
            StrView::from_str("u32"),
            core::mem::offset_of!(UnitStatsV1, level),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("deprecated_tag"),
            StrView::from_str("u32"),
            core::mem::offset_of!(UnitStatsV1, deprecated_tag),
            4,
            4,
        ),
    ]
}

fn stats_v2_fields() -> Vec<BamFieldDesc> {
    vec![
        BamFieldDesc::new(
            BamFieldKind::U64,
            StrView::from_str("level"),
            StrView::from_str("u64"),
            core::mem::offset_of!(UnitStatsV2, level),
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("hp"),
            StrView::from_str("f32"),
            core::mem::offset_of!(UnitStatsV2, hp),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("shield"),
            StrView::from_str("f32"),
            core::mem::offset_of!(UnitStatsV2, shield),
            4,
            4,
        ),
    ]
}

fn pod_alpha_bytes(v: &ModAlphaFaction) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&v.faction_id.to_ne_bytes());
    out[4..8].copy_from_slice(&v.reputation.to_ne_bytes());
    out[8..12].copy_from_slice(&v.power_scaled.to_ne_bytes());
    out[12..16].copy_from_slice(&v.flags.to_ne_bytes());
    out
}

fn pod_beta_bytes(v: &ModBetaAura) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(&v.spell_id.to_ne_bytes());
    out[4..8].copy_from_slice(&v.intensity.to_ne_bytes());
    out[8..12].copy_from_slice(&v.remaining_ticks.to_ne_bytes());
    out
}

fn stats_v1_bytes(v: &UnitStatsV1) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(&v.hp.to_ne_bytes());
    out[4..8].copy_from_slice(&v.level.to_ne_bytes());
    out[8..12].copy_from_slice(&v.deprecated_tag.to_ne_bytes());
    out
}

fn read_stats_v2(world: &World, entity: Entity, handle: BamComponent) -> UnitStatsV2 {
    let bytes = read_entity_pod_bytes(world, entity, handle).unwrap();
    assert_eq!(bytes.len(), core::mem::size_of::<UnitStatsV2>());
    let level = u64::from_ne_bytes(bytes[0..8].try_into().unwrap());
    let hp = f32::from_ne_bytes(bytes[8..12].try_into().unwrap());
    let shield = f32::from_ne_bytes(bytes[12..16].try_into().unwrap());
    UnitStatsV2 { level, hp, shield }
}

#[test]
fn save_5000_entities_two_mods_strip_missing_mod_b_preserves_mod_a_byte_for_byte() {
    const ENTITY_COUNT: usize = 5_000;
    const ALPHA_FP: u64 = 0x1111_2222_3333_4444;
    const BETA_FP: u64 = 0x5555_6666_7777_8888;

    let mut save_app = App::new();
    save_app.add_plugins((AbiModPlugin, ModSaveGamePlugin::new()));
    save_app
        .register_mod_component::<HostPosition>(ComponentOptions::new().with_raw_insert())
        .unwrap();

    let mod_a = ModId(10);
    let mod_b = ModId(20);
    spawn_active_mod_info(save_app.world_mut(), mod_a, "mod_alpha", "1.2.0");
    spawn_active_mod_info(save_app.world_mut(), mod_b, "mod_beta", "2.0.0");

    let alpha_fields = mod_alpha_fields();
    let beta_fields = mod_beta_fields();
    let alpha_h = register_dynamic_mod_component(
        save_app.world_mut(),
        mod_a,
        "mod_alpha::FactionState",
        c"mod_alpha::FactionState",
        core::mem::size_of::<ModAlphaFaction>(),
        core::mem::align_of::<ModAlphaFaction>(),
        ALPHA_FP,
        &alpha_fields,
    );
    let beta_h = register_dynamic_mod_component(
        save_app.world_mut(),
        mod_b,
        "mod_beta::SpellAura",
        c"mod_beta::SpellAura",
        core::mem::size_of::<ModBetaAura>(),
        core::mem::align_of::<ModBetaAura>(),
        BETA_FP,
        &beta_fields,
    );

    // Spawn 5,000 persistent (!ModScoped) entities carrying Host + Mod A + Mod B components.
    let mut expected_by_tag: HashMap<u32, (HostPosition, [u8; 16])> =
        HashMap::with_capacity(ENTITY_COUNT);
    for i in 0..ENTITY_COUNT {
        let idx = i as u32;
        let pos = HostPosition {
            x: (idx as f32) * 1.25 - 500.0,
            y: (idx as f32) * -0.5 + 42.0,
            entity_tag: idx,
        };
        let alpha = ModAlphaFaction {
            faction_id: idx % 17,
            reputation: (idx as i32) * 3 - 7_500,
            power_scaled: (idx as f32) * 0.125 + 1.0,
            flags: idx.rotate_left(7) ^ 0xA5A5_5A5A,
        };
        let beta = ModBetaAura {
            spell_id: 9000 + idx,
            intensity: (idx as f32) * 0.01,
            remaining_ticks: idx % 120,
        };
        let alpha_bytes = pod_alpha_bytes(&alpha);
        let beta_bytes = pod_beta_bytes(&beta);

        let e = save_app.world_mut().spawn(pos).id();
        write_entity_pod_bytes(save_app.world_mut(), e, alpha_h, &alpha_bytes).unwrap();
        write_entity_pod_bytes(save_app.world_mut(), e, beta_h, &beta_bytes).unwrap();
        expected_by_tag.insert(idx, (pos, alpha_bytes));
    }

    // Spawn 1 persistent entity owned exclusively by Mod B (should not leak as a ghost entity).
    let beta_only_e = save_app.world_mut().spawn_empty().id();
    let beta_only_bytes = pod_beta_bytes(&ModBetaAura {
        spell_id: 777,
        intensity: 3.5,
        remaining_ticks: 99,
    });
    write_entity_pod_bytes(save_app.world_mut(), beta_only_e, beta_h, &beta_only_bytes).unwrap();

    // Spawn 35 ephemeral ModScoped entities that MUST NOT be serialized.
    for i in 0..25u32 {
        let e = save_app
            .world_mut()
            .spawn((
                ModScoped(mod_a),
                HostPosition {
                    x: 9999.0,
                    y: i as f32,
                    entity_tag: 1_000_000 + i,
                },
            ))
            .id();
        let ab = pod_alpha_bytes(&ModAlphaFaction {
            faction_id: 99,
            reputation: -999,
            power_scaled: 0.0,
            flags: 0,
        });
        write_entity_pod_bytes(save_app.world_mut(), e, alpha_h, &ab).unwrap();
    }
    for _ in 0..10 {
        let e = save_app.world_mut().spawn(ModScoped(mod_b)).id();
        write_entity_pod_bytes(save_app.world_mut(), e, beta_h, &beta_only_bytes).unwrap();
    }

    // Save archive and verify headers + JSON & binary roundtrip.
    let archive = save_app.save_mod_archive().unwrap();
    assert_eq!(archive.format_version, MOD_SAVE_ARCHIVE_VERSION);
    assert_eq!(
        archive.entities.len(),
        ENTITY_COUNT + 1,
        "all 35 ModScoped entities must be excluded while all 5,001 persistent entities are saved"
    );
    assert_eq!(archive.mods.len(), 2);
    assert_eq!(archive.component_headers.len(), 3);

    let alpha_hdr = archive
        .component_headers
        .iter()
        .find(|h| h.component_name == "mod_alpha::FactionState")
        .unwrap();
    assert_eq!(alpha_hdr.mod_id, Some(mod_a.0));
    assert_eq!(alpha_hdr.mod_name, "mod_alpha");
    assert_eq!(alpha_hdr.semver, "1.2.0");
    assert_eq!(alpha_hdr.schema_fingerprint, ALPHA_FP);
    assert_eq!(alpha_hdr.size, 16);
    assert_eq!(alpha_hdr.align, 4);
    assert_eq!(alpha_hdr.format, SavedComponentFormat::RawPodHex);
    assert_eq!(alpha_hdr.fields.len(), 4);

    let json_text = archive.to_json_string().unwrap();
    let archive_from_json = ModSaveArchive::from_json_str(&json_text).unwrap();
    assert_eq!(archive_from_json, archive);

    let bin_bytes = archive.to_binary_bytes().unwrap();
    let archive_from_bin = ModSaveArchive::from_binary_bytes(&bin_bytes).unwrap();
    assert_eq!(archive_from_bin, archive);

    // Build World 2 with Mod B DISABLED (only Host + Mod A active!).
    let mut load_app = App::new();
    load_app.add_plugins((AbiModPlugin, ModSaveGamePlugin::new()));
    load_app
        .register_mod_component::<HostPosition>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    let mod_a_reloaded = ModId(101);
    spawn_active_mod_info(load_app.world_mut(), mod_a_reloaded, "mod_alpha", "1.2.0");
    let alpha_h_v2 = register_dynamic_mod_component(
        load_app.world_mut(),
        mod_a_reloaded,
        "mod_alpha::FactionState",
        c"mod_alpha::FactionState",
        core::mem::size_of::<ModAlphaFaction>(),
        core::mem::align_of::<ModAlphaFaction>(),
        ALPHA_FP,
        &alpha_fields,
    );

    // 1. StrictRequireMods MUST reject atomically with MissingMods { missing: ["mod_beta"] }.
    let strict_err = load_app
        .load_mod_archive(&archive_from_bin, ModSaveLoadPolicy::StrictRequireMods)
        .unwrap_err();
    match strict_err {
        ModSaveLoadError::MissingMods { missing } => {
            assert_eq!(missing, vec!["mod_beta".to_owned()]);
        }
        other => panic!("expected MissingMods, got {other:?}"),
    }
    let mut strict_check_q = load_app.world_mut().query::<&HostPosition>();
    assert_eq!(
        strict_check_q.iter(load_app.world()).count(),
        0,
        "StrictRequireMods failure must not spawn partial entities"
    );

    // 2. StripMissingModComponents restores all 5,000 Host + Mod A components byte-for-byte while
    //    stripping all 5,001 Mod B components cleanly.
    let report = load_app
        .load_mod_archive(
            &archive_from_bin,
            ModSaveLoadPolicy::StripMissingModComponents,
        )
        .unwrap();
    assert_eq!(report.restored_entities, ENTITY_COUNT);
    assert_eq!(report.restored_components, ENTITY_COUNT * 2);
    assert_eq!(report.stripped_components, ENTITY_COUNT + 1);
    assert_eq!(report.migrated_components, 0);
    assert_eq!(
        report.stripped_component_names,
        vec!["mod_beta::SpellAura".to_owned()]
    );
    assert_eq!(report.entity_map.len(), ENTITY_COUNT);

    // Verify EVERY SINGLE ONE of the 5,000 entities byte-for-byte!
    let mut seen_tags = 0usize;
    let restored_entities: Vec<(Entity, HostPosition)> = {
        let mut q = load_app.world_mut().query::<(Entity, &HostPosition)>();
        q.iter(load_app.world()).map(|(e, &p)| (e, p)).collect()
    };
    assert_eq!(restored_entities.len(), ENTITY_COUNT);

    for (entity, restored_pos) in restored_entities {
        let (expected_pos, expected_alpha_bytes) = expected_by_tag
            .remove(&restored_pos.entity_tag)
            .expect("unexpected or duplicate entity_tag restored");
        assert_eq!(restored_pos, expected_pos);
        let restored_alpha_bytes =
            read_entity_pod_bytes(load_app.world(), entity, alpha_h_v2).unwrap();
        assert_eq!(
            restored_alpha_bytes.as_slice(),
            &expected_alpha_bytes,
            "Mod A raw bytes must be preserved byte-for-byte for entity_tag={}",
            restored_pos.entity_tag
        );
        seen_tags += 1;
    }
    assert_eq!(seen_tags, ENTITY_COUNT);
    assert!(expected_by_tag.is_empty());
}

#[test]
fn strict_require_mods_rejects_version_and_schema_fingerprint_mismatches() {
    let mut save_app = App::new();
    save_app.add_plugins((AbiModPlugin, ModSaveGamePlugin::new()));
    let mod_a = ModId(1);
    spawn_active_mod_info(save_app.world_mut(), mod_a, "rpg_core", "1.0.0");
    let v1_fields = stats_v1_fields();
    let v1_h = register_dynamic_mod_component(
        save_app.world_mut(),
        mod_a,
        "rpg_core::UnitStats",
        c"rpg_core::UnitStats",
        core::mem::size_of::<UnitStatsV1>(),
        core::mem::align_of::<UnitStatsV1>(),
        0xAAAA_0001,
        &v1_fields,
    );

    let e = save_app.world_mut().spawn_empty().id();
    let raw = stats_v1_bytes(&UnitStatsV1 {
        hp: 100.0,
        level: 5,
        deprecated_tag: 9,
    });
    write_entity_pod_bytes(save_app.world_mut(), e, v1_h, &raw).unwrap();
    let archive = save_app.save_mod_archive().unwrap();

    // Case 1: Active mod has SemVer 2.0.0 != saved 1.0.0 -> VersionMismatch.
    let mut ver_app = App::new();
    ver_app.add_plugins((AbiModPlugin, ModSaveGamePlugin::new()));
    spawn_active_mod_info(ver_app.world_mut(), ModId(2), "rpg_core", "2.0.0");
    register_dynamic_mod_component(
        ver_app.world_mut(),
        ModId(2),
        "rpg_core::UnitStats",
        c"rpg_core::UnitStats",
        core::mem::size_of::<UnitStatsV1>(),
        core::mem::align_of::<UnitStatsV1>(),
        0xAAAA_0001,
        &v1_fields,
    );
    let err = ver_app
        .load_mod_archive(&archive, ModSaveLoadPolicy::StrictRequireMods)
        .unwrap_err();
    assert!(matches!(
        err,
        ModSaveLoadError::VersionMismatch {
            ref mod_name,
            ref saved_semver,
            ref active_semver
        } if mod_name == "rpg_core" && saved_semver == "1.0.0" && active_semver == "2.0.0"
    ));

    // Case 2: Same SemVer 1.0.0, but schema_fingerprint changed -> SchemaFingerprintMismatch.
    let mut fp_app = App::new();
    fp_app.add_plugins((AbiModPlugin, ModSaveGamePlugin::new()));
    spawn_active_mod_info(fp_app.world_mut(), ModId(3), "rpg_core", "1.0.0");
    let v2_fields = stats_v2_fields();
    register_dynamic_mod_component(
        fp_app.world_mut(),
        ModId(3),
        "rpg_core::UnitStats",
        c"rpg_core::UnitStats",
        core::mem::size_of::<UnitStatsV2>(),
        core::mem::align_of::<UnitStatsV2>(),
        0xBBBB_0002,
        &v2_fields,
    );
    let fp_err = fp_app
        .load_mod_archive(&archive, ModSaveLoadPolicy::StrictRequireMods)
        .unwrap_err();
    assert!(matches!(
        fp_err,
        ModSaveLoadError::SchemaFingerprintMismatch {
            ref component_name,
            saved_fingerprint: 0xAAAA_0001,
            active_fingerprint: 0xBBBB_0002,
        } if component_name == "rpg_core::UnitStats"
    ));
}

#[test]
fn migrate_on_load_upgrades_bam_field_v1_to_v2_automatically_and_via_custom_migrator() {
    let mut save_app = App::new();
    save_app.add_plugins((AbiModPlugin, ModSaveGamePlugin::new()));
    let mod_v1 = ModId(50);
    spawn_active_mod_info(save_app.world_mut(), mod_v1, "rpg_core", "1.0.0");

    let v1_fields = stats_v1_fields();
    let v1_h = register_dynamic_mod_component(
        save_app.world_mut(),
        mod_v1,
        "rpg_core::UnitStats",
        c"rpg_core::UnitStats",
        core::mem::size_of::<UnitStatsV1>(),
        core::mem::align_of::<UnitStatsV1>(),
        0x1111_0001,
        &v1_fields,
    );

    let samples = [
        UnitStatsV1 {
            hp: 150.5,
            level: 12,
            deprecated_tag: 4,
        },
        UnitStatsV1 {
            hp: 320.25,
            level: 99,
            deprecated_tag: 10,
        },
    ];
    let mut src_entities = Vec::new();
    for (idx, stats) in samples.iter().enumerate() {
        let e = save_app
            .world_mut()
            .spawn(Name::new(format!("hero_{idx}")))
            .id();
        write_entity_pod_bytes(save_app.world_mut(), e, v1_h, &stats_v1_bytes(stats)).unwrap();
        src_entities.push(e.to_bits());
    }

    // Save using Base64 POD encoding to exercise Base64 + saved field header decoding during migration.
    let opts = ModSaveOptions {
        format_preference: ModSaveFormatPreference::AutoPodBase64ThenJson,
        ..ModSaveOptions::default()
    };
    let archive = save_app.save_mod_archive_with(&opts).unwrap();
    assert_eq!(
        archive.component_headers[0].format,
        SavedComponentFormat::RawPodBase64
    );

    // 1. Automatic field-name-matched v1 -> v2 migration (hp moved offset 0 -> 8, level moved
    //    offset 4 -> 0 and widened u32 -> u64, deprecated_tag dropped, shield zero-initialized).
    let mut auto_app = App::new();
    auto_app.add_plugins((
        AbiModPlugin,
        ModSaveGamePlugin::with_policy(ModSaveLoadPolicy::MigrateOnLoad),
    ));
    let mod_v2 = ModId(51);
    spawn_active_mod_info(auto_app.world_mut(), mod_v2, "rpg_core", "2.0.0");
    let v2_fields = stats_v2_fields();
    let v2_h = register_dynamic_mod_component(
        auto_app.world_mut(),
        mod_v2,
        "rpg_core::UnitStats",
        c"rpg_core::UnitStats",
        core::mem::size_of::<UnitStatsV2>(),
        core::mem::align_of::<UnitStatsV2>(),
        0x2222_0002,
        &v2_fields,
    );

    let report = auto_app
        .load_mod_archive(&archive, ModSaveLoadPolicy::MigrateOnLoad)
        .unwrap();
    assert_eq!(report.restored_entities, 2);
    assert_eq!(report.restored_components, 2);
    assert_eq!(report.migrated_components, 2);
    assert_eq!(report.stripped_components, 0);
    assert_eq!(
        report.migrated_component_names,
        vec!["rpg_core::UnitStats".to_owned()]
    );

    for (idx, expected_v1) in samples.iter().enumerate() {
        let new_e = report.entity_map[&src_entities[idx]];
        assert_eq!(
            auto_app.world().get::<Name>(new_e).unwrap().as_str(),
            format!("hero_{idx}")
        );
        let upgraded = read_stats_v2(auto_app.world(), new_e, v2_h);
        assert_eq!(upgraded.level, u64::from(expected_v1.level));
        assert_eq!(upgraded.hp, expected_v1.hp);
        assert_eq!(upgraded.shield, 0.0);
    }

    // 2. Registered custom migrator computing `shield = deprecated_tag * 2.5` from v1 bytes.
    let mut custom_app = App::new();
    custom_app.add_plugins((AbiModPlugin, ModSaveGamePlugin::new()));
    spawn_active_mod_info(custom_app.world_mut(), ModId(52), "rpg_core", "2.0.0");
    let v2_h_custom = register_dynamic_mod_component(
        custom_app.world_mut(),
        ModId(52),
        "rpg_core::UnitStats",
        c"rpg_core::UnitStats",
        core::mem::size_of::<UnitStatsV2>(),
        core::mem::align_of::<UnitStatsV2>(),
        0x2222_0002,
        &v2_fields,
    );
    custom_app.register_save_migrator("rpg_core::UnitStats", |ctx, payload| {
        assert_eq!(ctx.saved_header.schema_fingerprint, 0x1111_0001);
        assert_eq!(ctx.active_schema_fingerprint, 0x2222_0002);
        let raw = payload.decode_raw_bytes().map_err(|e| e.to_string())?;
        let hp = f32::from_ne_bytes(raw[0..4].try_into().unwrap());
        let level = u32::from_ne_bytes(raw[4..8].try_into().unwrap());
        let deprecated_tag = u32::from_ne_bytes(raw[8..12].try_into().unwrap());
        let mut fields = serde_json::Map::new();
        fields.insert("level".to_owned(), serde_json::json!(u64::from(level) + 1));
        fields.insert("hp".to_owned(), serde_json::json!(hp));
        fields.insert(
            "shield".to_owned(),
            serde_json::json!((deprecated_tag as f32) * 2.5),
        );
        Ok(MigratedComponentPayload::JsonFields(fields))
    });

    let custom_report = custom_app
        .load_mod_archive(&archive, ModSaveLoadPolicy::MigrateOnLoad)
        .unwrap();
    assert_eq!(custom_report.migrated_components, 2);
    for (idx, expected_v1) in samples.iter().enumerate() {
        let new_e = custom_report.entity_map[&src_entities[idx]];
        let upgraded = read_stats_v2(custom_app.world(), new_e, v2_h_custom);
        assert_eq!(upgraded.level, u64::from(expected_v1.level) + 1);
        assert_eq!(upgraded.hp, expected_v1.hp);
        assert_eq!(upgraded.shield, (expected_v1.deprecated_tag as f32) * 2.5);
    }
}

#[test]
fn witness_non_pod_component_migration_and_file_roundtrip() {
    let mut v1_app = App::new();
    v1_app.add_plugins((AbiModPlugin, ModSaveGamePlugin::new()));
    v1_app
        .register_mod_component::<ProfileV1>(ComponentOptions::new().with_default())
        .unwrap();

    let e1 = v1_app
        .world_mut()
        .spawn(ProfileV1 {
            hp: 420.0,
            title: "Archmage".to_owned(),
            loot_ids: vec![7, 14, 21],
            guild_id: FfiOption::some(88),
            obsolete_flag: true,
        })
        .id();

    let archive = v1_app.save_mod_archive().unwrap();
    assert_eq!(archive.component_headers.len(), 1);
    assert_eq!(
        archive.component_headers[0].format,
        SavedComponentFormat::WitnessFieldsJson
    );

    // Test JSON & binary file persistence helpers cleanly.
    let json_path = temp_save_path("witness", "json");
    let bin_path = temp_save_path("witness", "bamsave");
    archive.write_json_file(&json_path).unwrap();
    archive.write_binary_file(&bin_path).unwrap();
    let loaded_json = ModSaveArchive::read_json_file(&json_path).unwrap();
    let loaded_bin = ModSaveArchive::read_binary_file(&bin_path).unwrap();
    let _ = std::fs::remove_file(&json_path);
    let _ = std::fs::remove_file(&bin_path);
    assert_eq!(loaded_json, archive);
    assert_eq!(loaded_bin, archive);

    // Load into World 2 with evolved ProfileV2 (removed `obsolete_flag`, added `prestige_rank`
    // defaulting to `77`).
    let mut v2_app = App::new();
    v2_app.add_plugins((AbiModPlugin, ModSaveGamePlugin::new()));
    v2_app
        .register_mod_component::<ProfileV2>(ComponentOptions::new().with_default())
        .unwrap();

    let report = v2_app
        .load_mod_archive(&loaded_bin, ModSaveLoadPolicy::MigrateOnLoad)
        .unwrap();
    assert_eq!(report.restored_entities, 1);
    assert_eq!(report.migrated_components, 1);
    let migrated_e = report.entity_map[&e1.to_bits()];
    let profile = v2_app.world().get::<ProfileV2>(migrated_e).unwrap();
    assert_eq!(profile.hp, 420.0);
    assert_eq!(profile.title, "Archmage");
    assert_eq!(profile.loot_ids, vec![7, 14, 21]);
    assert_eq!(profile.guild_id, FfiOption::some(88));
    assert_eq!(profile.prestige_rank, 77);
}
