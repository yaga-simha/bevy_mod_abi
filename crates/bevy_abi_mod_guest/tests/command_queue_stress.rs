//! Handle-based and batched command queue stress & edge-case tests.

#![deny(unsafe_code)]

mod common;

use core::ffi::CStr;

use bevy_abi_mod_guest::prelude::*;
use common::{MockHostWorld, RecordedCommand};

#[cfg(not(feature = "derive"))]
macro_rules! impl_pod_no_derive {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: Plain `#[repr(C)]` test struct with fixed-width POD fields.
            #[allow(unsafe_code)]
            unsafe impl RawInsertable for $ty {}
            // SAFETY: Plain `#[repr(C)]` test struct with fixed-width POD fields.
            #[allow(unsafe_code)]
            unsafe impl AbiPod for $ty {}
        )*
    };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct PaddedPayload {
    tag: u8,
    // 3 bytes of natural C padding (`MaybeUninit<u8>`) — must be sound under Miri!
    value: u32,
}

// SAFETY: `PaddedPayload` intentionally leaves 3 uninitialized C alignment padding bytes to test
// Miri uninit-padding soundness across the command queue; every field is `AbiPod`.
#[allow(unsafe_code)]
unsafe impl RawInsertable for PaddedPayload {}
// SAFETY: Deterministic `#[repr(C)]` layout over `u8` and `u32` primitives with no drop glue.
#[allow(unsafe_code)]
unsafe impl AbiPod for PaddedPayload {}

impl GuestComponent for PaddedPayload {
    const NAME: &'static CStr = c"cmd::PaddedPayload";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct MarkerZst;

impl GuestComponent for MarkerZst {
    const NAME: &'static CStr = c"cmd::MarkerZst";
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "derive", derive(AbiPod))]
struct WithDefault {
    hp: f32,
}

impl GuestComponent for WithDefault {
    const NAME: &'static CStr = c"cmd::WithDefault";
}

#[cfg(not(feature = "derive"))]
impl_pod_no_derive!(MarkerZst, WithDefault);

#[test]
fn handle_commands_and_batched_spawn_fast_and_heap_paths() {
    let mut world = MockHostWorld::new();
    let padded_h = world.expose_component::<PaddedPayload>(
        "cmd::PaddedPayload",
        PaddedPayload::UUID,
        0,
        false,
    );
    let marker_h = world.expose_component::<MarkerZst>("cmd::MarkerZst", MarkerZst::UUID, 0, true);
    let def_h =
        world.expose_component::<WithDefault>("cmd::WithDefault", WithDefault::UUID, 0, true);

    // Also expose 20 u32 components to exercise the `> 16` heap fallback in `ValidatedCommands::spawn`.
    let mut bulk_handles = Vec::with_capacity(20);
    for idx in 0..20u32 {
        let name = format!("cmd::Bulk{idx}");
        let uuid = ComponentUuid::from_parts(&name, 4, 4, u64::from(idx));
        bulk_handles.push(world.expose_component::<u32>(&name, uuid, 0, false));
    }

    let bulk_clone = bulk_handles.clone();
    let inst = world
        .load_fn(c"cmd_stress_mod", move |app| {
            // Pre-resolve handles once at init ($O(1)$ lookup pattern):
            let resolved_padded = app
                .resolve_component::<PaddedPayload>()
                .expect("resolve PaddedPayload");
            assert_eq!(resolved_padded, padded_h);

            app.system(c"enqueue_all").build(move |ctx| {
                let cmds = ctx.commands();
                assert!(cmds.is_valid());

                // 1. `spawn_empty` + handle `insert` (including padded struct & ZST) + `insert_default` + `remove`:
                let e0 = cmds.spawn_empty().expect("spawn_empty");
                let payload = PaddedPayload {
                    tag: 7,
                    value: 0x1234_5678,
                };
                assert_eq!(cmds.insert(e0, padded_h, &payload), FfiResult::OK);
                assert_eq!(cmds.insert(e0, marker_h, &MarkerZst), FfiResult::OK);
                assert_eq!(cmds.insert_default(e0, def_h), FfiResult::OK);
                // `PaddedPayload` was registered with `has_default: false` -> `PERMISSION_DENIED`:
                assert_eq!(
                    cmds.insert_default(e0, padded_h),
                    FfiResult::PERMISSION_DENIED
                );
                assert_eq!(cmds.remove(e0, marker_h), FfiResult::OK);

                // 2. Convenience `GuestComponent` helpers:
                assert_eq!(cmds.insert_component(e0, &payload), FfiResult::OK);
                assert_eq!(
                    cmds.insert_component_default::<WithDefault>(e0),
                    FfiResult::OK
                );
                assert_eq!(cmds.remove_component::<PaddedPayload>(e0), FfiResult::OK);

                // 3. Batched `spawn(&[])` (0 entries -> routes cleanly):
                let e_empty_batch = cmds.spawn(&[]).expect("empty batch spawn");

                // 4. Batched `spawn` fast stack path (`<= 16` entries, zero heap allocation):
                let def_val = WithDefault { hp: 99.5 };
                let e_small = cmds
                    .spawn(&[
                        SpawnEntry::new(padded_h, &payload),
                        SpawnEntry::marker(marker_h),
                        SpawnEntry::new(def_h, &def_val),
                    ])
                    .expect("small batch spawn");
                assert_ne!(e_empty_batch, e_small);

                // 5. Batched `spawn` heap fallback (`20 > 16` entries):
                let bulk_vals: [u32; 20] = core::array::from_fn(|i| (i as u32) * 10);
                let bulk_entries: Vec<SpawnEntry<'_>> = bulk_clone
                    .iter()
                    .enumerate()
                    .map(|(i, &h)| SpawnEntry::new(h, &bulk_vals[i]))
                    .collect();
                let e_large = cmds.spawn(&bulk_entries).expect("large batch spawn");
                assert_eq!(cmds.despawn(e_large), FfiResult::OK);

                // 6. Invalid handle / null entity error checks:
                assert_eq!(
                    cmds.insert(e0, BamComponent::INVALID, &payload),
                    FfiResult::INVALID_COMPONENT
                );
                assert_eq!(
                    cmds.insert(FfiEntity::NULL, padded_h, &payload),
                    FfiResult::INVALID_ENTITY
                );
                assert_eq!(cmds.despawn(FfiEntity::NULL), FfiResult::INVALID_ENTITY);
                assert_eq!(
                    cmds.spawn(&[SpawnEntry::new(BamComponent::INVALID, &payload)]),
                    Err(FfiResult::INVALID_COMPONENT)
                );
            })
        })
        .expect("load cmd_stress_mod");

    assert_eq!(world.run_system(0), FfiResult::OK);

    assert_eq!(world.commands.len(), 12);
    assert_eq!(
        world.commands[0],
        RecordedCommand::SpawnEmpty(FfiEntity::from_bits(100))
    );
    assert_eq!(
        world.commands[9],
        RecordedCommand::SpawnBatch {
            entity: FfiEntity::from_bits(102),
            components: vec![(padded_h, 8), (marker_h, 0), (def_h, 4)],
        }
    );
    if let RecordedCommand::SpawnBatch { entity, components } = &world.commands[10] {
        assert_eq!(*entity, FfiEntity::from_bits(103));
        assert_eq!(components.len(), 20);
    } else {
        panic!("expected 20-component SpawnBatch");
    }
    assert_eq!(
        world.commands[11],
        RecordedCommand::Despawn(FfiEntity::from_bits(103))
    );

    world.unload_instance(inst);
}
