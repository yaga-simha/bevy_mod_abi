//! Integration tests for `bridge-reflect` (`bevy_reflect`, `bevy_remote`, `bevy_settings`).

#![forbid(unsafe_code)]

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_reflect::reflect::{
    BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
    BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
    ModReflectAppExt, ModReflectError, ModReflectPlugin, ModReflectWorldExt,
};
use bevy_app::App;
use bevy_ecs::prelude::{Component, Resource};
use bevy_ecs::reflect::{ReflectComponent, ReflectResource};
use bevy_reflect::prelude::ReflectDefault;
use bevy_reflect::structs::Struct;
use bevy_reflect::{PartialReflect, Reflect, ReflectCloneError};
use bevy_remote::{BrpRequest, RemoteMethodSystemId, RemoteMethods, RemotePlugin};
use bevy_settings::{ReflectSettingsGroup, SettingsGroup, SettingsPlugin};
use core::sync::atomic::{AtomicUsize, Ordering};
use std::fs;
use std::path::PathBuf;

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

fn temp_toml_path(tag: &str) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "bam_reflect_settings_{tag}_{}_{}.toml",
        std::process::id(),
        seq
    ))
}

#[derive(Component, Clone, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "reflect_test::NpcProfile")]
struct NpcProfile {
    hp: f32,
    level: u32,
    title: String,
    hostile: bool,
    loot_ids: Vec<u32>,
    guild_id: FfiOption<u32>,
}

#[derive(Component, Clone, Default, Debug, PartialEq, Reflect, ModComponent)]
#[mod_component(name = "reflect_test::CombatStats")]
#[reflect(Component, Default)]
struct CombatStats {
    power: f32,
    armor: i32,
}

#[derive(Component, Default, Debug, Reflect, ModComponent)]
#[mod_component(name = "reflect_test::CustomReflected", opaque)]
#[reflect(Component, Default)]
struct CustomReflected {
    #[reflect(ignore)]
    hidden_tag: u32,
    tag: u32,
}

#[derive(Resource, Default, Debug, PartialEq, Reflect, ModComponent)]
#[mod_component(name = "reflect_test::WorldTuning")]
struct WorldTuning {
    gravity: f32,
    motd: String,
    hardcore: bool,
}

#[derive(Resource, SettingsGroup, Reflect, Default, Debug, PartialEq, ModComponent)]
#[reflect(Resource, SettingsGroup, Default)]
#[mod_component(name = "reflect_test::AudioModSettings")]
#[settings_group(group = "audio_mod_settings")]
struct AudioModSettings {
    volume: f32,
    muted: bool,
}

#[test]
fn witness_dynamic_struct_and_fallible_to_dynamic_reflect_roundtrip() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModReflectPlugin));

    let npc_h = app
        .register_mod_component::<NpcProfile>(ComponentOptions::new().with_default())
        .unwrap();
    let stats_h = app
        .register_reflected_component::<CombatStats>(ComponentOptions::new().with_default())
        .unwrap();
    let opaque_h = app
        .register_reflected_component::<CustomReflected>(ComponentOptions::new().with_default())
        .unwrap();

    let world = app.world_mut();
    let schema = world.register_witness_reflection(npc_h).unwrap();
    assert_eq!(schema.canonical_name, "reflect_test::NpcProfile");
    assert_eq!(schema.fields.len(), 6);

    let e = world
        .spawn((
            NpcProfile {
                hp: 120.0,
                level: 7,
                title: "Captain".to_owned(),
                hostile: false,
                loot_ids: vec![10, 20, 30],
                guild_id: FfiOption::some(42),
            },
            CombatStats {
                power: 55.5,
                armor: 18,
            },
            CustomReflected {
                hidden_tag: 7,
                tag: 99,
            },
        ))
        .id();

    // 1. Witness-synthesized DynamicStruct export -> inspect -> mutate -> apply back.
    let mut dyn_npc = world.export_witness_dynamic_struct(e, npc_h).unwrap();
    assert_eq!(
        *dyn_npc
            .field("hp")
            .unwrap()
            .try_downcast_ref::<f32>()
            .unwrap(),
        120.0
    );
    assert_eq!(
        dyn_npc
            .field("title")
            .unwrap()
            .try_downcast_ref::<String>()
            .unwrap(),
        "Captain"
    );
    assert_eq!(
        dyn_npc
            .field("loot_ids")
            .unwrap()
            .try_downcast_ref::<Vec<u32>>()
            .unwrap(),
        &[10, 20, 30]
    );
    assert_eq!(
        *dyn_npc
            .field("guild_id")
            .unwrap()
            .try_downcast_ref::<Option<u32>>()
            .unwrap(),
        Some(42)
    );

    dyn_npc.insert("hp", 250.0f32);
    dyn_npc.insert("title", "Warlord".to_owned());
    dyn_npc.insert("hostile", true);
    dyn_npc.insert("guild_id", None::<u32>);
    world
        .apply_witness_dynamic_struct(e, npc_h, &dyn_npc)
        .unwrap();

    let updated = world.get::<NpcProfile>(e).unwrap();
    assert_eq!(updated.hp, 250.0);
    assert_eq!(updated.title, "Warlord");
    assert!(updated.hostile);
    assert!(updated.guild_id.is_none());

    // 2. Typed Reflect component via ReflectFromPtr + fallible PartialReflect::to_dynamic().
    let stats_dyn = world.clone_mod_component_dynamic(e, stats_h).unwrap();
    assert!(stats_dyn.is_dynamic());
    assert_eq!(
        *stats_dyn
            .reflect_ref()
            .as_struct()
            .unwrap()
            .field("power")
            .unwrap()
            .try_downcast_ref::<f32>()
            .unwrap(),
        55.5
    );

    // UncloneableOpaque derives Reflect with `#[reflect(ignore)]` (so `reflect_clone` works on
    // reflected fields, wait: check if `UncloneableOpaque` has a `fn()` reflected field or verify
    // `PartialReflect::to_dynamic` Result type directly!).
    let opaque_clone = world.clone_mod_component_dynamic(e, opaque_h);
    assert!(opaque_clone.is_ok());

    // `#[reflect(ignore)]` without `#[reflect(clone)]` returns `ReflectCloneError::FieldNotCloneable`,
    // and an opaque `PartialReflect` returning `NotCloneable` propagates through `to_dynamic()`.
    let custom_ref = world.reflect_mod_component(e, opaque_h).unwrap();
    assert!(matches!(
        custom_ref.reflect_clone(),
        Err(ReflectCloneError::FieldNotCloneable { .. })
    ));

    #[derive(bevy_reflect::TypePath)]
    struct NonCloneableOpaque;

    impl PartialReflect for NonCloneableOpaque {
        fn get_represented_type_info(&self) -> Option<&'static bevy_reflect::TypeInfo> {
            None
        }
        fn into_partial_reflect(self: Box<Self>) -> Box<dyn PartialReflect> {
            self
        }
        fn as_partial_reflect(&self) -> &dyn PartialReflect {
            self
        }
        fn as_partial_reflect_mut(&mut self) -> &mut dyn PartialReflect {
            self
        }
        fn try_into_reflect(self: Box<Self>) -> Result<Box<dyn Reflect>, Box<dyn PartialReflect>> {
            Err(self)
        }
        fn try_as_reflect(&self) -> Option<&dyn Reflect> {
            None
        }
        fn try_as_reflect_mut(&mut self) -> Option<&mut dyn Reflect> {
            None
        }
        fn try_apply(
            &mut self,
            _value: &dyn PartialReflect,
        ) -> Result<(), bevy_reflect::ApplyError> {
            Ok(())
        }
        fn reflect_ref(&self) -> bevy_reflect::ReflectRef<'_> {
            bevy_reflect::ReflectRef::Opaque(self)
        }
        fn reflect_mut(&mut self) -> bevy_reflect::ReflectMut<'_> {
            bevy_reflect::ReflectMut::Opaque(self)
        }
        fn reflect_owned(self: Box<Self>) -> bevy_reflect::ReflectOwned {
            bevy_reflect::ReflectOwned::Opaque(self)
        }
        fn reflect_clone(&self) -> Result<Box<dyn Reflect>, ReflectCloneError> {
            Err(ReflectCloneError::NotCloneable {
                type_path: "NonCloneableOpaque".into(),
            })
        }
    }

    let err: Result<Box<dyn PartialReflect>, ReflectCloneError> = NonCloneableOpaque.to_dynamic();
    let wrapped = ModReflectError::from(err.unwrap_err());
    assert!(matches!(
        wrapped,
        ModReflectError::Clone(ReflectCloneError::NotCloneable { .. })
    ));
}

#[test]
fn bevy_remote_brp_lists_inspects_mutates_and_inserts_mod_components_and_resources() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, RemotePlugin::default(), ModReflectPlugin));

    let npc_h = app
        .register_mod_component::<NpcProfile>(ComponentOptions::new().with_default())
        .unwrap();
    let _stats_h = app
        .register_reflected_component::<CombatStats>(ComponentOptions::new().with_default())
        .unwrap();
    let _tuning_h = app.register_mod_resource::<WorldTuning>().unwrap();
    app.insert_resource(WorldTuning {
        gravity: 9.81,
        motd: "Welcome".to_owned(),
        hardcore: false,
    });

    let entity = app
        .world_mut()
        .spawn((
            NpcProfile {
                hp: 80.0,
                level: 3,
                title: "Scout".to_owned(),
                hostile: false,
                loot_ids: vec![1, 2],
                guild_id: FfiOption::some(9),
            },
            CombatStats {
                power: 12.0,
                armor: 5,
            },
        ))
        .id();
    let bare_entity = app.world_mut().spawn_empty().id();
    app.finish();
    app.cleanup();

    // 1. Live `RemotePlugin` `RemoteMethods` invocation via `world.run_system_with`.
    let RemoteMethodSystemId::Instant(get_sys_id) = *app
        .world()
        .resource::<RemoteMethods>()
        .get(BRP_METHOD_GET_COMPONENTS)
        .expect("BRP_METHOD_GET_COMPONENTS installed by ModReflectPlugin::finish")
    else {
        panic!("expected Instant RemoteMethodSystemId");
    };
    let get_res = app
        .world_mut()
        .run_system_with(
            get_sys_id,
            Some(serde_json::json!({
                "entity": entity.to_bits(),
                "components": ["reflect_test::NpcProfile", "reflect_test::CombatStats"],
            })),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        get_res["components"]["reflect_test::NpcProfile"]["hp"],
        80.0
    );
    assert_eq!(
        get_res["components"]["reflect_test::NpcProfile"]["title"],
        "Scout"
    );
    assert_eq!(
        get_res["components"]["reflect_test::CombatStats"]["power"],
        12.0
    );

    // 2. `list_components` (per-entity) includes witness field descriptors.
    let list_res = app
        .world_mut()
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_LIST_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(1)),
            params: Some(serde_json::json!({ "entity": entity.to_bits() })),
        })
        .unwrap();
    let list_arr = list_res.as_array().unwrap();
    assert!(list_arr.iter().any(|item| {
        item["name"] == "reflect_test::NpcProfile" && !item["fields"].as_array().unwrap().is_empty()
    }));

    // 3. `mutate_components` mutates both witness component and typed Reflect component.
    app.world_mut()
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_MUTATE_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(2)),
            params: Some(serde_json::json!({
                "entity": entity.to_bits(),
                "components": {
                    "reflect_test::NpcProfile": {
                        "hp": 999.0,
                        "title": "Boss",
                        "hostile": true,
                        "loot_ids": [77, 88],
                    },
                    "reflect_test::CombatStats": {
                        "power": 77.5,
                        "armor": 30,
                    }
                }
            })),
        })
        .unwrap();

    let npc = app.world().get::<NpcProfile>(entity).unwrap();
    assert_eq!(npc.hp, 999.0);
    assert_eq!(npc.title, "Boss");
    assert!(npc.hostile);
    assert_eq!(npc.loot_ids, vec![77, 88]);

    let stats = app.world().get::<CombatStats>(entity).unwrap();
    assert_eq!(stats.power, 77.5);
    assert_eq!(stats.armor, 30);

    // 4. `insert_components` inserts witness-backed component onto `bare_entity`.
    app.world_mut()
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_INSERT_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(3)),
            params: Some(serde_json::json!({
                "entity": bare_entity.to_bits(),
                "components": {
                    "reflect_test::NpcProfile": {
                        "hp": 45.0,
                        "title": "Rookie",
                    }
                }
            })),
        })
        .unwrap();
    let bare_npc = app.world().get::<NpcProfile>(bare_entity).unwrap();
    assert_eq!(bare_npc.hp, 45.0);
    assert_eq!(bare_npc.title, "Rookie");
    let _ = npc_h;

    // 5. `get_resources` & `mutate_resources` inspect & mutate `WorldTuning`.
    let res_get = app
        .world_mut()
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_GET_RESOURCES.to_owned(),
            id: Some(serde_json::json!(4)),
            params: Some(serde_json::json!({ "resources": ["reflect_test::WorldTuning"] })),
        })
        .unwrap();
    assert_eq!(res_get["reflect_test::WorldTuning"]["motd"], "Welcome");

    app.world_mut()
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_MUTATE_RESOURCES.to_owned(),
            id: Some(serde_json::json!(5)),
            params: Some(serde_json::json!({
                "resource": "reflect_test::WorldTuning",
                "value": { "gravity": 3.71, "motd": "Mars", "hardcore": true }
            })),
        })
        .unwrap();
    let tuning = app.world().resource::<WorldTuning>();
    assert_eq!(tuning.motd, "Mars");
    assert!(tuning.hardcore);
    assert!((tuning.gravity - 3.71).abs() < 1e-4);
}

#[test]
fn bevy_settings_toml_roundtrip_synchronizes_settings_group_and_witness_resources() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModReflectPlugin));

    let _audio_h = app.expose_mod_settings_group::<AudioModSettings>().unwrap();
    let tuning_h = app.register_mod_resource::<WorldTuning>().unwrap();
    app.insert_resource(WorldTuning {
        gravity: 12.5,
        motd: "SavedWorld".to_owned(),
        hardcore: true,
    });
    app.register_mod_settings_resource(tuning_h, "world_tuning")
        .unwrap();

    // Verify `SettingsPlugin` recognizes our exposed `SettingsGroup` cleanly without panicking.
    app.add_plugins(SettingsPlugin::new("bevy_abi_mod_bridge_reflect_test"));

    {
        let mut audio = app.world_mut().resource_mut::<AudioModSettings>();
        audio.volume = 0.65;
        audio.muted = true;
    }

    let path = temp_toml_path("roundtrip");
    let _ = fs::remove_file(&path);
    app.world().save_mod_settings_file(&path).unwrap();
    let toml_text = fs::read_to_string(&path).unwrap();
    assert!(toml_text.contains("[audio_mod_settings]"));
    assert!(toml_text.contains("[world_tuning]"));

    // Reset live state, then load back from TOML file.
    *app.world_mut().resource_mut::<AudioModSettings>() = AudioModSettings::default();
    *app.world_mut().resource_mut::<WorldTuning>() = WorldTuning::default();

    let applied = app.world_mut().load_mod_settings_file(&path).unwrap();
    assert_eq!(applied, 2);

    let audio_after = app.world().resource::<AudioModSettings>();
    assert!((audio_after.volume - 0.65).abs() < 1e-4);
    assert!(audio_after.muted);

    let tuning_after = app.world().resource::<WorldTuning>();
    assert!((tuning_after.gravity - 12.5).abs() < 1e-4);
    assert_eq!(tuning_after.motd, "SavedWorld");
    assert!(tuning_after.hardcore);

    let _ = fs::remove_file(&path);
}

#[derive(bevy_asset::Asset, Reflect, Clone, Debug, PartialEq)]
struct BrpIconAsset {
    name: String,
}

#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct OpaqueLoadout {
    label: String,
    tier: u32,
    icon: bevy_asset::Handle<BrpIconAsset>,
}

impl bevy_abi_mod_sys::ModComponent for OpaqueLoadout {
    const CANONICAL_NAME: &'static str = "reflect_test::OpaqueLoadout";
    const COMPONENT_CSTR: &'static core::ffi::CStr = c"reflect_test::OpaqueLoadout";
    const UUID: bevy_abi_mod_sys::ComponentUuid = bevy_abi_mod_sys::ComponentUuid::from_namespaced(
        "reflect_test::OpaqueLoadout",
        core::mem::size_of::<Self>(),
        core::mem::align_of::<Self>(),
        0,
    );
    type VTable = ();
    fn vtable() -> &'static Self::VTable {
        &()
    }
    fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
        &[]
    }
}

#[test]
fn brp_opaque_reflected_component_and_asset_handle_roundtrip() {
    use bevy_asset::io::AssetSourceBuilder;
    use bevy_asset::io::memory::{Dir, MemoryAssetReader};
    use bevy_asset::{AssetApp, AssetMetaCheck, AssetPath, AssetPlugin, AssetServer, Handle};

    let mod_dir = Dir::default();
    let mut app = App::new();
    app.register_asset_source(
        "mod",
        AssetSourceBuilder::new(move || {
            Box::new(MemoryAssetReader {
                root: mod_dir.clone(),
            })
        }),
    );
    app.add_plugins((
        bevy_app::TaskPoolPlugin::default(),
        AssetPlugin {
            meta_check: AssetMetaCheck::Never,
            ..Default::default()
        },
        AbiModPlugin,
        RemotePlugin::default(),
        ModReflectPlugin,
    ));
    app.init_asset::<BrpIconAsset>()
        .register_asset_reflect::<BrpIconAsset>();
    app.register_reflected_component::<OpaqueLoadout>(ComponentOptions::new().with_default())
        .unwrap();

    let knight_icon: Handle<BrpIconAsset> = app
        .world()
        .resource::<AssetServer>()
        .load("mod://ui/knight.icon");
    let entity = app
        .world_mut()
        .spawn(OpaqueLoadout {
            label: "knight".to_owned(),
            tier: 2,
            icon: knight_icon,
        })
        .id();
    app.finish();
    app.cleanup();

    // 1. `get_components` on an opaque reflected component must emit unwrapped JSON with
    //    `HandleSerializeProcessor` path encoding (matching `TypedReflectDeserializer`).
    let get_res = app
        .world_mut()
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_GET_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(10)),
            params: Some(serde_json::json!({
                "entity": entity.to_bits(),
                "components": ["reflect_test::OpaqueLoadout"],
            })),
        })
        .unwrap();
    let mut loadout_json = get_res["components"]["reflect_test::OpaqueLoadout"].clone();
    assert_eq!(loadout_json["label"], "knight");
    assert_eq!(loadout_json["tier"], 2);
    assert_eq!(loadout_json["icon"]["Path"], "mod://ui/knight.icon");

    // 2. Mutate the returned JSON in-place and feed it straight back into `mutate_components`.
    loadout_json["label"] = serde_json::json!("paladin");
    loadout_json["tier"] = serde_json::json!(5);
    loadout_json["icon"] = serde_json::json!({ "Path": "mod://ui/paladin.icon" });

    app.world_mut()
        .dispatch_mod_brp_request(BrpRequest {
            method: BRP_METHOD_MUTATE_COMPONENTS.to_owned(),
            id: Some(serde_json::json!(11)),
            params: Some(serde_json::json!({
                "entity": entity.to_bits(),
                "component": "reflect_test::OpaqueLoadout",
                "value": loadout_json,
            })),
        })
        .unwrap();

    let updated = app.world().get::<OpaqueLoadout>(entity).unwrap();
    assert_eq!(updated.label, "paladin");
    assert_eq!(updated.tier, 5);
    assert_eq!(
        updated.icon.path(),
        Some(&AssetPath::parse("mod://ui/paladin.icon"))
    );
}
