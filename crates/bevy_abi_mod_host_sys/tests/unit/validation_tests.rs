use super::*;
use crate::registry::{ComponentOptions, DynamicComponentReg, extract_owned_dynamic_fields};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{ComponentDesc, ComponentUuid};

fn register_dyn_for_test(
    reg: &mut ModRegistry,
    comp_id: ComponentId,
    desc: &ComponentDesc,
    name: &str,
) -> BamComponent {
    // SAFETY: Test descriptor has valid `fields` pointer when `field_count > 0`.
    let (schema_fingerprint, fields, owned_field_strings) =
        unsafe { extract_owned_dynamic_fields(desc) }
            .unwrap_or_else(|_| (0, std::sync::Arc::from([]), None));
    reg.register_dynamic_component(DynamicComponentReg {
        comp_id,
        desc,
        name,
        owner: None,
        drop_slot: None,
        required_components: Box::new([]),
        schema_fingerprint,
        fields,
        owned_field_strings,
    })
    .unwrap()
}
use bevy_ecs::component::Component;

#[derive(Component, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bridge_test::BioCard")]
struct BioCard {
    title: String,
    level: u32,
    active: bool,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bridge_test::TintUniform")]
struct TintUniform {
    rgba: [f32; 4],
}

#[test]
fn witness_field_introspection_export_apply_and_insert_work_under_miri() {
    let mut world = World::new();
    let cid = world.register_component::<BioCard>();
    let mut reg = ModRegistry::new();
    let handle = reg
        .register_component::<BioCard>(cid, ComponentOptions::new().with_default())
        .unwrap();
    world.insert_resource(reg);

    let fields_meta = inspect_component_witness_fields(&world, handle).unwrap();
    assert_eq!(fields_meta.len(), 3);
    assert_eq!(fields_meta[0].name, "title");
    assert_eq!(fields_meta[0].type_tag, "String");
    assert_eq!(fields_meta[1].name, "level");
    assert_eq!(fields_meta[1].type_tag, "u32");
    assert_eq!(fields_meta[2].name, "active");
    assert_eq!(fields_meta[2].type_tag, "bool");

    let ent = world.spawn_empty().id();
    let init_fields = vec![
        ExportedFieldRecord {
            field_name: "title".to_owned(),
            type_tag: "String".to_owned(),
            bytes: b"Alchemist".to_vec(),
        },
        ExportedFieldRecord {
            field_name: "level".to_owned(),
            type_tag: "u32".to_owned(),
            bytes: 42u32.to_ne_bytes().to_vec(),
        },
        ExportedFieldRecord {
            field_name: "active".to_owned(),
            type_tag: "bool".to_owned(),
            bytes: vec![1u8],
        },
    ];
    insert_or_apply_entity_component_fields(&mut world, ent, handle, &init_fields).unwrap();
    assert_eq!(
        world.entity(ent).get::<BioCard>().unwrap(),
        &BioCard {
            title: "Alchemist".to_owned(),
            level: 42,
            active: true,
        }
    );

    let exported = export_entity_component_fields(&world, ent, handle).unwrap();
    assert_eq!(exported.len(), 3);
    assert_eq!(exported[0].bytes, b"Alchemist");

    let patch = vec![ExportedFieldRecord {
        field_name: "level".to_owned(),
        type_tag: "u16".to_owned(), // lossless numeric widening u16 -> u32
        bytes: 99u16.to_ne_bytes().to_vec(),
    }];
    apply_entity_component_fields(&mut world, ent, handle, &patch).unwrap();
    assert_eq!(world.entity(ent).get::<BioCard>().unwrap().level, 99);
}

#[test]
fn cross_world_extract_copies_host_and_dynamic_pod_components_and_resources() {
    let mut main_world = World::new();
    let mut sub_world = World::new();

    let tint_cid = main_world.register_component::<TintUniform>();
    let mut reg = ModRegistry::new();
    let tint_h = reg
        .register_component::<TintUniform>(tint_cid, ComponentOptions::new().with_raw_insert())
        .unwrap();

    // Register a dynamic POD component and dynamic POD resource in `main_world`.
    let dyn_layout = Layout::from_size_align(8, 4).unwrap();
    // SAFETY: Valid POD layout with `drop = None`.
    let dyn_cid = main_world.register_component_with_descriptor(unsafe {
        ComponentDescriptor::new_with_layout(
            "bridge_test::DynUvScale".to_owned(),
            StorageType::Table,
            dyn_layout,
            None,
            true,
            false,
            ComponentCloneBehavior::Default,
            None,
        )
    });
    let dyn_desc = ComponentDesc::new(
        c"bridge_test::DynUvScale".as_ptr(),
        ComponentUuid::from_parts("bridge_test::DynUvScale", 8, 4, 0),
        8,
        4,
    );
    let dyn_h = register_dyn_for_test(&mut reg, dyn_cid, &dyn_desc, "bridge_test::DynUvScale");

    main_world.insert_resource(reg);

    let src_e = main_world
        .spawn(TintUniform {
            rgba: [0.25, 0.5, 0.75, 1.0],
        })
        .id();
    write_entity_pod_bytes(&mut main_world, src_e, dyn_h, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();

    let dst_e = sub_world.spawn_empty().id();
    assert!(
        extract_component_between_worlds(&main_world, &mut sub_world, src_e, dst_e, tint_h)
            .unwrap()
    );
    assert!(
        extract_component_between_worlds(&main_world, &mut sub_world, src_e, dst_e, dyn_h).unwrap()
    );

    assert_eq!(
        sub_world.entity(dst_e).get::<TintUniform>().unwrap().rgba,
        [0.25, 0.5, 0.75, 1.0]
    );
    let sub_dyn_h = sub_world
        .resource::<ModRegistry>()
        .get_by_name("bridge_test::DynUvScale")
        .unwrap()
        .handle;
    assert_eq!(
        read_entity_pod_bytes(&sub_world, dst_e, sub_dyn_h).unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7, 8]
    );
}

#[test]
fn bam_field_desc_validation_rejects_oob_misaligned_duplicate_and_size_mismatch() {
    use bevy_abi_mod_sys::{BamFieldDesc, BamFieldKind, StrView};

    fn register_with_fields(
        world: &mut World,
        reg: &mut ModRegistry,
        name: &'static str,
        c_name: &'static core::ffi::CStr,
        size: usize,
        align: usize,
        fields: &[BamFieldDesc],
    ) -> BamComponent {
        let layout = Layout::from_size_align(size, align).unwrap();
        // SAFETY: Valid POD layout with `drop = None`.
        let cid = world.register_component_with_descriptor(unsafe {
            ComponentDescriptor::new_with_layout(
                name.to_owned(),
                StorageType::Table,
                layout,
                None,
                true,
                false,
                ComponentCloneBehavior::Default,
                None,
            )
        });
        let desc = ComponentDesc::new(
            c_name.as_ptr(),
            ComponentUuid::from_parts(name, size, align, 0),
            size,
            align,
        )
        .with_fields(fields.as_ptr(), fields.len());
        register_dyn_for_test(reg, cid, &desc, name)
    }

    let mut world = World::new();
    let mut reg = ModRegistry::new();

    // 1. Out-of-bounds offset (`offset + size > entry.size`)
    let oob_fields = [BamFieldDesc::new(
        BamFieldKind::U32,
        StrView::from_str("oob"),
        StrView::from_str("u32"),
        8,
        4,
        4,
    )];
    let h_oob = register_with_fields(
        &mut world,
        &mut reg,
        "val_test::Oob",
        c"val_test::Oob",
        8,
        4,
        &oob_fields,
    );
    assert_eq!(
        inspect_component_bam_fields_in_registry(&reg, h_oob),
        Err(FfiResult::OUT_OF_RANGE)
    );

    // 2. Misaligned offset (`offset % align != 0`)
    let misalign_fields = [BamFieldDesc::new(
        BamFieldKind::U32,
        StrView::from_str("bad_off"),
        StrView::from_str("u32"),
        2,
        4,
        4,
    )];
    let h_misaligned = register_with_fields(
        &mut world,
        &mut reg,
        "val_test::Misaligned",
        c"val_test::Misaligned",
        8,
        4,
        &misalign_fields,
    );
    assert_eq!(
        inspect_component_bam_fields_in_registry(&reg, h_misaligned),
        Err(FfiResult::INVALID_ARGUMENT)
    );

    // 3. Primitive size mismatch (`U32` with size 2)
    let bad_size_fields = [BamFieldDesc::new(
        BamFieldKind::U32,
        StrView::from_str("short_u32"),
        StrView::from_str("u32"),
        0,
        2,
        2,
    )];
    let h_sz = register_with_fields(
        &mut world,
        &mut reg,
        "val_test::BadSize",
        c"val_test::BadSize",
        8,
        4,
        &bad_size_fields,
    );
    assert_eq!(
        inspect_component_bam_fields_in_registry(&reg, h_sz),
        Err(FfiResult::INVALID_ARGUMENT)
    );

    // 4. Duplicate field names
    let dup_fields = [
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("x"),
            StrView::from_str("f32"),
            0,
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("x"),
            StrView::from_str("f32"),
            4,
            4,
            4,
        ),
    ];
    let h_dup = register_with_fields(
        &mut world,
        &mut reg,
        "val_test::Dup",
        c"val_test::Dup",
        8,
        4,
        &dup_fields,
    );
    assert_eq!(
        inspect_component_bam_fields_in_registry(&reg, h_dup),
        Err(FfiResult::INVALID_ARGUMENT)
    );

    // 5. Valid descriptor with padding + Vec3 + Entity + Bool + OPAQUE
    let valid_fields = [
        BamFieldDesc::new(
            BamFieldKind::U8,
            StrView::from_str("tag"),
            StrView::from_str("u8"),
            0,
            1,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("score"),
            StrView::from_str("f32"),
            4,
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::BOOL,
            StrView::from_str("active"),
            StrView::from_str("bool"),
            8,
            1,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("vel"),
            StrView::from_str("Vec3"),
            12,
            12,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U64,
            StrView::from_str("target"),
            StrView::from_str("Entity"),
            24,
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::OPAQUE,
            StrView::from_str("reserved"),
            StrView::from_str("Opaque"),
            32,
            8,
            8,
        ),
    ];
    let h_ok = register_with_fields(
        &mut world,
        &mut reg,
        "val_test::Valid",
        c"val_test::Valid",
        40,
        8,
        &valid_fields,
    );
    let parsed = inspect_component_bam_fields_in_registry(&reg, h_ok)
        .unwrap()
        .unwrap();
    assert_eq!(parsed.len(), 6);
    assert_eq!(parsed[3].pod_kind, Some(BamFieldPodKind::Vec3));
    assert!(parsed[3].is_inplace_reflectable);
    assert_eq!(parsed[4].pod_kind, Some(BamFieldPodKind::Entity));
    assert_eq!(parsed[5].pod_kind, None);
}

#[cfg(feature = "bridge-reflect")]
#[test]
fn bam_field_reflect_from_ptr_and_reflect_component_are_sound_under_miri() {
    use bevy_abi_mod_sys::{BamFieldDesc, BamFieldKind, ModComponent, StrView};
    use bevy_ecs::prelude::Resource;
    use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent, ReflectResource};
    use bevy_reflect::structs::DynamicStruct;
    use bevy_reflect::{GetPath, ReflectFromPtr, TypeInfo};

    #[repr(C)]
    struct ForeignBody {
        tag: u8,
        // 3 bytes implicit padding!
        score: f32,
        active: u8, // raw C99 bool byte (test foreign value `2` -> truthy!)
        // 3 bytes implicit padding!
        vel: [f32; 3],
        target_bits: u64,
    }
    assert_eq!(core::mem::size_of::<ForeignBody>(), 32);
    assert_eq!(core::mem::align_of::<ForeignBody>(), 8);

    #[repr(C)]
    #[derive(Resource)]
    struct MiriSimClock {
        dt: f32,
        tick: u32,
    }

    const CLOCK_FIELDS: &[BamFieldDesc] = &[
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("dt"),
            StrView::from_str("f32"),
            core::mem::offset_of!(MiriSimClock, dt),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("tick"),
            StrView::from_str("u32"),
            core::mem::offset_of!(MiriSimClock, tick),
            4,
            4,
        ),
    ];

    impl ModComponent for MiriSimClock {
        const CANONICAL_NAME: &'static str = "miri_c99::SimClock";
        const COMPONENT_CSTR: &'static core::ffi::CStr = c"miri_c99::SimClock";
        const UUID: ComponentUuid = ComponentUuid::from_parts("miri_c99::SimClock", 8, 4, 0);
        const IS_ABI_POD: bool = true;
        const BAM_FIELDS: &'static [BamFieldDesc] = CLOCK_FIELDS;
        type VTable = ();
        fn vtable() -> &'static Self::VTable {
            &()
        }
        fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
            &[]
        }
    }

    let fields = [
        BamFieldDesc::new(
            BamFieldKind::U8,
            StrView::from_str("tag"),
            StrView::from_str("u8"),
            core::mem::offset_of!(ForeignBody, tag),
            1,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::F32,
            StrView::from_str("score"),
            StrView::from_str("f32"),
            core::mem::offset_of!(ForeignBody, score),
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::BOOL,
            StrView::from_str("active"),
            StrView::from_str("bool"),
            core::mem::offset_of!(ForeignBody, active),
            1,
            1,
        ),
        BamFieldDesc::new(
            BamFieldKind::NESTED,
            StrView::from_str("vel"),
            StrView::from_str("Vec3"),
            core::mem::offset_of!(ForeignBody, vel),
            12,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U64,
            StrView::from_str("target_bits"),
            StrView::from_str("Entity"),
            core::mem::offset_of!(ForeignBody, target_bits),
            8,
            8,
        ),
    ];

    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    let mut reg = ModRegistry::new();

    // Register dynamic C99-style component (no Rust `ModComponent` / no witness VTable!)
    let layout = Layout::from_size_align(32, 8).unwrap();
    // SAFETY: Valid POD layout with `drop = None`.
    let comp_id = world.register_component_with_descriptor(unsafe {
        ComponentDescriptor::new_with_layout(
            "miri_c99::ForeignBody".to_owned(),
            StorageType::Table,
            layout,
            None,
            true,
            false,
            ComponentCloneBehavior::Default,
            None,
        )
    });
    let desc = ComponentDesc::new(
        c"miri_c99::ForeignBody".as_ptr(),
        ComponentUuid::from_parts("miri_c99::ForeignBody", 32, 8, 0),
        32,
        8,
    )
    .with_fields(fields.as_ptr(), fields.len());
    let handle = register_dyn_for_test(&mut reg, comp_id, &desc, "miri_c99::ForeignBody");

    // Also register a POD singleton resource carrying ONLY `BAM_FIELDS` (no `#[derive(Reflect)]`,
    // no witness VTable!).
    let res_cid = world.register_component::<MiriSimClock>();
    let res_handle = reg.register_resource::<MiriSimClock>(res_cid).unwrap();
    world.insert_resource(MiriSimClock {
        dt: 0.016,
        tick: 12,
    });

    world.insert_resource(reg);

    // Register dynamic `StructInfo` + `ReflectFromPtr` + `ReflectComponent`/`ReflectResource`
    let tid = register_bam_field_reflect_type(&mut world, handle)
        .unwrap()
        .expect("aligned POD struct must bind in-place ReflectFromPtr slot");
    let res_tid = register_bam_field_reflect_type(&mut world, res_handle)
        .unwrap()
        .expect("aligned POD resource must bind in-place ReflectFromPtr slot");

    // Verify `StructInfo`, `ReflectFromPtr`, `ReflectComponent`, `ReflectResource`
    let (rfp, reflect_comp) = {
        let app_types = world.resource::<AppTypeRegistry>().read();
        let type_reg = app_types.get(tid).unwrap();
        let TypeInfo::Struct(info) = type_reg.type_info() else {
            panic!("expected TypeInfo::Struct");
        };
        assert_eq!(info.type_path(), "miri_c99::ForeignBody");
        assert_eq!(info.type_path_table().ident(), Some("ForeignBody"));
        assert_eq!(info.field_len(), 5);
        assert!(type_reg.data::<ReflectResource>().is_none());

        let res_reg = app_types.get(res_tid).unwrap();
        assert!(res_reg.data::<ReflectResource>().is_some());
        (
            type_reg.data::<ReflectFromPtr>().unwrap().clone(),
            type_reg.data::<ReflectComponent>().unwrap().clone(),
        )
    };

    // Insert component with interior padding set to 0xAA and C99 truthy bool = 2
    let ent = world.spawn_empty().id();
    let mut raw_bytes = [0xAAu8; 32];
    raw_bytes[0] = 7;
    raw_bytes[4..8].copy_from_slice(&42.5f32.to_ne_bytes());
    raw_bytes[8] = 2; // non-canonical C99 true!
    raw_bytes[12..16].copy_from_slice(&1.0f32.to_ne_bytes());
    raw_bytes[16..20].copy_from_slice(&2.0f32.to_ne_bytes());
    raw_bytes[20..24].copy_from_slice(&3.0f32.to_ne_bytes());
    raw_bytes[24..32].copy_from_slice(&0x0001_0002_0003_0004u64.to_ne_bytes());
    write_entity_pod_bytes(&mut world, ent, handle, &raw_bytes).unwrap();

    // 1. Read-only `ReflectFromPtr` borrow via `reflect_entity_component`
    {
        let ent_ref = world.entity(ent);
        let r = reflect_entity_component(&world, ent_ref, comp_id, &rfp).unwrap();
        let s = r.reflect_ref().as_struct().unwrap();
        assert_eq!(
            *s.field("tag").unwrap().try_downcast_ref::<u8>().unwrap(),
            7
        );
        assert_eq!(
            *s.field("score").unwrap().try_downcast_ref::<f32>().unwrap(),
            42.5
        );
        assert!(
            *s.field("active")
                .unwrap()
                .try_downcast_ref::<bool>()
                .unwrap()
        );
        assert_eq!(
            *s.field("vel")
                .unwrap()
                .try_downcast_ref::<[f32; 3]>()
                .unwrap(),
            [1.0, 2.0, 3.0]
        );
        assert_eq!(
            *s.field("target_bits")
                .unwrap()
                .try_downcast_ref::<u64>()
                .unwrap(),
            0x0001_0002_0003_0004u64
        );
        let dyn_val = r.to_dynamic().unwrap();
        assert_eq!(
            dyn_val.get_represented_type_info().unwrap().type_path(),
            "miri_c99::ForeignBody"
        );
    }

    // 2. Mutable `ReflectFromPtr` borrow via `reflect_entity_component_mut`
    {
        let mut ent_mut = world.entity_mut(ent);
        let mut r = reflect_entity_component_mut(&mut ent_mut, comp_id, &rfp).unwrap();
        *r.reflect_path_mut("score")
            .unwrap()
            .try_downcast_mut::<f32>()
            .unwrap() = 99.25;
        *r.reflect_path_mut("active")
            .unwrap()
            .try_downcast_mut::<bool>()
            .unwrap() = false;
        *r.reflect_path_mut("vel[1]")
            .unwrap()
            .try_downcast_mut::<f32>()
            .unwrap() = -7.5;
    }

    // Verify byte-exact mutation in raw ECS storage & untouched padding bytes
    let mutated_bytes = read_entity_pod_bytes(&world, ent, handle).unwrap();
    assert_eq!(mutated_bytes[0], 7);
    assert_eq!(&mutated_bytes[1..4], &[0xAA, 0xAA, 0xAA]);
    assert_eq!(
        f32::from_ne_bytes(mutated_bytes[4..8].try_into().unwrap()),
        99.25
    );
    assert_eq!(mutated_bytes[8], 0);
    assert_eq!(
        f32::from_ne_bytes(mutated_bytes[16..20].try_into().unwrap()),
        -7.5
    );

    // 3. `ReflectComponent::insert`, `reflect`, `take`, and `remove`
    let ent2 = world.spawn_empty().id();
    let mut init_dyn = DynamicStruct::default();
    init_dyn.insert("tag", 11u8);
    init_dyn.insert("score", 3.5f32);
    init_dyn.insert("active", true);
    init_dyn.insert("vel", [4.0f32, 5.0, 6.0]);
    init_dyn.insert("target_bits", 77u64);
    {
        let app_types = world.resource::<AppTypeRegistry>().clone();
        let types_guard = app_types.read();
        let mut e2 = world.entity_mut(ent2);
        reflect_comp.insert(&mut e2, &init_dyn, &types_guard);
    }
    let taken = {
        let mut e2 = world.entity_mut(ent2);
        reflect_comp
            .take(&mut e2)
            .expect("take should return boxed slot")
    };
    assert!(!world.entity(ent2).contains_id(comp_id));
    let taken_s = taken.reflect_ref().as_struct().unwrap();
    assert_eq!(
        *taken_s
            .field("score")
            .unwrap()
            .try_downcast_ref::<f32>()
            .unwrap(),
        3.5
    );

    // 4. Declarative `BamFieldDesc` resource reflection via `reflect_resource_component` / `_mut`
    let res_rfp = world
        .resource::<AppTypeRegistry>()
        .read()
        .get(res_tid)
        .unwrap()
        .data::<ReflectFromPtr>()
        .unwrap()
        .clone();
    {
        let r = reflect_resource_component(&world, res_cid, &res_rfp).unwrap();
        assert_eq!(
            *r.reflect_path("tick")
                .unwrap()
                .try_downcast_ref::<u32>()
                .unwrap(),
            12
        );
    }
    {
        let mut r = reflect_resource_component_mut(&mut world, res_cid, &res_rfp).unwrap();
        *r.reflect_path_mut("tick")
            .unwrap()
            .try_downcast_mut::<u32>()
            .unwrap() = 55;
    }
    assert_eq!(world.resource::<MiriSimClock>().tick, 55);
    let tick_bytes = read_resource_bam_field_bytes(&world, res_handle, 4, 4).unwrap();
    assert_eq!(u32::from_ne_bytes(tick_bytes.try_into().unwrap()), 55);
}
