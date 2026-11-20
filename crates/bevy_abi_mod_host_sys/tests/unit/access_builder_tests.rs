use super::*;
use crate::registry::ModRegistry;
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::BamTermDesc;
use bevy_ecs::message::{Message, Messages};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
struct ReloadPing {
    seq: u32,
}

impl Message for ReloadPing {}

#[test]
fn persistent_message_cursor_survives_query_group_reload() {
    let mut world = World::new();
    world.init_resource::<Messages<ReloadPing>>();
    let res_id = world
        .components()
        .component_id::<Messages<ReloadPing>>()
        .expect("Messages<ReloadPing> resource id");

    let mut registry = ModRegistry::new();
    let handle = registry
        .register_message::<ReloadPing>(res_id)
        .expect("register ReloadPing message");
    let snapshot = registry.snapshot();

    let term = BamTermDesc::new(handle, AccessMode::MESSAGE_READ);
    let spec = BamQuerySpecOwned::new(vec![term]);

    // Write 3 messages before initial load.
    {
        let mut msgs = world.resource_mut::<Messages<ReloadPing>>();
        msgs.write(ReloadPing { seq: 1 });
        msgs.write(ReloadPing { seq: 2 });
        msgs.write(ReloadPing { seq: 3 });
    }

    // First mod generation reads 2 of 3 messages (or all 3) and drops on unload.
    {
        let mut group = CachedQueryGroup::new(spec.clone());
        group.set_system_identity(Some(Arc::from("mod_a")), "tick_pings");
        let mut groups = [group];
        initialize_query_groups_in_world(&mut world, snapshot, &mut groups).expect("init gen 1");

        let cursor = groups[0].msg_cursors[0]
            .as_mut()
            .expect("cursor initialized")
            .downcast_mut::<MessageCursor<ReloadPing>>()
            .expect("MessageCursor<ReloadPing>");
        let messages = world.resource::<Messages<ReloadPing>>();
        let batch: Vec<u32> = cursor.read(messages).take(2).map(|m| m.seq).collect();
        assert_eq!(batch, vec![1, 2]);
    }

    // Write one more message while unloaded.
    world
        .resource_mut::<Messages<ReloadPing>>()
        .write(ReloadPing { seq: 4 });

    // Second mod generation with identical (mod_namespace, system_name, query_idx, term_idx, uuid)
    // resumes cleanly at seq=3 without re-reading seq=1,2!
    {
        let mut group = CachedQueryGroup::new(spec.clone());
        group.set_system_identity(Some(Arc::from("mod_a")), "tick_pings");
        let mut groups = [group];
        initialize_query_groups_in_world(&mut world, snapshot, &mut groups).expect("init gen 2");

        let cursor = groups[0].msg_cursors[0]
            .as_mut()
            .expect("cursor restored")
            .downcast_mut::<MessageCursor<ReloadPing>>()
            .expect("MessageCursor<ReloadPing>");
        let messages = world.resource::<Messages<ReloadPing>>();
        let batch: Vec<u32> = cursor.read(messages).map(|m| m.seq).collect();
        assert_eq!(batch, vec![3, 4]);
    }

    // Different system_name gets an independent fresh cursor starting from seq=1.
    {
        let mut other = CachedQueryGroup::new(spec);
        other.set_system_identity(Some(Arc::from("mod_a")), "other_system");
        let mut groups = [other];
        initialize_query_groups_in_world(&mut world, snapshot, &mut groups).expect("init other");

        let cursor = groups[0].msg_cursors[0]
            .as_mut()
            .expect("fresh cursor")
            .downcast_mut::<MessageCursor<ReloadPing>>()
            .expect("MessageCursor<ReloadPing>");
        let messages = world.resource::<Messages<ReloadPing>>();
        let batch: Vec<u32> = cursor.read(messages).map(|m| m.seq).collect();
        assert_eq!(batch, vec![1, 2, 3, 4]);
    }
}

#[test]
fn persistent_removed_cursor_survives_query_group_reload() {
    #[repr(C)]
    #[derive(bevy_ecs::component::Component, Clone, Copy, ModComponent)]
    struct EphemeralTag;

    let mut world = World::new();
    let cid = world.register_component::<EphemeralTag>();
    let mut registry = ModRegistry::new();
    let handle = registry
        .register_component::<EphemeralTag>(cid, Default::default())
        .expect("register EphemeralTag");
    let snapshot = registry.snapshot();

    let e1 = world.spawn(EphemeralTag).id();
    let e2 = world.spawn(EphemeralTag).id();
    let e3 = world.spawn(EphemeralTag).id();
    world.despawn(e1);
    world.despawn(e2);

    let term = BamTermDesc::new(handle, AccessMode::REMOVED_READ);
    let spec = BamQuerySpecOwned::new(vec![term]);

    {
        let mut group = CachedQueryGroup::new(spec.clone());
        group.set_system_identity(Some(Arc::from("mod_b")), "watch_removals");
        let mut groups = [group];
        initialize_query_groups_in_world(&mut world, snapshot, &mut groups)
            .expect("init removed gen 1");

        let cursor = groups[0].msg_cursors[0]
            .as_mut()
            .expect("removed cursor initialized")
            .downcast_mut::<MessageCursor<RemovedComponentEntity>>()
            .expect("MessageCursor<RemovedComponentEntity>");
        let removed = world.removed_components().get(cid).expect("removed stream");
        let first: Vec<bevy_ecs::entity::Entity> = cursor
            .read(removed)
            .take(1)
            .map(|ev| ev.clone().into())
            .collect();
        assert_eq!(first, vec![e1]);
    }

    world.despawn(e3);

    {
        let mut group = CachedQueryGroup::new(spec);
        group.set_system_identity(Some(Arc::from("mod_b")), "watch_removals");
        let mut groups = [group];
        initialize_query_groups_in_world(&mut world, snapshot, &mut groups)
            .expect("init removed gen 2");

        let cursor = groups[0].msg_cursors[0]
            .as_mut()
            .expect("removed cursor restored")
            .downcast_mut::<MessageCursor<RemovedComponentEntity>>()
            .expect("MessageCursor<RemovedComponentEntity>");
        let removed = world.removed_components().get(cid).expect("removed stream");
        let rest: Vec<bevy_ecs::entity::Entity> =
            cursor.read(removed).map(|ev| ev.clone().into()).collect();
        assert_eq!(rest, vec![e2, e3]);
    }
}

#[repr(C)]
#[derive(bevy_ecs::component::Component, Clone, Copy, ModComponent)]
struct AccCompA(u32);

#[repr(C)]
#[derive(bevy_ecs::component::Component, Clone, Copy, ModComponent)]
#[component(immutable)]
struct AccImm(u32);

#[repr(C)]
#[derive(bevy_ecs::component::Component, Clone, Copy, ModComponent)]
#[component(storage = "SparseSet")]
struct AccSparse(u32);

#[repr(C)]
#[derive(bevy_ecs::prelude::Resource, Clone, Copy, ModComponent)]
struct AccRes(u32);

#[test]
fn access_builder_validation_conflicts_or_groups_and_find_conflicting_name() {
    use bevy_abi_mod_sys::{BamComponent, FfiResult, ModComponent as _};

    let mut world = World::new();
    let a_cid = world.register_component::<AccCompA>();
    let imm_cid = world.register_component::<AccImm>();
    let sp_cid = world.register_component::<AccSparse>();
    let res_cid = world.register_component::<AccRes>();

    let mut reg = ModRegistry::new();
    let a_h = reg
        .register_component::<AccCompA>(a_cid, Default::default())
        .unwrap();
    let imm_h = reg
        .register_component::<AccImm>(
            imm_cid,
            crate::registry::ComponentOptions::default().immutable(),
        )
        .unwrap();
    let sp_h = reg
        .register_component::<AccSparse>(sp_cid, Default::default())
        .unwrap();
    let res_h = reg.register_resource::<AccRes>(res_cid).unwrap();
    let snap = reg.snapshot();

    // 1. `find_conflicting_name` `"unknown"` fallback when `id_to_name` lacks ID.
    let mut set_a = FilteredAccessSet::new();
    let mut fa_a = bevy_ecs::query::FilteredAccess::default();
    fa_a.add_write(a_cid);
    set_a.add(fa_a.clone());
    let empty_names = HashMap::new();
    assert_eq!(
        find_conflicting_name(&empty_names, &set_a, &fa_a),
        "unknown"
    );

    // 2. `validate_system_queries_against_snapshot` error cases:
    // > 32 terms
    let too_many = vec![BamTermDesc::new(a_h, AccessMode::READ); MAX_TERMS_PER_QUERY + 1];
    assert_eq!(
        validate_system_queries_against_snapshot(snap, &[BamQuerySpecOwned::new(too_many)])
            .unwrap_err()
            .0,
        FfiResult::CAPACITY_EXCEEDED
    );
    // Invalid mode
    let mut bad_mode = BamTermDesc::new(a_h, AccessMode::READ);
    bad_mode.mode = AccessMode(255);
    assert_eq!(
        validate_system_queries_against_snapshot(snap, &[BamQuerySpecOwned::new(vec![bad_mode])])
            .unwrap_err()
            .0,
        FfiResult::INVALID_ARGUMENT
    );
    // Unknown component
    assert_eq!(
        validate_system_queries_against_snapshot(
            snap,
            &[BamQuerySpecOwned::new(vec![BamTermDesc::new(
                BamComponent::from_raw(999),
                AccessMode::READ
            )])]
        )
        .unwrap_err()
        .0,
        FfiResult::INVALID_COMPONENT
    );
    // Kind mismatch
    assert_eq!(
        validate_system_queries_against_snapshot(
            snap,
            &[BamQuerySpecOwned::new(vec![BamTermDesc::new(
                a_h,
                AccessMode::RESOURCE_READ
            )])]
        )
        .unwrap_err()
        .0,
        FfiResult::INVALID_COMPONENT
    );
    // Immutable write
    assert_eq!(
        validate_system_queries_against_snapshot(
            snap,
            &[BamQuerySpecOwned::new(vec![BamTermDesc::new(
                imm_h,
                AccessMode::WRITE
            )])]
        )
        .unwrap_err()
        .0,
        FfiResult::PERMISSION_DENIED
    );
    // Invalid `or_group` mode
    let mut bad_or = BamTermDesc::new(a_h, AccessMode::READ);
    bad_or.or_group = 1;
    assert_eq!(
        validate_system_queries_against_snapshot(snap, &[BamQuerySpecOwned::new(vec![bad_or])])
            .unwrap_err()
            .0,
        FfiResult::INVALID_ARGUMENT
    );
    // Intra-query read/write & write/write conflicts
    assert_eq!(
        validate_system_queries_against_snapshot(
            snap,
            &[BamQuerySpecOwned::new(vec![
                BamTermDesc::new(a_h, AccessMode::WRITE),
                BamTermDesc::new(a_h, AccessMode::READ),
            ])]
        )
        .unwrap_err()
        .0,
        FfiResult::ACCESS_CONFLICT
    );
    assert_eq!(
        validate_system_queries_against_snapshot(
            snap,
            &[BamQuerySpecOwned::new(vec![
                BamTermDesc::new(a_h, AccessMode::READ),
                BamTermDesc::new(a_h, AccessMode::WRITE),
            ])]
        )
        .unwrap_err()
        .0,
        FfiResult::ACCESS_CONFLICT
    );
    // Resource conflicts (write->read and read->write)
    assert_eq!(
        validate_system_queries_against_snapshot(
            snap,
            &[BamQuerySpecOwned::new(vec![
                BamTermDesc::new(res_h, AccessMode::RESOURCE_WRITE),
                BamTermDesc::new(res_h, AccessMode::RESOURCE_READ),
            ])]
        )
        .unwrap_err()
        .0,
        FfiResult::ACCESS_CONFLICT
    );
    assert_eq!(
        validate_system_queries_against_snapshot(
            snap,
            &[BamQuerySpecOwned::new(vec![
                BamTermDesc::new(res_h, AccessMode::RESOURCE_READ),
                BamTermDesc::new(res_h, AccessMode::RESOURCE_WRITE),
            ])]
        )
        .unwrap_err()
        .0,
        FfiResult::ACCESS_CONFLICT
    );
    // Inter-query borrow conflict
    assert_eq!(
        validate_system_queries_against_snapshot(
            snap,
            &[
                BamQuerySpecOwned::new(vec![BamTermDesc::new(a_h, AccessMode::WRITE)]),
                BamQuerySpecOwned::new(vec![BamTermDesc::new(a_h, AccessMode::OPTIONAL_READ)]),
            ]
        )
        .unwrap_err()
        .0,
        FfiResult::ACCESS_CONFLICT
    );

    // Valid complex combination covering `OPTIONAL_READ`, `OPTIONAL_WRITE`, `FILTER_ONLY`,
    // `WITHOUT`, `ALLOW`, `HAS`, `CHANGED`/`ADDED` (both inside and outside `or_group`,
    // with and without `WRITE`).
    let mut or_with = BamTermDesc::new(sp_h, AccessMode::FILTER_ONLY);
    or_with.or_group = 1;
    let mut or_without = BamTermDesc::new(imm_h, AccessMode::WITHOUT);
    or_without.or_group = 1;
    let mut or_changed_ro = BamTermDesc::new(imm_h, AccessMode::CHANGED);
    or_changed_ro.or_group = 2;
    let mut or_added_wr = BamTermDesc::new(a_h, AccessMode::ADDED);
    or_added_wr.or_group = 2;

    let valid_groups = [BamQuerySpecOwned::new(vec![
        BamTermDesc::new(a_h, AccessMode::WRITE),
        BamTermDesc::new(imm_h, AccessMode::OPTIONAL_READ),
        BamTermDesc::new(sp_h, AccessMode::ALLOW),
        BamTermDesc::new(res_h, AccessMode::RESOURCE_READ),
        or_with,
        or_without,
        or_changed_ro,
        or_added_wr,
    ])];
    assert!(validate_system_queries_against_snapshot(snap, &valid_groups).is_ok());

    let mut cached: Vec<CachedQueryGroup> = valid_groups
        .iter()
        .cloned()
        .map(CachedQueryGroup::new)
        .collect();
    let fas = initialize_query_groups_in_world(&mut world, snap, &mut cached).unwrap();
    assert!(fas.combined_access().has_read(res_cid));

    // 3. `initialize_query_groups_in_world` error variants:
    let err_cases: Vec<(Vec<BamQuerySpecOwned>, SystemInitError)> = vec![
        (
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(
                    a_h,
                    AccessMode::READ
                );
                MAX_TERMS_PER_QUERY + 1
            ])],
            SystemInitError::TooManyTerms,
        ),
        (
            vec![BamQuerySpecOwned::new(vec![bad_mode])],
            SystemInitError::InvalidMode { index: 0 },
        ),
        (
            vec![BamQuerySpecOwned::new(vec![bad_or])],
            SystemInitError::InvalidMode { index: 0 },
        ),
        (
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                BamComponent::from_raw(999),
                AccessMode::READ,
            )])],
            SystemInitError::UnknownComponent(String::from("BamComponent(999)")),
        ),
        (
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                a_h,
                AccessMode::RESOURCE_READ,
            )])],
            SystemInitError::KindMismatch(String::from(AccCompA::CANONICAL_NAME)),
        ),
        (
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                imm_h,
                AccessMode::WRITE,
            )])],
            SystemInitError::ImmutableWrite(String::from(AccImm::CANONICAL_NAME)),
        ),
        (
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(a_h, AccessMode::WRITE),
                BamTermDesc::new(a_h, AccessMode::READ),
            ])],
            SystemInitError::SelfConflict {
                a: String::from(AccCompA::CANONICAL_NAME),
                b: String::from(AccCompA::CANONICAL_NAME),
            },
        ),
        (
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(a_h, AccessMode::READ),
                BamTermDesc::new(a_h, AccessMode::WRITE),
            ])],
            SystemInitError::SelfConflict {
                a: String::from(AccCompA::CANONICAL_NAME),
                b: String::from(AccCompA::CANONICAL_NAME),
            },
        ),
        (
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(res_h, AccessMode::RESOURCE_WRITE),
                BamTermDesc::new(res_h, AccessMode::RESOURCE_READ),
            ])],
            SystemInitError::SelfConflict {
                a: String::from(AccRes::CANONICAL_NAME),
                b: String::from(AccRes::CANONICAL_NAME),
            },
        ),
        (
            vec![BamQuerySpecOwned::new(vec![
                BamTermDesc::new(res_h, AccessMode::RESOURCE_READ),
                BamTermDesc::new(res_h, AccessMode::RESOURCE_WRITE),
            ])],
            SystemInitError::SelfConflict {
                a: String::from(AccRes::CANONICAL_NAME),
                b: String::from(AccRes::CANONICAL_NAME),
            },
        ),
        (
            vec![
                BamQuerySpecOwned::new(vec![BamTermDesc::new(a_h, AccessMode::WRITE)]),
                BamQuerySpecOwned::new(vec![BamTermDesc::new(a_h, AccessMode::OPTIONAL_WRITE)]),
            ],
            SystemInitError::SelfConflict {
                a: String::from(AccCompA::CANONICAL_NAME),
                b: String::from(AccCompA::CANONICAL_NAME),
            },
        ),
    ];

    for (specs, expected) in err_cases {
        let mut groups: Vec<CachedQueryGroup> =
            specs.into_iter().map(CachedQueryGroup::new).collect();
        let got = initialize_query_groups_in_world(&mut world, snap, &mut groups).unwrap_err();
        assert_eq!(got, expected);
    }
}
