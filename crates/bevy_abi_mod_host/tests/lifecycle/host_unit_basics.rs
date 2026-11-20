//! Core host plugin, registration, `summary_tick` validation, quarantine, deferred command, and
//! `Disabled` `LoadedModInfo` `despawn_all_where` cleanup tests.

use crate::DROP_SLOT_POOL_RWLOCK;
use crate::support::{TempDylibCopy, fixture_dylib};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::GuestSystem;
use bevy_abi_mod_sys::{
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET,
    BAM_STORAGE_FLAG_SUMMARY_TICK, BamSystemCtx,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::message::Messages;
use bevy_ecs::prelude::Resource;
use bevy_ecs::query::Allow;
use bevy_ecs::schedule::ScheduleLabel;
use bevy_ecs::system::Commands;
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "host_unit::Health")]
struct Health {
    hp: f32,
    max_hp: f32,
}

#[derive(Component, Default, Debug, ModComponent)]
#[mod_component(name = "host_unit::UnitTag")]
struct UnitTag {
    label: String,
    active: bool,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Default, Debug, ModComponent)]
#[mod_component(name = "host_unit::Gravity")]
struct Gravity {
    g: f32,
}

#[derive(ScheduleLabel, Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SimulationStep;

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[component(summary_tick)]
#[mod_component(name = "host_unit::SummaryTableComp")]
struct SummaryTableComp {
    val: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[component(summary_tick)]
#[mod_component(name = "host_unit::SummaryTableExplicit")]
struct SummaryTableExplicit {
    val: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[component(summary_tick)]
#[mod_component(name = "host_unit::SummaryBothAttrs", summary_tick)]
struct SummaryBothAttrs {
    val: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "host_unit::PlainTableComp")]
struct PlainTableComp {
    val: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "host_unit::PlainAttrMismatch", summary_tick)]
struct PlainAttrMismatch {
    val: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "host_unit::SparseComp")]
struct SparseComp {
    val: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[component(summary_tick)]
#[mod_component(name = "host_unit::SummaryRejectedSparse")]
struct SummaryRejectedSparse {
    val: u32,
}

#[test]
fn world_and_app_ext_register_components_resources_and_custom_schedules() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let health_h = app
        .register_mod_component::<Health>(ComponentOptions::new().with_raw_insert().with_default())
        .unwrap();
    let tag_h = app
        .register_mod_component::<UnitTag>(
            ComponentOptions::new()
                .with_default()
                .immutable()
                .sparse_set(),
        )
        .unwrap();
    let grav_h = app.register_mod_resource::<Gravity>().unwrap();

    // Handles `1` (`ChildOf`) and `2` (`Children`) are reserved by `AbiModPlugin`.
    assert_eq!(health_h, BamComponent::from_raw(3));
    assert_eq!(tag_h, BamComponent::from_raw(4));
    assert_eq!(grav_h, BamComponent::from_raw(5));

    let reg = app.world().resource::<ModRegistry>();
    assert_eq!(
        reg.get_by_name(bevy_abi_mod_sys::BAM_CHILD_OF_NAME)
            .map(|c| c.handle),
        Some(BamComponent::from_raw(1))
    );
    assert_eq!(
        reg.get_by_name(bevy_abi_mod_sys::BAM_CHILDREN_NAME)
            .map(|c| c.handle),
        Some(BamComponent::from_raw(2))
    );
    let health_meta = reg.get_by_handle(health_h).unwrap();
    assert!(health_meta.insert_raw_fn.is_some());
    assert!(health_meta.insert_default_fn.is_some());

    let tag_meta = reg.get_by_handle(tag_h).unwrap();
    assert!(tag_meta.insert_raw_fn.is_none());
    assert!(tag_meta.insert_default_fn.is_some());
    assert_eq!(
        tag_meta.storage_flags & (BAM_STORAGE_FLAG_IMMUTABLE | BAM_STORAGE_FLAG_SPARSE_SET),
        BAM_STORAGE_FLAG_IMMUTABLE | BAM_STORAGE_FLAG_SPARSE_SET
    );

    let grav_meta = reg.get_by_handle(grav_h).unwrap();
    assert_eq!(
        grav_meta.storage_flags & BAM_STORAGE_FLAG_RESOURCE,
        BAM_STORAGE_FLAG_RESOURCE
    );

    // Built-in schedule IDs (`< 1024`) rejected by `register_mod_schedule`; custom `>= 1024` accepted.
    assert!(matches!(
        app.register_mod_schedule(ScheduleId::UPDATE, SimulationStep),
        Err(ModLoadError::InvalidScheduleRegistration { .. })
    ));
    let custom_id = ScheduleId::custom(1024);
    app.register_mod_schedule(custom_id, SimulationStep)
        .unwrap();
    assert!(app.world().resource::<ModScheduleMap>().contains(custom_id));
}

#[test]
fn quarantine_reenable_and_single_message_emission() {
    static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn faulting_once_cb(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        let prev = CALL_COUNT.fetch_add(1, Ordering::SeqCst);
        if prev == 0 {
            FfiResult::INTERNAL_ERROR
        } else {
            FfiResult::OK
        }
    }

    CALL_COUNT.store(0, Ordering::SeqCst);
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let mod_id = ModId(42);
    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "faulting_once",
        Vec::new(),
        faulting_once_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_mod_id(mod_id);

    app.world_mut()
        .resource_mut::<ModQuarantine>()
        .register_system(mod_id, sys.system_name_arc(), sys.enabled_flag());
    app.add_systems(Update, sys);

    // Frame 1: runs and faults -> quarantined; emits exactly ONE `ModSystemFaulted` message.
    app.update();
    assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 1);
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(mod_id, "faulting_once")
    );
    assert_eq!(
        app.world().resource::<Messages<ModSystemFaulted>>().len(),
        1
    );

    // Frame 2: system remains quarantined -> callback not called.
    app.update();
    assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 1);

    // Re-enable via `ModQuarantine::reenable` -> Frame 3 executes cleanly!
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .reenable(mod_id, "faulting_once")
    );
    app.update();
    assert_eq!(CALL_COUNT.load(Ordering::SeqCst), 2);
    assert!(
        !app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(mod_id, "faulting_once")
    );
}

#[test]
fn deferred_mod_commands_ext_executes_cleanly_on_flush() {
    fn trigger_deferred_ops(mut commands: Commands) {
        commands.unload_mod(ModId(999_999));
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.add_systems(Update, trigger_deferred_ops);
    app.update();
    assert!(app.world().resource::<ModHost>().is_empty());
}

#[test]
fn register_mod_component_validates_summary_tick_against_bevy_component_info() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // Valid 1: Bevy `#[component(summary_tick)]` inherits `BAM_STORAGE_FLAG_SUMMARY_TICK` automatically.
    let h_inferred = app
        .register_mod_component::<SummaryTableComp>(ComponentOptions::new())
        .expect("summary_tick Table component must register cleanly");
    // Valid 2: Explicit `.summary_tick()` on a `#[component(summary_tick)]` Table component succeeds.
    let h_explicit = app
        .register_mod_component::<SummaryTableExplicit>(ComponentOptions::new().summary_tick())
        .expect("explicit summary_tick on #[component(summary_tick)] must succeed");
    // Valid 3: Matching `#[component(summary_tick)]` + `#[mod_component(..., summary_tick)]` succeeds.
    let h_both = app
        .register_mod_component::<SummaryBothAttrs>(ComponentOptions::new())
        .expect("matching Bevy + mod_component summary_tick attrs must succeed");

    let reg = app.world().resource::<ModRegistry>();
    for handle in [h_inferred, h_explicit, h_both] {
        let meta = reg.get_by_handle(handle).expect("registered metadata");
        assert_ne!(
            meta.storage_flags & BAM_STORAGE_FLAG_SUMMARY_TICK,
            0,
            "expected BAM_STORAGE_FLAG_SUMMARY_TICK on handle {handle:?}"
        );
    }

    // Mismatch 1: Plain Table component without `#[component(summary_tick)]` + `.summary_tick()`.
    let err_plain =
        app.register_mod_component::<PlainTableComp>(ComponentOptions::new().summary_tick());
    assert!(matches!(
        err_plain,
        Err(ComponentCollisionError::MetadataMismatch { ref name, .. })
            if name == "host_unit::PlainTableComp"
    ));

    // Mismatch 2: Plain Table component with `#[mod_component(..., summary_tick)]` but missing Bevy `#[component(summary_tick)]`.
    let err_attr = app.register_mod_component::<PlainAttrMismatch>(ComponentOptions::new());
    assert!(matches!(
        err_attr,
        Err(ComponentCollisionError::MetadataMismatch { ref name, .. })
            if name == "host_unit::PlainAttrMismatch"
    ));

    // Mismatch 3: `SparseSet` component with `.summary_tick()`.
    let err_sparse =
        app.register_mod_component::<SparseComp>(ComponentOptions::new().summary_tick());
    assert!(matches!(
        err_sparse,
        Err(ComponentCollisionError::MetadataMismatch { ref name, .. })
            if name == "host_unit::SparseComp"
    ));

    // Mismatch 4: `#[component(summary_tick)]` Table component combined with `.sparse_set()`.
    let err_conflict =
        app.register_mod_component::<SummaryRejectedSparse>(ComponentOptions::new().sparse_set());
    assert!(matches!(
        err_conflict,
        Err(ComponentCollisionError::MetadataMismatch { ref name, .. })
            if name == "host_unit::SummaryRejectedSparse"
    ));

    // Rejected components must NOT be registered in `ModRegistry`.
    let reg = app.world().resource::<ModRegistry>();
    for rejected in [
        "host_unit::PlainTableComp",
        "host_unit::PlainAttrMismatch",
        "host_unit::SparseComp",
        "host_unit::SummaryRejectedSparse",
    ] {
        assert!(
            reg.get_by_name(rejected).is_none(),
            "rejected component '{rejected}' must not enter ModRegistry"
        );
    }
}

#[test]
fn disabled_loaded_mod_info_entities_despawn_on_unload_and_unload_all() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "disabled_loaded_info");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let mod_a = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_a");
    let mod_b = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_b");

    let (ent_a, ent_b) = {
        let world = app.world_mut();
        let mut q = world.query::<(Entity, &LoadedModInfo)>();
        let mut ent_a = None;
        let mut ent_b = None;
        for (entity, info) in q.iter(world) {
            if info.id == mod_a {
                ent_a = Some(entity);
            } else if info.id == mod_b {
                ent_b = Some(entity);
            }
        }
        (ent_a.expect("mod_a entity"), ent_b.expect("mod_b entity"))
    };

    // Disable BOTH `LoadedModInfo` entities; manual queries without `Allow<Disabled>` would miss them.
    app.world_mut().entity_mut(ent_a).insert(Disabled);
    app.world_mut().entity_mut(ent_b).insert(Disabled);

    // Unload mod_a: its `Disabled` `LoadedModInfo` entity MUST be despawned while mod_b's survives.
    app.unload_mod(mod_a).expect("unload mod_a");
    assert!(!app.world().entities().contains(ent_a));
    assert!(app.world().entities().contains(ent_b));
    {
        let world = app.world_mut();
        let mut q = world.query_filtered::<&LoadedModInfo, Allow<Disabled>>();
        let remaining: Vec<ModId> = q.iter(world).map(|info| info.id).collect();
        assert_eq!(remaining, vec![mod_b]);
    }

    // `unload_all_mods` MUST also despawn remaining `Disabled` `LoadedModInfo` entities.
    app.unload_all_mods();
    assert!(!app.world().entities().contains(ent_b));
    let world = app.world_mut();
    let mut q = world.query_filtered::<&LoadedModInfo, Allow<Disabled>>();
    assert_eq!(q.iter(world).count(), 0);
}
