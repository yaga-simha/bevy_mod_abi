//! Lifecycle, State Serialization, Component Hooks, Dynamic Layouts & Observers Tests (`ABI 0.2.0`):
//! - `reload_mod` / `reload_mod_with` / `ModCommandsExt::reload_mod` preserving guest
//!   state via `BamManifest::save_state` and `BamManifest::restore_state`, propagating
//!   `StateSaveFailed` / `StateRestoreFailed`, and remaining backwards-compatible with
//!   `MIN_V0_2_SIZE` (`32`-byte) manifests that omit state callbacks.
//! - Dynamic component `on_add` and `on_remove` hooks firing on spawn/insert/remove/despawn,
//!   disarming cleanly via `ArcSwapOption` on `unload_mod` (`dlclose` safety when entities outlive
//!   the mod), and rolling back cleanly on failed `init`.

use crate::support::{TempDylibCopy, fixture_dylib};
use bevy_abi_mod_host::prelude::*;
use bevy_app::{App, FixedUpdate, PostUpdate, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::event::{EntityEvent, Event};
use bevy_ecs::message::Messages;
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::system::Commands;
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ext::ReloadStateProbe")]
struct ReloadStateProbe {
    reload_epoch: u64,
    accumulated_ticks: u64,
    fail_next_save: u32,
    fail_next_restore: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, AbiPod)]
struct HookedAuraMirror {
    strength: u32,
    tag: u32,
}

#[test]
fn hot_reload_transfers_state_handles_errors_and_supports_legacy_manifests() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let ext_dylib = TempDylibCopy::new(
        &fixture_dylib("state_hooks_and_observers_mod"),
        "hot_reload_state",
    );
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_schedule(FixedUpdate);
    app.init_schedule(PostUpdate);
    app.insert_resource(ReloadStateProbe::default());
    app.register_mod_resource::<ReloadStateProbe>().unwrap();

    let opts = LoadOptions::new().with_shadow_copy(true).strict();
    let mod_v1 = app
        .load_mod_with(ext_dylib.path(), opts.clone())
        .expect("initial load of state_hooks_and_observers_mod");

    // Run 3 frames: `reload_epoch == 0`, `accumulated_ticks == 3`.
    app.update();
    app.update();
    app.update();
    assert_eq!(
        *app.world().resource::<ReloadStateProbe>(),
        ReloadStateProbe {
            reload_epoch: 0,
            accumulated_ticks: 3,
            fail_next_save: 0,
            fail_next_restore: 0,
        }
    );

    // 1. Hot-reload via `app.reload_mod_with`: snapshots `(0, 3)`, shuts down v1, loads v2, restores `(1, 3)`!
    let mod_v2 = app
        .reload_mod_with(mod_v1, opts.clone())
        .expect("reload_mod_with must preserve state");
    assert_ne!(mod_v1, mod_v2);
    app.update();
    assert_eq!(
        *app.world().resource::<ReloadStateProbe>(),
        ReloadStateProbe {
            reload_epoch: 1,
            accumulated_ticks: 4,
            fail_next_save: 0,
            fail_next_restore: 0,
        }
    );

    // 2. Deferred `Commands::reload_mod` inside a Bevy system also preserves state (`epoch -> 2`)!
    let trigger_deferred = std::sync::Arc::new(AtomicUsize::new(1));
    app.add_systems(Update, move |mut commands: Commands<'_, '_>| {
        if trigger_deferred.swap(0, Ordering::SeqCst) == 1 {
            commands.reload_mod(mod_v2);
        }
    });
    app.update(); // Runs `ext_state_tick` (ticks=5), then applies deferred `reload_mod` -> epoch=2!
    app.update(); // Runs reloaded instance -> epoch=2, ticks=6!
    assert_eq!(app.world().resource::<ReloadStateProbe>().reload_epoch, 2);
    assert_eq!(
        app.world().resource::<ReloadStateProbe>().accumulated_ticks,
        6
    );

    // Find current active ModId after deferred reload
    let active_id = app
        .world()
        .resource::<ModHost>()
        .loaded_mods()
        .first()
        .expect("reloaded mod active")
        .id();

    // 3. `save_state` failure aborts reload BEFORE unloading the running mod (`StateSaveFailed`)!
    app.world_mut()
        .resource_mut::<ReloadStateProbe>()
        .fail_next_save = 1;
    app.update(); // Syncs `fail_next_save = 1` into guest instance (`ticks = 7`)
    let save_err = app
        .reload_mod_with(active_id, opts.clone())
        .expect_err("failing save_state must return StateSaveFailed");
    assert!(
        matches!(
            save_err,
            ModLoadError::StateSaveFailed {
                status: FfiResult::INVALID_STATE,
                ..
            }
        ),
        "unexpected error: {save_err:?}"
    );
    // Original mod is STILL loaded and running!
    assert!(
        app.world()
            .resource::<ModHost>()
            .get_mod(active_id)
            .is_some()
    );
    app.world_mut()
        .resource_mut::<ReloadStateProbe>()
        .fail_next_save = 0;

    // 4. `restore_state` failure rolls back the replacement mod cleanly (`StateRestoreFailed`)!
    app.world_mut()
        .resource_mut::<ReloadStateProbe>()
        .fail_next_restore = 1;
    app.update(); // Syncs `fail_next_restore = 1` into guest instance
    let restore_err = app
        .reload_mod_with(active_id, opts.clone())
        .expect_err("failing restore_state must return StateRestoreFailed");
    assert!(
        matches!(
            restore_err,
            ModLoadError::StateRestoreFailed {
                status: FfiResult::INVALID_STATE,
                ..
            }
        ),
        "unexpected error: {restore_err:?}"
    );
    assert!(app.world().resource::<ModHost>().is_empty());

    // 5. Backwards compatibility: reloading a mod without state callbacks (`ordered_sets_mod`) succeeds cleanly!
    let clean_dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "legacy_reload");
    let clean_v1 = app.load_mod_with(clean_dylib.path(), opts.clone()).unwrap();
    let clean_v2 = app
        .reload_mod_with(clean_v1, opts)
        .expect("mod without state callbacks reloads cleanly");
    app.update();
    app.unload_mod(clean_v2).unwrap();
    assert!(app.world().resource::<Messages<ModUnloaded>>().len() >= 2);
}

#[test]
fn dynamic_component_hooks_fire_and_disarm_safely_on_unload() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    static HOOK_EVENTS: Mutex<Vec<(u64, u32, u32)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn record_hook_event(entity: FfiEntity, is_remove: u32, strength: u32) {
        HOOK_EVENTS
            .lock()
            .unwrap()
            .push((entity.to_bits(), is_remove, strength));
    }

    HOOK_EVENTS.lock().unwrap().clear();
    let ext_dylib = TempDylibCopy::new(
        &fixture_dylib("state_hooks_and_observers_mod"),
        "component_hooks",
    );

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_schedule(FixedUpdate);
    app.init_schedule(PostUpdate);
    // SAFETY: `record_hook_event` is a static `extern "C"` function valid for the lifetime of the test process.
    unsafe {
        app.world_mut()
            .resource_mut::<ModRegistry>()
            .register_custom_symbol("bam.v0.ext.record_hook", record_hook_event as *const c_void);
    }

    let mod_id = app
        .load_mod_with(
            ext_dylib.path(),
            LoadOptions::new().with_shadow_copy(true).strict(),
        )
        .expect("load state_hooks_and_observers_mod");

    let reg_comp = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("state_hooks_and_observers_mod::HookedAura")
        .cloned()
        .expect("HookedAura registered");
    let comp_id = reg_comp.component_id;

    // Spawn two entities and insert dynamic `HookedAura` — `on_add` (`is_remove == 0`) must fire for both!
    let e1: Entity = app.world_mut().spawn_empty().id();
    let e2: Entity = app.world_mut().spawn_empty().id();
    OwningPtr::make(
        HookedAuraMirror {
            strength: 55,
            tag: 1,
        },
        |ptr| {
            // SAFETY: `ptr` points to a valid `HookedAuraMirror` matching `comp_id`'s layout (`8` B, align `4`).
            unsafe {
                app.world_mut().entity_mut(e1).insert_by_id(comp_id, ptr);
            }
        },
    );
    OwningPtr::make(
        HookedAuraMirror {
            strength: 88,
            tag: 2,
        },
        |ptr| {
            // SAFETY: `ptr` points to a valid `HookedAuraMirror` matching `comp_id`'s layout (`8` B, align `4`).
            unsafe {
                app.world_mut().entity_mut(e2).insert_by_id(comp_id, ptr);
            }
        },
    );

    assert_eq!(
        *HOOK_EVENTS.lock().unwrap(),
        vec![(e1.to_bits(), 0, 55), (e2.to_bits(), 0, 88)]
    );

    // Despawn `e1` WHILE mod is still loaded -> `on_remove` (`is_remove == 1, strength == 55`) fires!
    assert!(app.world_mut().despawn(e1));
    assert_eq!(
        *HOOK_EVENTS.lock().unwrap(),
        vec![
            (e1.to_bits(), 0, 55),
            (e2.to_bits(), 0, 88),
            (e1.to_bits(), 1, 55),
        ]
    );

    // Now UNLOAD `state_hooks_and_observers_mod` (`dlclose`!) while `e2` STILL carries `HookedAura` in `World`!
    app.unload_mod(mod_id)
        .expect("unload state_hooks_and_observers_mod");

    // Despawning `e2` AFTER unload must NOT crash (`ArcSwapOption` disarmed the hook before `dlclose`)
    // and must NOT invoke the unmapped guest hook!
    assert!(app.world_mut().despawn(e2));
    assert_eq!(HOOK_EVENTS.lock().unwrap().len(), 3);
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ext::EnableObserverAndLayoutTest")]
struct EnableObserverAndLayoutTest;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ext::WatchedTarget")]
struct WatchedTarget;

#[repr(C)]
#[derive(Event, Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ext::PingSignal")]
struct CPingSignal {
    seq: u32,
}

#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "ext::DamageApplied")]
struct CDamageApplied {
    #[event_target]
    entity: TargetEntity,
    amount: f32,
    pad0: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ext::EnableInSetTest")]
struct EnableInSetTest;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "ext::DamagedMark")]
struct CDamagedMark {
    hits: u32,
}

#[test]
fn c99_dynamic_layout_and_entity_event_observers_lifecycle() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    static OBS_EVENTS: Mutex<Vec<(u32, u64, u32, i32)>> = Mutex::new(Vec::new());
    static WATCHED_BITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    unsafe extern "C" fn record_obs_event(
        kind: u32,
        target: FfiEntity,
        value_u32: u32,
        value_f32: f32,
    ) {
        OBS_EVENTS
            .lock()
            .unwrap()
            .push((kind, target.to_bits(), value_u32, value_f32 as i32));
    }

    unsafe extern "C" fn get_watched_target() -> FfiEntity {
        FfiEntity::from_bits(WATCHED_BITS.load(Ordering::SeqCst))
    }

    OBS_EVENTS.lock().unwrap().clear();

    #[derive(bevy_ecs::schedule::SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
    enum ObserverHostSet {
        BeforeLogic,
        Logic,
        AfterLogic,
    }

    let ext_dylib = TempDylibCopy::new(
        &fixture_dylib("state_hooks_and_observers_mod"),
        "obs_layout",
    );
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_schedule(FixedUpdate);
    app.init_schedule(PostUpdate);
    app.register_mod_visible_set("logic", ObserverHostSet::Logic);
    {
        use bevy_ecs::schedule::IntoScheduleConfigs;
        app.configure_sets(
            Update,
            (
                ObserverHostSet::BeforeLogic,
                ObserverHostSet::Logic,
                ObserverHostSet::AfterLogic,
            )
                .chain(),
        );
        app.add_systems(
            Update,
            (
                (|| {
                    OBS_EVENTS.lock().unwrap().push((10, 0, 0, 0));
                })
                .in_set(ObserverHostSet::BeforeLogic),
                (|| {
                    OBS_EVENTS.lock().unwrap().push((20, 0, 0, 0));
                })
                .in_set(ObserverHostSet::AfterLogic),
            ),
        );
    }

    app.register_mod_component::<EnableObserverAndLayoutTest>(ComponentOptions::new())
        .unwrap();
    app.register_mod_component::<EnableInSetTest>(ComponentOptions::new())
        .unwrap();
    app.register_mod_component::<WatchedTarget>(ComponentOptions::new())
        .unwrap();
    app.register_mod_component::<CDamagedMark>(ComponentOptions::new())
        .unwrap();
    app.register_mod_event::<CPingSignal>().unwrap();
    app.register_mod_entity_event::<CDamageApplied>().unwrap();

    let watched_entity = app.world_mut().spawn_empty().id();
    let unwatched_entity = app.world_mut().spawn_empty().id();
    WATCHED_BITS.store(watched_entity.to_bits(), Ordering::SeqCst);

    // SAFETY: `record_obs_event` and `get_watched_target` have C ABI linkage and live for the process.
    unsafe {
        let mut reg = app.world_mut().resource_mut::<ModRegistry>();
        reg.register_custom_symbol("bam.v0.ext.record_event", record_obs_event as *const c_void);
        reg.register_custom_symbol(
            "bam.v0.ext.get_watched_target",
            get_watched_target as *const c_void,
        );
    }

    let mod_id = app
        .load_mod(ext_dylib.path())
        .expect("load state_hooks_and_observers_mod");

    // 1. Verify runtime component registered via `bam_desc_component_with_layout` /
    //    `ComponentDescriptor::new_with_layout` can be attached to host entities and runs `drop_fn`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CPrivateLayoutMirror {
        magic: u64,
        counter: u32,
        pad0: u32,
    }
    let reg_snap = app.world().resource::<ModRegistry>().snapshot_arc();
    let priv_comp = reg_snap
        .get_by_name("state_hooks_and_observers_mod::CPrivateLayout")
        .expect("CPrivateLayout registered by C99 mod");
    let priv_id = priv_comp.component_id;
    let e_layout_pre = app.world_mut().spawn_empty().id();
    let e_layout_post = app.world_mut().spawn_empty().id();
    let mut val_a = CPrivateLayoutMirror {
        magic: 123,
        counter: 77,
        pad0: 0,
    };
    let mut val_b = CPrivateLayoutMirror {
        magic: 456,
        counter: 88,
        pad0: 0,
    };
    // SAFETY: `val_a` and `val_b` are initialized `CPrivateLayoutMirror` values matching `priv_id` layout.
    unsafe {
        let p_a = OwningPtr::new(core::ptr::NonNull::from(&mut val_a).cast::<u8>());
        app.world_mut()
            .entity_mut(e_layout_pre)
            .insert_by_id(priv_id, p_a);
        let p_b = OwningPtr::new(core::ptr::NonNull::from(&mut val_b).cast::<u8>());
        app.world_mut()
            .entity_mut(e_layout_post)
            .insert_by_id(priv_id, p_b);
    }
    assert!(app.world_mut().despawn(e_layout_pre));

    // 2. Trigger global `Event` (`PingSignal`) & `EntityEvent` (`DamageApplied`).
    app.world_mut().trigger(CPingSignal { seq: 9 });
    // Trigger on unwatched entity -> watched entity observer must NOT fire.
    app.world_mut().trigger(CDamageApplied {
        entity: TargetEntity::from(unwatched_entity),
        amount: 15.0,
        pad0: 0,
    });
    assert!(app.world().get::<CDamagedMark>(unwatched_entity).is_none());

    // Trigger on watched entity -> C99 observer fires AND enqueues `CDamagedMark` via `cmd_insert`!
    app.world_mut().trigger(CDamageApplied {
        entity: TargetEntity::from(watched_entity),
        amount: 33.0,
        pad0: 0,
    });
    // Flush observer deferred commands and verify `.in_set("host::logic")` executes strictly
    // between host `BeforeLogic` (10) and `AfterLogic` (20).
    app.update();
    app.world_mut().run_schedule(FixedUpdate);
    app.world_mut().run_schedule(PostUpdate);

    assert_eq!(
        app.world().get::<CDamagedMark>(watched_entity),
        Some(&CDamagedMark { hits: 33 })
    );
    assert_eq!(
        *OBS_EVENTS.lock().unwrap(),
        vec![
            (99, 0, 77, 123),
            (1, 0, 9, 0),
            (2, watched_entity.to_bits(), 0, 33),
            (10, 0, 0, 0),
            (5, 0, 0, 0),
            (20, 0, 0, 0),
            (6, 0, 0, 0),
        ]
    );

    // 3. Unload mod (`dlclose`!) and verify host sets still execute, while observers & dynamic
    //    destructors are cleanly disarmed/despawned.
    app.unload_mod(mod_id)
        .expect("unload state_hooks_and_observers_mod");

    let count_before = OBS_EVENTS.lock().unwrap().len();
    app.world_mut().trigger(CPingSignal { seq: 10 });
    app.world_mut().trigger(CDamageApplied {
        entity: TargetEntity::from(watched_entity),
        amount: 99.0,
        pad0: 0,
    });
    assert!(app.world_mut().despawn(e_layout_post));
    // Trigger/despawn caused zero callbacks; host Update still runs BeforeLogic & AfterLogic only.
    assert_eq!(OBS_EVENTS.lock().unwrap().len(), count_before);
    app.update();
    assert_eq!(
        &OBS_EVENTS.lock().unwrap()[count_before..],
        &[(10, 0, 0, 0), (20, 0, 0, 0)]
    );
}
