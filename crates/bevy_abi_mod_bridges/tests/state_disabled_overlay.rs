//! Integration tests for Phase 3 Item #12:
//! 1. Dynamic generation-indexed `bevy_state` slabs (`>= 24` distinct states, `>= 64` variants),
//!    slot release/reuse generation validation, `SubStates`, and read-only `ComputedStates`.
//! 2. `StdTypesBridgePlugin` exposure of `bevy_ecs::entity_disabling::Disabled` for guest C-ABI
//!    queries (`DefaultQueryFilters`, `Allow<Disabled>`, `With<Disabled>`, `Without<Disabled>`,
//!    and `Has<Disabled>`).
//! 3. Opt-in `AssetOverlayPolicy` (`Disabled`, `LoadOrderPriority`, `TopologicalOverlay` driven by
//!    `ModDependencyGraph`, and `ExplicitPriority`) with 2-mod & 3-mod overrides, fallbacks,
//!    sandboxing, and automatic revert on unload.

#[cfg(feature = "bridge-state")]
mod state_slabs_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bevy_abi_mod_bridges::state::{
        MOD_STATE_FLAG_COMPUTED, MOD_STATE_FLAG_SUB_STATE, ModDespawnOnEnter, ModDespawnOnExit,
        ModNextState, ModStateEntry, ModStateExt, ModStateHandle, ModStateKind, ModStateSlab,
        ModStateTransitionRequest, STATE_ABSENT_DISCRIMINANT,
    };
    use bevy_abi_mod_host::{AbiModPlugin, ModScheduleMap, ModVisibleSets};
    use bevy_abi_mod_host_sys::{GuestSystem, ModRegistry};
    use bevy_abi_mod_sys::{BamSystemCtx, FfiResult, ScheduleId};
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity_disabling::Disabled;
    use bevy_ecs::message::Messages;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_state::app::StatesPlugin;
    use bevy_state::state::{
        ComputedStates, OnEnter, OnExit, OnTransition, State, States, SubStates,
    };
    use bevy_state::state_scoped::{DespawnOnEnter, DespawnOnExit};
    use core::ffi::c_void;

    macro_rules! define_simple_states {
        ($($name:ident),+ $(,)?) => {
            $(
                #[derive(States, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
                enum $name {
                    #[default]
                    Alpha,
                    Beta,
                }
            )+
        };
    }

    // 23 distinct top-level states (`DynState1`..`DynState23`) plus 64-variant `BigState0` &
    // `BigState23` plus reusable `DynState24`.
    define_simple_states!(
        DynState1, DynState2, DynState3, DynState4, DynState5, DynState6, DynState7, DynState8,
        DynState9, DynState10, DynState11, DynState12, DynState13, DynState14, DynState15,
        DynState16, DynState17, DynState18, DynState19, DynState20, DynState21, DynState22,
        DynState24,
    );

    macro_rules! define_64_variant_state {
        ($name:ident) => {
            #[derive(States, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
            enum $name {
                #[default]
                V0,
                V1,
                V2,
                V3,
                V4,
                V5,
                V6,
                V7,
                V8,
                V9,
                V10,
                V11,
                V12,
                V13,
                V14,
                V15,
                V16,
                V17,
                V18,
                V19,
                V20,
                V21,
                V22,
                V23,
                V24,
                V25,
                V26,
                V27,
                V28,
                V29,
                V30,
                V31,
                V32,
                V33,
                V34,
                V35,
                V36,
                V37,
                V38,
                V39,
                V40,
                V41,
                V42,
                V43,
                V44,
                V45,
                V46,
                V47,
                V48,
                V49,
                V50,
                V51,
                V52,
                V53,
                V54,
                V55,
                V56,
                V57,
                V58,
                V59,
                V60,
                V61,
                V62,
                V63,
            }

            impl $name {
                const ALL: [(u32, &'static str, Self); 64] = [
                    (0, "V0", Self::V0),
                    (1, "V1", Self::V1),
                    (2, "V2", Self::V2),
                    (3, "V3", Self::V3),
                    (4, "V4", Self::V4),
                    (5, "V5", Self::V5),
                    (6, "V6", Self::V6),
                    (7, "V7", Self::V7),
                    (8, "V8", Self::V8),
                    (9, "V9", Self::V9),
                    (10, "V10", Self::V10),
                    (11, "V11", Self::V11),
                    (12, "V12", Self::V12),
                    (13, "V13", Self::V13),
                    (14, "V14", Self::V14),
                    (15, "V15", Self::V15),
                    (16, "V16", Self::V16),
                    (17, "V17", Self::V17),
                    (18, "V18", Self::V18),
                    (19, "V19", Self::V19),
                    (20, "V20", Self::V20),
                    (21, "V21", Self::V21),
                    (22, "V22", Self::V22),
                    (23, "V23", Self::V23),
                    (24, "V24", Self::V24),
                    (25, "V25", Self::V25),
                    (26, "V26", Self::V26),
                    (27, "V27", Self::V27),
                    (28, "V28", Self::V28),
                    (29, "V29", Self::V29),
                    (30, "V30", Self::V30),
                    (31, "V31", Self::V31),
                    (32, "V32", Self::V32),
                    (33, "V33", Self::V33),
                    (34, "V34", Self::V34),
                    (35, "V35", Self::V35),
                    (36, "V36", Self::V36),
                    (37, "V37", Self::V37),
                    (38, "V38", Self::V38),
                    (39, "V39", Self::V39),
                    (40, "V40", Self::V40),
                    (41, "V41", Self::V41),
                    (42, "V42", Self::V42),
                    (43, "V43", Self::V43),
                    (44, "V44", Self::V44),
                    (45, "V45", Self::V45),
                    (46, "V46", Self::V46),
                    (47, "V47", Self::V47),
                    (48, "V48", Self::V48),
                    (49, "V49", Self::V49),
                    (50, "V50", Self::V50),
                    (51, "V51", Self::V51),
                    (52, "V52", Self::V52),
                    (53, "V53", Self::V53),
                    (54, "V54", Self::V54),
                    (55, "V55", Self::V55),
                    (56, "V56", Self::V56),
                    (57, "V57", Self::V57),
                    (58, "V58", Self::V58),
                    (59, "V59", Self::V59),
                    (60, "V60", Self::V60),
                    (61, "V61", Self::V61),
                    (62, "V62", Self::V62),
                    (63, "V63", Self::V63),
                ];
            }
        };
    }

    define_64_variant_state!(BigState0);
    define_64_variant_state!(BigState23);

    #[derive(Component)]
    struct ScopedMarker;

    #[test]
    fn dynamic_slab_supports_24_states_64_variants_and_generation_reuse() {
        static BIG23_EXIT_0: AtomicUsize = AtomicUsize::new(0);
        static BIG23_TRANS_0_63: AtomicUsize = AtomicUsize::new(0);
        static BIG23_ENTER_63: AtomicUsize = AtomicUsize::new(0);
        static BIG23_IN_63_TICKS: AtomicUsize = AtomicUsize::new(0);

        BIG23_EXIT_0.store(0, Ordering::SeqCst);
        BIG23_TRANS_0_63.store(0, Ordering::SeqCst);
        BIG23_ENTER_63.store(0, Ordering::SeqCst);
        BIG23_IN_63_TICKS.store(0, Ordering::SeqCst);

        extern "C" fn cb_exit_0(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            BIG23_EXIT_0.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_trans_0_63(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            BIG23_TRANS_0_63.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_enter_63(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            BIG23_ENTER_63.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_in_63(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            BIG23_IN_63_TICKS.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }

        let mut app = App::new();
        app.add_plugins((AbiModPlugin, StatesPlugin));

        // Slot 0: 64-variant state (exceeds legacy 32-variant limit!).
        let spec0 = app
            .expose_mod_state::<BigState0>(ScheduleId::custom(10_000), &BigState0::ALL)
            .expect("expose 64-variant BigState0");
        assert_eq!(spec0.state_slot(), 0);
        assert_eq!(spec0.generation(), 1);
        assert_eq!(spec0.variant_count(), 64);

        // Slots 1..=22: 22 more distinct states.
        macro_rules! expose_pair {
            ($ty:ty, $base:expr) => {
                app.expose_mod_state::<$ty>(
                    ScheduleId::custom($base),
                    &[(0, "Alpha", <$ty>::Alpha), (1, "Beta", <$ty>::Beta)],
                )
                .expect(stringify!($ty))
            };
        }

        let _s1 = expose_pair!(DynState1, 20_000);
        let _s2 = expose_pair!(DynState2, 20_100);
        let _s3 = expose_pair!(DynState3, 20_200);
        let _s4 = expose_pair!(DynState4, 20_300);
        let _s5 = expose_pair!(DynState5, 20_400);
        let _s6 = expose_pair!(DynState6, 20_500);
        let _s7 = expose_pair!(DynState7, 20_600);
        let _s8 = expose_pair!(DynState8, 20_700);
        let _s9 = expose_pair!(DynState9, 20_800);
        let _s10 = expose_pair!(DynState10, 20_900);
        let _s11 = expose_pair!(DynState11, 21_000);
        let spec12 = expose_pair!(DynState12, 21_100);
        let _s13 = expose_pair!(DynState13, 21_200);
        let _s14 = expose_pair!(DynState14, 21_300);
        let _s15 = expose_pair!(DynState15, 21_400);
        let _s16 = expose_pair!(DynState16, 21_500);
        let _s17 = expose_pair!(DynState17, 21_600);
        let _s18 = expose_pair!(DynState18, 21_700);
        let _s19 = expose_pair!(DynState19, 21_800);
        let _s20 = expose_pair!(DynState20, 21_900);
        let _s21 = expose_pair!(DynState21, 22_000);
        let _s22 = expose_pair!(DynState22, 22_100);

        // Slot 23 (24th distinct state!): ALSO 64 variants!
        let spec23 = app
            .expose_mod_state::<BigState23>(ScheduleId::custom(30_000), &BigState23::ALL)
            .expect("expose 24th state BigState23 with 64 variants");

        assert_eq!(spec12.state_slot(), 12);
        assert_eq!(spec23.state_slot(), 23);
        assert_eq!(spec23.generation(), 1);
        assert_eq!(spec23.variant_count(), 64);

        {
            let slab = app.world().resource::<ModStateSlab>();
            assert_eq!(slab.len(), 24);
            assert_eq!(slab.slot_capacity(), 24);
            assert!(slab.is_alive(spec0.handle()));
            assert!(slab.is_alive(spec12.handle()));
            assert!(slab.is_alive(spec23.handle()));
            assert_eq!(slab.variant_count(spec23.handle()), Some(64));
        }

        // Verify schedule map contains variant 63's enter/exit/transition schedules on slot 23.
        {
            let sched_map = app.world().resource::<ModScheduleMap>();
            assert!(sched_map.contains(spec23.on_enter_schedule(63).unwrap()));
            assert!(sched_map.contains(spec23.on_exit_schedule(0).unwrap()));
            assert!(sched_map.contains(spec23.on_transition_schedule(0, 63).unwrap()));
        }

        // Wire guest systems to slot 23's variant 0 -> 63 lifecycle & visible in_state set.
        let reg = app.world().resource::<ModRegistry>().clone();
        let in_63_set = app
            .world()
            .resource::<ModVisibleSets>()
            .resolve("state.BigState23.V63")
            .expect("state.BigState23.V63 visible set");

        app.add_systems(
            OnExit(BigState23::V0),
            GuestSystem::new(
                "exit_0",
                vec![],
                cb_exit_0,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnTransition {
                exited: BigState23::V0,
                entered: BigState23::V63,
            },
            GuestSystem::new(
                "trans_0_63",
                vec![],
                cb_trans_0_63,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnEnter(BigState23::V63),
            GuestSystem::new(
                "enter_63",
                vec![],
                cb_enter_63,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            Update,
            GuestSystem::new("in_63", vec![], cb_in_63, core::ptr::null_mut(), reg)
                .in_set(in_63_set),
        );

        // Spawn dynamic state-scoped entities on variant 0 (exit) and variant 63 (enter) — including
        // a `Disabled` entity — plus variant 63 on slot 0!
        let e23_exit = app
            .world_mut()
            .spawn((ScopedMarker, spec23.despawn_on_exit_marker(0).unwrap()))
            .id();
        let e23_enter = app
            .world_mut()
            .spawn((
                ScopedMarker,
                Disabled,
                spec23.despawn_on_enter_marker(63).unwrap(),
            ))
            .id();
        let e0_enter_63 = app
            .world_mut()
            .spawn((ScopedMarker, spec0.despawn_on_enter_marker(63).unwrap()))
            .id();

        assert!(
            app.world()
                .get::<DespawnOnExit<BigState23>>(e23_exit)
                .is_some()
        );
        assert!(
            app.world()
                .get::<DespawnOnEnter<BigState23>>(e23_enter)
                .is_some()
        );
        assert!(
            app.world()
                .get::<DespawnOnEnter<BigState0>>(e0_enter_63)
                .is_some()
        );

        // Frame 1: in V0 -> in_63 does not tick.
        app.update();
        assert_eq!(BIG23_IN_63_TICKS.load(Ordering::SeqCst), 0);

        // Frame 2: request transitions on BOTH slot 0 (`0 -> 63`) and slot 23 (`0 -> 63`) via
        // universal `ModStateTransitionRequest` message stream!
        {
            let mut msgs = app
                .world_mut()
                .resource_mut::<Messages<ModStateTransitionRequest>>();
            msgs.write(spec0.transition_request(63).unwrap());
            msgs.write(spec23.transition_request(63).unwrap());
        }
        app.update();

        assert_eq!(
            *app.world().resource::<State<BigState0>>().get(),
            BigState0::V63
        );
        assert_eq!(
            *app.world().resource::<State<BigState23>>().get(),
            BigState23::V63
        );
        assert_eq!(BIG23_EXIT_0.load(Ordering::SeqCst), 1);
        assert_eq!(BIG23_TRANS_0_63.load(Ordering::SeqCst), 1);
        assert_eq!(BIG23_ENTER_63.load(Ordering::SeqCst), 1);
        assert_eq!(BIG23_IN_63_TICKS.load(Ordering::SeqCst), 1);

        // Verify live `ModStateSlab` & `ModStateEntry` snapshot mirror reflect `63`!
        assert_eq!(
            app.world()
                .resource::<ModStateSlab>()
                .current_discriminant(spec23.handle()),
            Some(Some(63))
        );
        let entry23 = *app
            .world()
            .get::<ModStateEntry>(spec23.state_entry_entity())
            .expect("BigState23 ModStateEntry");
        assert_eq!(entry23.state_handle(), spec23.handle());
        assert_eq!(entry23.active_discriminant(), Some(63));
        assert!(entry23.is_mutable());

        // All three dynamically scoped entities (including `Disabled`) despawned on variant 63!
        assert!(app.world().get_entity(e23_exit).is_err());
        assert!(app.world().get_entity(e23_enter).is_err());
        assert!(app.world().get_entity(e0_enter_63).is_err());

        // Generation-indexed slab release & reuse (ABA protection).
        let stale_handle = spec12.handle();
        let stale_entity = spec12.state_entry_entity();
        assert_eq!(stale_handle, ModStateHandle::new(12, 1));
        assert!(ModStateSlab::release_in_world(
            app.world_mut(),
            stale_handle
        ));
        assert!(!ModStateSlab::release_in_world(
            app.world_mut(),
            stale_handle
        ));
        assert!(app.world().get_entity(stale_entity).is_err());
        assert!(
            !app.world()
                .resource::<ModStateSlab>()
                .is_alive(stale_handle)
        );
        assert_eq!(
            ModStateSlab::request_transition(app.world_mut(), stale_handle, 1),
            FfiResult::NOT_FOUND
        );

        // Exposing `DynState24` reuses slot 12 with bumped `generation == 2`!
        let spec24 = expose_pair!(DynState24, 22_200);
        assert_eq!(spec24.handle(), ModStateHandle::new(12, 2));
        assert_eq!(app.world().resource::<ModStateSlab>().len(), 24);
        assert_eq!(app.world().resource::<ModStateSlab>().slot_capacity(), 24);

        // Stale `(12, 1)` handle is STILL rejected while fresh `(12, 2)` handle transitions cleanly!
        assert_eq!(
            ModStateSlab::request_transition(app.world_mut(), stale_handle, 1),
            FfiResult::NOT_FOUND
        );
        let probe = app
            .world_mut()
            .spawn((
                ModDespawnOnExit::new(stale_handle, 0),
                ModDespawnOnEnter::new(stale_handle, 1),
            ))
            .id();
        assert!(
            app.world()
                .get::<DespawnOnExit<DynState24>>(probe)
                .is_none()
        );
        assert!(
            app.world()
                .get::<DespawnOnEnter<DynState24>>(probe)
                .is_none()
        );

        assert_eq!(
            ModStateSlab::request_transition(app.world_mut(), spec24.handle(), 1),
            FfiResult::OK
        );
        app.update();
        assert_eq!(
            *app.world().resource::<State<DynState24>>().get(),
            DynState24::Beta
        );
        assert_eq!(
            app.world()
                .resource::<ModStateSlab>()
                .current_discriminant(spec24.handle()),
            Some(Some(1))
        );
    }

    #[derive(States, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    enum AppPhase {
        #[default]
        Menu,
        InGame,
        Paused,
    }

    #[derive(SubStates, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    #[source(AppPhase = AppPhase::InGame)]
    enum BattleSubState {
        #[default]
        Prep,
        Combat,
        Victory,
    }

    #[test]
    fn sub_states_conditional_lifecycle_transitions_and_hooks() {
        static ENTER_PREP: AtomicUsize = AtomicUsize::new(0);
        static EXIT_PREP: AtomicUsize = AtomicUsize::new(0);
        static TRANS_PREP_COMBAT: AtomicUsize = AtomicUsize::new(0);
        static ENTER_COMBAT: AtomicUsize = AtomicUsize::new(0);
        static EXIT_COMBAT: AtomicUsize = AtomicUsize::new(0);
        static IN_COMBAT_TICKS: AtomicUsize = AtomicUsize::new(0);

        ENTER_PREP.store(0, Ordering::SeqCst);
        EXIT_PREP.store(0, Ordering::SeqCst);
        TRANS_PREP_COMBAT.store(0, Ordering::SeqCst);
        ENTER_COMBAT.store(0, Ordering::SeqCst);
        EXIT_COMBAT.store(0, Ordering::SeqCst);
        IN_COMBAT_TICKS.store(0, Ordering::SeqCst);

        extern "C" fn cb_enter_prep(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            ENTER_PREP.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_exit_prep(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            EXIT_PREP.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_trans_p_c(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            TRANS_PREP_COMBAT.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_enter_combat(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            ENTER_COMBAT.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_exit_combat(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            EXIT_COMBAT.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_in_combat(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            IN_COMBAT_TICKS.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }

        let mut app = App::new();
        app.add_plugins((AbiModPlugin, StatesPlugin));

        let root_spec = app
            .expose_mod_state::<AppPhase>(
                ScheduleId::custom(4000),
                &[
                    (0, "Menu", AppPhase::Menu),
                    (1, "InGame", AppPhase::InGame),
                    (2, "Paused", AppPhase::Paused),
                ],
            )
            .expect("expose AppPhase");

        let sub_spec = app
            .register_mod_sub_state::<BattleSubState>(
                ScheduleId::custom(5000),
                &[
                    (0, "Prep", BattleSubState::Prep),
                    (1, "Combat", BattleSubState::Combat),
                    (2, "Victory", BattleSubState::Victory),
                ],
            )
            .expect("register BattleSubState");

        assert_eq!(sub_spec.kind(), ModStateKind::SubState);
        assert!(sub_spec.is_mutable());
        assert!(sub_spec.next_state_handle().is_some());

        let reg = app.world().resource::<ModRegistry>().clone();
        let in_combat_set = app
            .world()
            .resource::<ModVisibleSets>()
            .resolve("state.BattleSubState.Combat")
            .expect("state.BattleSubState.Combat set");

        app.add_systems(
            OnEnter(BattleSubState::Prep),
            GuestSystem::new(
                "enter_prep",
                vec![],
                cb_enter_prep,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnExit(BattleSubState::Prep),
            GuestSystem::new(
                "exit_prep",
                vec![],
                cb_exit_prep,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnTransition {
                exited: BattleSubState::Prep,
                entered: BattleSubState::Combat,
            },
            GuestSystem::new(
                "trans_p_c",
                vec![],
                cb_trans_p_c,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnEnter(BattleSubState::Combat),
            GuestSystem::new(
                "enter_combat",
                vec![],
                cb_enter_combat,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnExit(BattleSubState::Combat),
            GuestSystem::new(
                "exit_combat",
                vec![],
                cb_exit_combat,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            Update,
            GuestSystem::new(
                "in_combat",
                vec![],
                cb_in_combat,
                core::ptr::null_mut(),
                reg,
            )
            .in_set(in_combat_set),
        );

        // Frame 1: in `Menu` -> `BattleSubState` does NOT exist yet (no panic!).
        app.update();
        assert!(
            app.world()
                .get_resource::<State<BattleSubState>>()
                .is_none()
        );
        assert_eq!(
            app.world()
                .resource::<ModStateSlab>()
                .current_discriminant(sub_spec.handle()),
            Some(None)
        );
        let sub_entry = *app
            .world()
            .get::<ModStateEntry>(sub_spec.state_entry_entity())
            .unwrap();
        assert!(!sub_entry.is_active());
        assert_eq!(sub_entry.current_discriminant, STATE_ABSENT_DISCRIMINANT);
        assert_ne!(sub_entry.flags & MOD_STATE_FLAG_SUB_STATE, 0);
        assert_eq!(
            app.world()
                .resource::<ModNextState<BattleSubState>>()
                .active_discriminant(),
            None
        );
        assert_eq!(ENTER_PREP.load(Ordering::SeqCst), 0);
        assert_eq!(IN_COMBAT_TICKS.load(Ordering::SeqCst), 0);

        // Frame 2: transition `AppPhase`: Menu -> InGame => `BattleSubState::Prep` spawns!
        assert_eq!(
            ModStateSlab::request_transition(app.world_mut(), root_spec.handle(), 1),
            FfiResult::OK
        );
        app.update();
        assert_eq!(
            *app.world().resource::<State<BattleSubState>>().get(),
            BattleSubState::Prep
        );
        assert_eq!(ENTER_PREP.load(Ordering::SeqCst), 1);
        assert_eq!(
            app.world()
                .resource::<ModStateSlab>()
                .current_discriminant(sub_spec.handle()),
            Some(Some(0))
        );
        assert_eq!(
            app.world()
                .get::<ModStateEntry>(sub_spec.state_entry_entity())
                .unwrap()
                .active_discriminant(),
            Some(0)
        );

        // Attach dynamic despawn markers on Prep exit & Combat enter, then transition Prep -> Combat.
        let prep_scoped = app
            .world_mut()
            .spawn((ScopedMarker, sub_spec.despawn_on_exit_marker(0).unwrap()))
            .id();
        let enter_combat_scoped = app
            .world_mut()
            .spawn((ScopedMarker, sub_spec.despawn_on_enter_marker(1).unwrap()))
            .id();

        app.world_mut()
            .resource_mut::<Messages<ModStateTransitionRequest>>()
            .write(sub_spec.transition_request(1).unwrap());
        app.update();
        assert_eq!(
            *app.world().resource::<State<BattleSubState>>().get(),
            BattleSubState::Combat
        );
        assert_eq!(EXIT_PREP.load(Ordering::SeqCst), 1);
        assert_eq!(TRANS_PREP_COMBAT.load(Ordering::SeqCst), 1);
        assert_eq!(ENTER_COMBAT.load(Ordering::SeqCst), 1);
        assert_eq!(IN_COMBAT_TICKS.load(Ordering::SeqCst), 1);
        assert!(app.world().get_entity(prep_scoped).is_err());
        assert!(app.world().get_entity(enter_combat_scoped).is_err());

        // Spawn Combat-scoped entity and leave `InGame` -> `Menu`: sub-state despawns & exits!
        let combat_scoped = app
            .world_mut()
            .spawn((ScopedMarker, sub_spec.despawn_on_exit_marker(1).unwrap()))
            .id();
        assert_eq!(
            ModStateSlab::request_transition(app.world_mut(), root_spec.handle(), 0),
            FfiResult::OK
        );
        app.update();
        assert!(
            app.world()
                .get_resource::<State<BattleSubState>>()
                .is_none()
        );
        assert_eq!(EXIT_COMBAT.load(Ordering::SeqCst), 1);
        assert_eq!(IN_COMBAT_TICKS.load(Ordering::SeqCst), 1);
        assert!(app.world().get_entity(combat_scoped).is_err());
        assert_eq!(
            app.world()
                .resource::<ModStateSlab>()
                .current_discriminant(sub_spec.handle()),
            Some(None)
        );
        assert!(
            !app.world()
                .get::<ModStateEntry>(sub_spec.state_entry_entity())
                .unwrap()
                .is_active()
        );
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum HudMode {
        Lobby,
        InMatch,
    }

    impl ComputedStates for HudMode {
        type SourceStates = AppPhase;

        fn compute(sources: AppPhase) -> Option<Self> {
            match sources {
                AppPhase::Menu => Some(Self::Lobby),
                AppPhase::InGame | AppPhase::Paused => Some(Self::InMatch),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum ActiveSimulation {
        Running,
    }

    impl ComputedStates for ActiveSimulation {
        type SourceStates = AppPhase;

        fn compute(sources: AppPhase) -> Option<Self> {
            match sources {
                AppPhase::InGame => Some(Self::Running),
                AppPhase::Menu | AppPhase::Paused => None,
            }
        }
    }

    #[test]
    fn computed_states_read_only_exposure_and_transition_hooks() {
        static HUD_ENTER_LOBBY: AtomicUsize = AtomicUsize::new(0);
        static HUD_EXIT_LOBBY: AtomicUsize = AtomicUsize::new(0);
        static HUD_TRANS_L_TO_M: AtomicUsize = AtomicUsize::new(0);
        static HUD_ENTER_INMATCH: AtomicUsize = AtomicUsize::new(0);
        static HUD_INMATCH_TICKS: AtomicUsize = AtomicUsize::new(0);
        static SIM_ENTER: AtomicUsize = AtomicUsize::new(0);
        static SIM_EXIT: AtomicUsize = AtomicUsize::new(0);

        HUD_ENTER_LOBBY.store(0, Ordering::SeqCst);
        HUD_EXIT_LOBBY.store(0, Ordering::SeqCst);
        HUD_TRANS_L_TO_M.store(0, Ordering::SeqCst);
        HUD_ENTER_INMATCH.store(0, Ordering::SeqCst);
        HUD_INMATCH_TICKS.store(0, Ordering::SeqCst);
        SIM_ENTER.store(0, Ordering::SeqCst);
        SIM_EXIT.store(0, Ordering::SeqCst);

        extern "C" fn cb_hud_enter_lobby(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            HUD_ENTER_LOBBY.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_hud_exit_lobby(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            HUD_EXIT_LOBBY.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_hud_trans(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            HUD_TRANS_L_TO_M.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_hud_enter_match(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            HUD_ENTER_INMATCH.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_hud_in_match(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            HUD_INMATCH_TICKS.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_sim_enter(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            SIM_ENTER.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }
        extern "C" fn cb_sim_exit(_: *mut BamSystemCtx, _: *mut c_void) -> FfiResult {
            SIM_EXIT.fetch_add(1, Ordering::SeqCst);
            FfiResult::OK
        }

        let mut app = App::new();
        app.add_plugins((AbiModPlugin, StatesPlugin));

        let root_spec = app
            .expose_mod_state::<AppPhase>(
                ScheduleId::custom(6000),
                &[
                    (0, "Menu", AppPhase::Menu),
                    (1, "InGame", AppPhase::InGame),
                    (2, "Paused", AppPhase::Paused),
                ],
            )
            .expect("expose AppPhase");

        let hud_spec = app
            .register_mod_computed_state::<HudMode>(
                ScheduleId::custom(6100),
                &[
                    (0, "Lobby", HudMode::Lobby),
                    (1, "InMatch", HudMode::InMatch),
                ],
            )
            .expect("register HudMode");

        let sim_spec = app
            .register_mod_computed_state::<ActiveSimulation>(
                ScheduleId::custom(6200),
                &[(0, "Running", ActiveSimulation::Running)],
            )
            .expect("register ActiveSimulation");

        // Verify strict read-only guarantees on ComputedStates.
        assert_eq!(hud_spec.kind(), ModStateKind::Computed);
        assert!(!hud_spec.is_mutable());
        assert!(hud_spec.next_state_handle().is_none());
        assert!(hud_spec.next_state_resource_name().is_none());
        assert!(hud_spec.transition_request(1).is_none());
        assert_eq!(
            ModStateSlab::request_transition(app.world_mut(), hud_spec.handle(), 1),
            FfiResult::PERMISSION_DENIED
        );

        let reg = app.world().resource::<ModRegistry>().clone();
        assert!(
            reg.get_by_handle(hud_spec.mod_state_entry_handle())
                .unwrap()
                .is_immutable()
        );
        let in_match_set = app
            .world()
            .resource::<ModVisibleSets>()
            .resolve("state.HudMode.InMatch")
            .expect("state.HudMode.InMatch set");

        app.add_systems(
            OnEnter(HudMode::Lobby),
            GuestSystem::new(
                "hud_el",
                vec![],
                cb_hud_enter_lobby,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnExit(HudMode::Lobby),
            GuestSystem::new(
                "hud_xl",
                vec![],
                cb_hud_exit_lobby,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnTransition {
                exited: HudMode::Lobby,
                entered: HudMode::InMatch,
            },
            GuestSystem::new(
                "hud_tr",
                vec![],
                cb_hud_trans,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnEnter(HudMode::InMatch),
            GuestSystem::new(
                "hud_em",
                vec![],
                cb_hud_enter_match,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            Update,
            GuestSystem::new(
                "hud_tick",
                vec![],
                cb_hud_in_match,
                core::ptr::null_mut(),
                reg.clone(),
            )
            .in_set(in_match_set),
        );
        app.add_systems(
            OnEnter(ActiveSimulation::Running),
            GuestSystem::new(
                "sim_en",
                vec![],
                cb_sim_enter,
                core::ptr::null_mut(),
                reg.clone(),
            ),
        );
        app.add_systems(
            OnExit(ActiveSimulation::Running),
            GuestSystem::new("sim_ex", vec![], cb_sim_exit, core::ptr::null_mut(), reg),
        );

        // Frame 1: Menu -> HudMode::Lobby, ActiveSimulation is absent.
        app.update();
        assert_eq!(
            *app.world().resource::<State<HudMode>>().get(),
            HudMode::Lobby
        );
        assert!(
            app.world()
                .get_resource::<State<ActiveSimulation>>()
                .is_none()
        );
        assert_eq!(HUD_ENTER_LOBBY.load(Ordering::SeqCst), 1);
        assert_eq!(HUD_INMATCH_TICKS.load(Ordering::SeqCst), 0);
        assert_eq!(SIM_ENTER.load(Ordering::SeqCst), 0);

        let hud_entry = *app
            .world()
            .get::<ModStateEntry>(hud_spec.state_entry_entity())
            .unwrap();
        assert!(!hud_entry.is_mutable());
        assert!(hud_entry.is_active());
        assert_ne!(hud_entry.flags & MOD_STATE_FLAG_COMPUTED, 0);
        assert_eq!(hud_entry.active_discriminant(), Some(0));

        // Manually sending a forged `ModStateTransitionRequest` targeting a ComputedState is ignored!
        app.world_mut()
            .resource_mut::<Messages<ModStateTransitionRequest>>()
            .write(ModStateTransitionRequest::new(hud_spec.handle(), 1));
        app.update();
        assert_eq!(
            *app.world().resource::<State<HudMode>>().get(),
            HudMode::Lobby
        );

        // Spawn Lobby-exit scoped entity and transition Menu -> InGame.
        let lobby_scoped = app
            .world_mut()
            .spawn((ScopedMarker, hud_spec.despawn_on_exit_marker(0).unwrap()))
            .id();
        assert_eq!(
            ModStateSlab::request_transition(app.world_mut(), root_spec.handle(), 1),
            FfiResult::OK
        );
        app.update();
        assert_eq!(
            *app.world().resource::<State<HudMode>>().get(),
            HudMode::InMatch
        );
        assert_eq!(
            *app.world().resource::<State<ActiveSimulation>>().get(),
            ActiveSimulation::Running
        );
        assert_eq!(HUD_EXIT_LOBBY.load(Ordering::SeqCst), 1);
        assert_eq!(HUD_TRANS_L_TO_M.load(Ordering::SeqCst), 1);
        assert_eq!(HUD_ENTER_INMATCH.load(Ordering::SeqCst), 1);
        assert_eq!(HUD_INMATCH_TICKS.load(Ordering::SeqCst), 1);
        assert_eq!(SIM_ENTER.load(Ordering::SeqCst), 1);
        assert!(app.world().get_entity(lobby_scoped).is_err());

        // Transition InGame -> Paused: `HudMode` remains `InMatch`, while `ActiveSimulation` exits!
        assert_eq!(
            ModStateSlab::request_transition(app.world_mut(), root_spec.handle(), 2),
            FfiResult::OK
        );
        app.update();
        assert_eq!(
            *app.world().resource::<State<HudMode>>().get(),
            HudMode::InMatch
        );
        assert!(
            app.world()
                .get_resource::<State<ActiveSimulation>>()
                .is_none()
        );
        assert_eq!(SIM_EXIT.load(Ordering::SeqCst), 1);
        assert_eq!(
            app.world()
                .resource::<ModStateSlab>()
                .current_discriminant(sim_spec.handle()),
            Some(None)
        );
        assert!(
            !app.world()
                .get::<ModStateEntry>(sim_spec.state_entry_entity())
                .unwrap()
                .is_active()
        );
    }
}

#[cfg(feature = "bridge-std-types")]
mod disabled_filter_tests {
    use std::sync::{Arc, Mutex};

    use bevy_abi_mod_bridges::std_types::{CANONICAL_DISABLED_NAME, StdTypesBridgePlugin};
    use bevy_abi_mod_host::prelude::*;
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, host_iter_column_readonly, host_iter_entities,
        host_iter_next_chunk, host_sys_query,
    };
    use bevy_abi_mod_sys::{AccessMode, BamIter, BamSystemCtx, BamTermDesc, FfiBool, FfiResult};
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity_disabling::Disabled;
    use bevy_ecs::system::System as BevySystem;
    use core::ffi::c_void;

    #[repr(C)]
    #[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
    #[mod_component(name = "disabled_test::EntityTag")]
    struct EntityTag {
        id: u32,
    }

    #[derive(Default)]
    struct TagCollector {
        seen: Mutex<Vec<u32>>,
    }

    #[derive(Default)]
    struct HasCollector {
        seen: Mutex<Vec<(u32, bool)>>,
    }

    unsafe extern "C" fn collect_tags_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to a live `TagCollector` owned by the test harness.
        let collector = unsafe { &*user_data.cast::<TagCollector>() };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        // SAFETY: Query 0 is registered on `sys_ctx`.
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }
        let mut len = 0usize;
        let mut out = collector.seen.lock().unwrap();
        // SAFETY: `iter` is valid for the duration of this system callback.
        while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
            #[allow(clippy::cast_ptr_alignment)]
            // SAFETY: Column 0 is `Read<EntityTag>`.
            let tags_ptr = unsafe { host_iter_column_readonly(iter, 0) }.cast::<EntityTag>();
            // SAFETY: `iter` holds an active chunk whose entity slice is valid for `len` items.
            let ents_ptr = unsafe { host_iter_entities(iter) };
            if tags_ptr.is_null() || ents_ptr.is_null() {
                return FfiResult::NULL_POINTER;
            }
            for i in 0..len {
                // SAFETY: `i < len` within the active chunk slice.
                let tag = unsafe { core::ptr::read_unaligned(tags_ptr.add(i)) };
                out.push(tag.id);
            }
        }
        FfiResult::OK
    }

    unsafe extern "C" fn collect_tags_with_has_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to a live `HasCollector` owned by the test harness.
        let collector = unsafe { &*user_data.cast::<HasCollector>() };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        // SAFETY: Query 0 is registered on `sys_ctx`.
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }
        let mut len = 0usize;
        let mut out = collector.seen.lock().unwrap();
        // SAFETY: `iter` is valid for the duration of this system callback.
        while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
            #[allow(clippy::cast_ptr_alignment)]
            // SAFETY: Column 0 is `Read<EntityTag>`.
            let tags_ptr = unsafe { host_iter_column_readonly(iter, 0) }.cast::<EntityTag>();
            // SAFETY: Column 1 is `Has<Disabled>` (`FfiBool`).
            let has_ptr = unsafe { host_iter_column_readonly(iter, 1) }.cast::<FfiBool>();
            if tags_ptr.is_null() || has_ptr.is_null() {
                return FfiResult::NULL_POINTER;
            }
            for i in 0..len {
                // SAFETY: `i < len` within the active chunk slice.
                let tag = unsafe { core::ptr::read_unaligned(tags_ptr.add(i)) };
                // SAFETY: `i < len` within synthesized `FfiBool` column for `Has<Disabled>`.
                let is_disabled = unsafe { *has_ptr.add(i) }.get();
                out.push((tag.id, is_disabled));
            }
        }
        FfiResult::OK
    }

    #[test]
    fn std_types_disabled_bridge_filters_guest_queries_with_default_allow_with_without_has() {
        let mut app = App::new();
        app.add_plugins((AbiModPlugin, StdTypesBridgePlugin));

        let tag_h = app
            .register_mod_component::<EntityTag>(ComponentOptions::new().with_default())
            .expect("register EntityTag");

        // Resolve `Disabled` via both canonical name and short alias from `StdTypesBridgePlugin`.
        let (disabled_h, disabled_inserter) = {
            let reg = app.world().resource::<ModRegistry>();
            let by_canonical = reg
                .get_by_name(CANONICAL_DISABLED_NAME)
                .expect("Disabled registered under CANONICAL_DISABLED_NAME");
            let by_alias = reg
                .get_by_name("Disabled")
                .expect("Disabled registered under short alias");
            assert_eq!(by_canonical.handle, by_alias.handle);
            assert_eq!(by_canonical.size, 0);
            assert_eq!(by_canonical.align, 1);
            assert!(by_canonical.is_immutable());
            (
                by_canonical.handle,
                by_canonical
                    .insert_default_fn
                    .expect("Disabled exposes default inserter"),
            )
        };

        // Spawn 3 enabled entities and 2 Disabled entities (one via host `Disabled`, one via
        // bridged `insert_default_fn` matching guest `cmd_insert_default`).
        app.world_mut().spawn(EntityTag { id: 1 });
        app.world_mut().spawn(EntityTag { id: 2 });
        app.world_mut().spawn(EntityTag { id: 3 });
        app.world_mut().spawn((EntityTag { id: 10 }, Disabled));
        let e20 = app.world_mut().spawn(EntityTag { id: 20 }).id();
        disabled_inserter(app.world_mut(), e20);
        assert!(app.world().get::<Disabled>(e20).is_some());

        let reg = app.world().resource::<ModRegistry>().clone();

        let default_seen = Arc::new(TagCollector::default());
        let allow_seen = Arc::new(TagCollector::default());
        let with_seen = Arc::new(TagCollector::default());
        let without_seen = Arc::new(TagCollector::default());
        let has_seen = Arc::new(HasCollector::default());

        app.add_systems(
            Update,
            (
                GuestSystem::new(
                    "default_filter",
                    vec![BamQuerySpecOwned::new(vec![BamTermDesc::read(tag_h)])],
                    collect_tags_cb,
                    Arc::as_ptr(&default_seen) as *mut c_void,
                    reg.clone(),
                ),
                GuestSystem::new(
                    "allow_disabled",
                    vec![BamQuerySpecOwned::new(vec![
                        BamTermDesc::read(tag_h),
                        BamTermDesc::new(disabled_h, AccessMode::ALLOW),
                    ])],
                    collect_tags_cb,
                    Arc::as_ptr(&allow_seen) as *mut c_void,
                    reg.clone(),
                ),
                GuestSystem::new(
                    "with_disabled",
                    vec![BamQuerySpecOwned::new(vec![
                        BamTermDesc::read(tag_h),
                        BamTermDesc::new(disabled_h, AccessMode::FILTER_ONLY),
                    ])],
                    collect_tags_cb,
                    Arc::as_ptr(&with_seen) as *mut c_void,
                    reg.clone(),
                ),
                GuestSystem::new(
                    "without_disabled",
                    vec![BamQuerySpecOwned::new(vec![
                        BamTermDesc::read(tag_h),
                        BamTermDesc::new(disabled_h, AccessMode::WITHOUT),
                    ])],
                    collect_tags_cb,
                    Arc::as_ptr(&without_seen) as *mut c_void,
                    reg.clone(),
                ),
                GuestSystem::new(
                    "has_disabled",
                    vec![BamQuerySpecOwned::new(vec![
                        BamTermDesc::read(tag_h),
                        BamTermDesc::new(disabled_h, AccessMode::HAS),
                    ])],
                    collect_tags_with_has_cb,
                    Arc::as_ptr(&has_seen) as *mut c_void,
                    reg.clone(),
                ),
            ),
        );

        app.update();

        let mut def = default_seen.seen.lock().unwrap().clone();
        def.sort_unstable();
        assert_eq!(def, vec![1, 2, 3], "DefaultQueryFilters hides Disabled");

        let mut allow = allow_seen.seen.lock().unwrap().clone();
        allow.sort_unstable();
        assert_eq!(
            allow,
            vec![1, 2, 3, 10, 20],
            "Allow<Disabled> sees both enabled and Disabled entities"
        );

        let mut with_d = with_seen.seen.lock().unwrap().clone();
        with_d.sort_unstable();
        assert_eq!(with_d, vec![10, 20], "With<Disabled> sees only Disabled");

        let mut without_d = without_seen.seen.lock().unwrap().clone();
        without_d.sort_unstable();
        assert_eq!(
            without_d,
            vec![1, 2, 3],
            "Without<Disabled> sees only enabled"
        );

        let mut has_d = has_seen.seen.lock().unwrap().clone();
        has_d.sort_unstable();
        assert_eq!(
            has_d,
            vec![(1, false), (2, false), (3, false), (10, true), (20, true)],
            "Has<Disabled> sees all entities with accurate boolean flags"
        );

        // Mutable write access to immutable ZST `Disabled` is rejected at system initialization.
        let mut bad_sys = GuestSystem::new(
            "bad_write_disabled",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                disabled_h,
                AccessMode::WRITE,
            )])],
            collect_tags_cb,
            Arc::as_ptr(&default_seen) as *mut c_void,
            reg,
        );
        let _ = bad_sys.initialize(app.world_mut());
        assert!(
            matches!(
                bad_sys.init_error(),
                Some(SystemInitError::ImmutableWrite(_))
            ),
            "expected ImmutableWrite error for mutable Disabled query"
        );
    }
}

#[cfg(feature = "bridge-asset")]
mod asset_overlay_tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use bevy_abi_mod_bridges::asset::{
        AssetOverlayPolicy, ModAssetAppExt, ModAssetPlugin, ModAssetRegistry,
    };
    use bevy_abi_mod_host::{
        AbiModPlugin, LoadOptions, ModDependencyGraph, ModId, ModRegistry, ModUnloadHooks,
    };
    use bevy_abi_mod_sys::{FfiAssetLoadState, FfiResult};
    use bevy_app::{App, TaskPoolPlugin};
    use bevy_asset::io::Reader;
    use bevy_asset::{
        Asset, AssetApp, AssetIndex, AssetLoader, AssetMetaCheck, AssetPlugin, AssetServer, Assets,
        Handle, LoadContext,
    };
    use bevy_reflect::TypePath;

    static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bam_overlay_test_{label}_{}_{}",
                std::process::id(),
                seq
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp overlay dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Asset, TypePath, Debug, Clone, PartialEq, Eq)]
    struct TextAsset(String);

    #[derive(Default, TypePath)]
    struct TextAssetLoader;

    impl AssetLoader for TextAssetLoader {
        type Asset = TextAsset;
        type Settings = ();
        type Error = std::io::Error;

        async fn load(
            &self,
            reader: &mut dyn Reader,
            _settings: &(),
            _load_context: &mut LoadContext<'_>,
        ) -> Result<Self::Asset, Self::Error> {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await?;
            Ok(TextAsset(String::from_utf8_lossy(&bytes).into_owned()))
        }

        fn extensions(&self) -> &[&str] {
            &["txt"]
        }
    }

    fn wait_for_text(app: &mut App, handle: &Handle<TextAsset>, expected: &str) {
        for _ in 0..80 {
            app.update();
            if let Some(asset) = app.world().resource::<Assets<TextAsset>>().get(handle)
                && asset.0 == expected
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let actual = app
            .world()
            .resource::<Assets<TextAsset>>()
            .get(handle)
            .map(|a| a.0.clone());
        panic!("timed out waiting for overlay asset '{expected}', got {actual:?}");
    }

    #[test]
    fn topological_overlay_policy_3_mod_priority_fallback_and_unload_revert() {
        let default_dir = TempDir::new("topo_default");
        let base_dir = TempDir::new("topo_base");
        let mod_a_dir = TempDir::new("topo_mod_a");
        let mod_b_dir = TempDir::new("topo_mod_b");
        let mod_c_dir = TempDir::new("topo_mod_c");

        for d in [&base_dir, &mod_a_dir, &mod_b_dir, &mod_c_dir] {
            fs::create_dir_all(d.path.join("shared")).unwrap();
        }
        fs::write(base_dir.path.join("shared/theme.txt"), b"base").unwrap();
        fs::write(base_dir.path.join("shared/base_only.txt"), b"only-in-base").unwrap();
        fs::write(mod_a_dir.path.join("shared/theme.txt"), b"from-a").unwrap();
        fs::write(mod_a_dir.path.join("shared/a_only.txt"), b"only-in-a").unwrap();
        fs::write(mod_b_dir.path.join("shared/theme.txt"), b"from-b").unwrap();
        fs::write(mod_b_dir.path.join("shared/ab_shared.txt"), b"ab-from-b").unwrap();
        fs::write(mod_c_dir.path.join("shared/theme.txt"), b"from-c").unwrap();

        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            AbiModPlugin,
            ModAssetPlugin::with_overlay_policy(AssetOverlayPolicy::Disabled)
                .with_base_dir(&base_dir.path),
            AssetPlugin {
                file_path: default_dir.path.to_string_lossy().into_owned(),
                meta_check: AssetMetaCheck::Never,
                ..Default::default()
            },
        ));
        app.init_asset::<TextAsset>();
        app.register_asset_loader(TextAssetLoader);
        app.finish();

        // Mount in deliberately scrambled order (`mod_c`, `mod_a`, `mod_b`) so mount order differs
        // from topological order (`mod_a` <- `mod_b` <- `mod_c`), proving `ModDependencyGraph`
        // drives `TopologicalOverlay`!
        let id_a = ModId(1);
        let id_b = ModId(2);
        let id_c = ModId(3);
        app.mount_mod_asset_dir_for_mod(id_c, "mod_c", &mod_c_dir.path)
            .unwrap();
        app.mount_mod_asset_dir_for_mod(id_a, "mod_a", &mod_a_dir.path)
            .unwrap();
        app.mount_mod_asset_dir_for_mod(id_b, "mod_b", &mod_b_dir.path)
            .unwrap();

        {
            let mut dag = app.world_mut().resource_mut::<ModDependencyGraph>();
            dag.register_loaded_mod(
                id_a,
                "mod_a".to_string(),
                PathBuf::from("libmod_a.dylib"),
                LoadOptions::default(),
                vec![],
            );
            dag.register_loaded_mod(
                id_b,
                "mod_b".to_string(),
                PathBuf::from("libmod_b.dylib"),
                LoadOptions::default(),
                vec!["mod_a".to_string()],
            );
            dag.register_loaded_mod(
                id_c,
                "mod_c".to_string(),
                PathBuf::from("libmod_c.dylib"),
                LoadOptions::default(),
                vec!["mod_b".to_string()],
            );
        }

        let text_bam = app.register_mod_asset::<TextAsset>().unwrap();
        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let backend = snap.asset_backend().unwrap().clone();

        // 1. When `AssetOverlayPolicy::Disabled`, overlay access is strictly rejected.
        assert_eq!(
            backend
                .load_asset(
                    &snap,
                    Some("mod_a"),
                    text_bam,
                    "mod://overlay/shared/theme.txt"
                )
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );

        // 2. Opt into `TopologicalOverlay` and sync from `ModDependencyGraph`.
        app.set_asset_overlay_policy(AssetOverlayPolicy::TopologicalOverlay);
        app.sync_asset_overlay_order();
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .active_overlay_order(),
            vec!["mod_c", "mod_b", "mod_a"]
        );
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .resolve_overlay_file("shared/theme.txt")
                .unwrap()
                .0
                .as_deref(),
            Some("mod_c")
        );
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .resolve_overlay_file("shared/ab_shared.txt")
                .unwrap()
                .0
                .as_deref(),
            Some("mod_b")
        );
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .resolve_overlay_file("shared/a_only.txt")
                .unwrap()
                .0
                .as_deref(),
            Some("mod_a")
        );
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .resolve_overlay_file("shared/base_only.txt")
                .unwrap()
                .0,
            None
        );

        // Load overlay assets via both Bevy `AssetServer` and guest FFI `HostAssetBackend`.
        let server = app.world().resource::<AssetServer>().clone();
        let h_theme: Handle<TextAsset> = server.load("mod://overlay/shared/theme.txt");
        let h_ab: Handle<TextAsset> = server.load("mod://overlay/shared/ab_shared.txt");
        let h_a_only: Handle<TextAsset> = server.load("mod://overlay/shared/a_only.txt");
        let h_base_only: Handle<TextAsset> = server.load("mod://overlay/shared/base_only.txt");

        let guest_ffi_h = backend
            .load_asset(
                &snap,
                Some("mod_a"),
                text_bam,
                "mod://overlay/shared/theme.txt",
            )
            .expect("guest can load mod://overlay/ when policy enabled");

        wait_for_text(&mut app, &h_theme, "from-c");
        wait_for_text(&mut app, &h_ab, "ab-from-b");
        wait_for_text(&mut app, &h_a_only, "only-in-a");
        wait_for_text(&mut app, &h_base_only, "only-in-base");

        // Verify guest FFI handle resolves to the exact same overridden asset `"from-c"`.
        assert_eq!(
            backend.get_load_state(&snap, &guest_ffi_h).unwrap(),
            FfiAssetLoadState::LOADED
        );
        let guest_id = bevy_asset::AssetId::<TextAsset>::from(AssetIndex::from_bits(
            guest_ffi_h.to_index_bits().unwrap(),
        ));
        assert_eq!(
            app.world()
                .resource::<Assets<TextAsset>>()
                .get(guest_id)
                .unwrap()
                .0,
            "from-c"
        );

        // 3. Unload `mod_c` (simulating `ModUnloadHooks` + DAG unregister) -> automatically reverts
        //    live `h_theme` to `mod_b` ("from-b")!
        assert!(!app.world().resource::<ModUnloadHooks>().is_empty());
        {
            let reg = app.world().resource::<ModAssetRegistry>().clone();
            let dag = app.world().resource::<ModDependencyGraph>();
            let unloaded = reg.handle_mod_unloaded(id_c, None, Some(dag), None);
            assert_eq!(unloaded.as_deref(), Some("mod_c"));
        }
        app.world_mut()
            .resource_mut::<ModDependencyGraph>()
            .unregister_mod(id_c);
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .active_overlay_order(),
            vec!["mod_b", "mod_a"]
        );
        wait_for_text(&mut app, &h_theme, "from-b");

        // 4. Unload `mod_b` -> automatically reverts live `h_theme` to `mod_a` ("from-a")!
        {
            let reg = app.world().resource::<ModAssetRegistry>().clone();
            let dag = app.world().resource::<ModDependencyGraph>();
            let unloaded = reg.handle_mod_unloaded(id_b, None, Some(dag), None);
            assert_eq!(unloaded.as_deref(), Some("mod_b"));
        }
        app.world_mut()
            .resource_mut::<ModDependencyGraph>()
            .unregister_mod(id_b);
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .active_overlay_order(),
            vec!["mod_a"]
        );
        wait_for_text(&mut app, &h_theme, "from-a");

        // 5. Unload `mod_a` -> automatically reverts live `h_theme` to `base_dir` ("base")!
        {
            let reg = app.world().resource::<ModAssetRegistry>().clone();
            let dag = app.world().resource::<ModDependencyGraph>();
            let unloaded = reg.handle_mod_unloaded(id_a, None, Some(dag), None);
            assert_eq!(unloaded.as_deref(), Some("mod_a"));
        }
        app.world_mut()
            .resource_mut::<ModDependencyGraph>()
            .unregister_mod(id_a);
        assert!(
            app.world()
                .resource::<ModAssetRegistry>()
                .active_overlay_order()
                .is_empty()
        );
        wait_for_text(&mut app, &h_theme, "base");
    }

    #[test]
    fn explicit_priority_load_order_and_2_mod_overlay_sandboxing() {
        let default_dir = TempDir::new("exp_default");
        let mod_a_dir = TempDir::new("exp_mod_a");
        let mod_b_dir = TempDir::new("exp_mod_b");
        let outside_dir = TempDir::new("exp_outside");

        fs::write(mod_a_dir.path.join("banner.txt"), b"banner-a").unwrap();
        fs::write(mod_b_dir.path.join("banner.txt"), b"banner-b").unwrap();
        fs::write(outside_dir.path.join("secret.txt"), b"secret").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside_dir.path.join("secret.txt"),
            mod_b_dir.path.join("escape.txt"),
        )
        .unwrap();

        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            AbiModPlugin,
            ModAssetPlugin::with_overlay_policy(AssetOverlayPolicy::LoadOrderPriority),
            AssetPlugin {
                file_path: default_dir.path.to_string_lossy().into_owned(),
                meta_check: AssetMetaCheck::Never,
                ..Default::default()
            },
        ));
        app.init_asset::<TextAsset>();
        app.register_asset_loader(TextAssetLoader);
        app.finish();

        // Mount `mod_a` first, then `mod_b`: under `LoadOrderPriority`, `mod_b` wins.
        app.mount_mod_asset_dir("mod_a", &mod_a_dir.path).unwrap();
        app.mount_mod_asset_dir("mod_b", &mod_b_dir.path).unwrap();
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .active_overlay_order(),
            vec!["mod_b", "mod_a"]
        );

        let server = app.world().resource::<AssetServer>().clone();
        let h_banner: Handle<TextAsset> = server.load("mod://overlay/banner.txt");
        wait_for_text(&mut app, &h_banner, "banner-b");

        // Switch to `ExplicitPriority(["mod_a", "mod_b"])`: `mod_a` now overrides `mod_b` live!
        app.set_asset_overlay_policy(AssetOverlayPolicy::explicit_priority(["mod_a", "mod_b"]));
        assert_eq!(
            app.world()
                .resource::<ModAssetRegistry>()
                .active_overlay_order(),
            vec!["mod_a", "mod_b"]
        );
        wait_for_text(&mut app, &h_banner, "banner-a");

        // Unmounting `mod_a` automatically falls back to `mod_b`!
        assert!(app.unmount_mod_asset_dir("mod_a"));
        wait_for_text(&mut app, &h_banner, "banner-b");

        // Path traversal and symlink escape remain strictly rejected under overlay policies!
        let text_bam = app.register_mod_asset::<TextAsset>().unwrap();
        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let backend = snap.asset_backend().unwrap().clone();
        assert_eq!(
            backend
                .load_asset(
                    &snap,
                    Some("mod_b"),
                    text_bam,
                    "mod://overlay/../secret.txt"
                )
                .unwrap_err()
                .0,
            FfiResult::PERMISSION_DENIED
        );

        #[cfg(unix)]
        {
            let err = app
                .world()
                .resource::<ModAssetRegistry>()
                .resolve_overlay_file("escape.txt")
                .unwrap_err();
            assert!(
                matches!(err, bevy_asset::io::AssetReaderError::Io(_)),
                "symlink escape in overlay layer must fail with PermissionDenied I/O error, got {err:?}"
            );
        }
    }
}
