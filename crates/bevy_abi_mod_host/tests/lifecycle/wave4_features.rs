//! Wave 4 Lifecycle & Safety Feature Tests (`F-01`, `F-04`):
//! - `F-01`: `reload_mod` / `reload_mod_with` / `ModCommandsExt::reload_mod` preserving guest
//!   state via `BamManifest::save_state` and `BamManifest::restore_state`, propagating
//!   `StateSaveFailed` / `StateRestoreFailed`, and remaining backwards-compatible with
//!   `MIN_V0_2_SIZE` (`32`-byte) manifests that omit state callbacks.
//! - `F-04`: Dynamic component `on_add` and `on_remove` hooks firing on spawn/insert/remove/despawn,
//!   disarming cleanly via `ArcSwapOption` on `unload_mod` (`dlclose` safety when entities outlive
//!   the mod), and rolling back cleanly on failed `init`.

use crate::support::{TempDylibCopy, fixture_dylib};
use bevy_abi_mod_host::prelude::*;
use bevy_app::{App, Update};
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::system::Commands;
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, AbiPod)]
struct HookedAuraMirror {
    strength: u32,
    tag: u32,
}

#[test]
fn wave4_f01_hot_reload_transfers_state_handles_errors_and_supports_legacy_manifests() {
    let w4_dylib = TempDylibCopy::new(&fixture_dylib("w4_ext_mod"), "f01_state");
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(W4StateProbe::default());
    app.register_mod_resource::<W4StateProbe>().unwrap();

    let opts = LoadOptions::new().with_shadow_copy(true).strict();
    let mod_v1 = app
        .load_mod_with(w4_dylib.path(), opts.clone())
        .expect("initial load of w4_ext_mod");

    // Run 3 frames: `reload_epoch == 0`, `accumulated_ticks == 3`.
    app.update();
    app.update();
    app.update();
    assert_eq!(
        *app.world().resource::<W4StateProbe>(),
        W4StateProbe {
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
        *app.world().resource::<W4StateProbe>(),
        W4StateProbe {
            reload_epoch: 1,
            accumulated_ticks: 4,
            fail_next_save: 0,
            fail_next_restore: 0,
        }
    );

    // 2. Deferred `Commands::reload_mod` inside a Bevy system also preserves state (`epoch -> 2`)!
    let trigger_deferred = std::sync::Arc::new(AtomicUsize::new(1));
    let trigger_c = std::sync::Arc::clone(&trigger_deferred);
    app.add_systems(Update, move |mut commands: Commands<'_, '_>| {
        if trigger_c.swap(0, Ordering::SeqCst) == 1 {
            commands.reload_mod(mod_v2);
        }
    });
    app.update(); // Runs `w4_state_tick` (ticks=5), then applies deferred `reload_mod` -> epoch=2!
    app.update(); // Runs reloaded instance -> epoch=2, ticks=6!
    assert_eq!(app.world().resource::<W4StateProbe>().reload_epoch, 2);
    assert_eq!(app.world().resource::<W4StateProbe>().accumulated_ticks, 6);

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
        .resource_mut::<W4StateProbe>()
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
        .resource_mut::<W4StateProbe>()
        .fail_next_save = 0;

    // 4. `restore_state` failure rolls back the replacement mod cleanly (`StateRestoreFailed`)!
    app.world_mut()
        .resource_mut::<W4StateProbe>()
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

    // 5. Backwards compatibility: reloading a mod without state callbacks (`l05c_clean_mod`) succeeds cleanly!
    let clean_dylib = TempDylibCopy::new(&fixture_dylib("l05c_clean_mod"), "f01_legacy");
    let clean_v1 = app.load_mod_with(clean_dylib.path(), opts.clone()).unwrap();
    let clean_v2 = app
        .reload_mod_with(clean_v1, opts)
        .expect("mod without state callbacks reloads cleanly");
    app.update();
    app.unload_mod(clean_v2).unwrap();
    assert!(app.world().resource::<Messages<ModUnloaded>>().len() >= 2);
}

#[test]
fn wave4_f04_dynamic_component_hooks_fire_and_disarm_safely_on_unload() {
    static HOOK_EVENTS: Mutex<Vec<(u64, u32, u32)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn record_hook_event(entity: FfiEntity, is_remove: u32, strength: u32) {
        HOOK_EVENTS
            .lock()
            .unwrap()
            .push((entity.to_bits(), is_remove, strength));
    }

    HOOK_EVENTS.lock().unwrap().clear();
    let w4_dylib = TempDylibCopy::new(&fixture_dylib("w4_ext_mod"), "f04_hooks");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    // SAFETY: `record_hook_event` is a static `extern "C"` function valid for the lifetime of the test process.
    unsafe {
        app.world_mut()
            .resource_mut::<ModRegistry>()
            .register_custom_symbol("bam.v0.w4.record_hook", record_hook_event as *const c_void);
    }

    let mod_id = app
        .load_mod_with(
            w4_dylib.path(),
            LoadOptions::new().with_shadow_copy(true).strict(),
        )
        .expect("load w4_ext_mod");

    let reg_comp = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("w4_ext_mod::HookedAura")
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

    // Now UNLOAD `w4_ext_mod` (`dlclose`!) while `e2` STILL carries `HookedAura` in `World`!
    app.unload_mod(mod_id).expect("unload w4_ext_mod");

    // Despawning `e2` AFTER unload must NOT crash (`ArcSwapOption` disarmed the hook before `dlclose`)
    // and must NOT invoke the unmapped guest hook!
    assert!(app.world_mut().despawn(e2));
    assert_eq!(HOOK_EVENTS.lock().unwrap().len(), 3);
}
