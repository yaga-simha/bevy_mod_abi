//! Adversarial ABI v0.2 boundary validation, extensible struct-size negotiation (`C-02`, `C-08`),
//! flag/reserved-word rejection (`C-09`), descriptor layout, and SplitMix64 fuzz matrices.

mod fuzz_boundaries {
    //! Adversarial / Fuzz-Style C-ABI `0.2.0` Boundary Verification Suite.
    //!
    //! Deterministically fuzzes the `host_*` `extern "C"` entry points across thousands of
    //! malformed, truncated, out-of-bounds, overflow, non-UTF-8, forged-entity, and permission-violating
    //! inputs using a pure-Rust SplitMix64 PRNG.

    use crate::support::{NameTag, Position, Velocity};
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
    use bevy_abi_mod_host_sys::{
        HostCommands, HostCommandsStorage, HostIter, LoadContext, MAX_SYSTEM_QUERIES, QueryPlan,
        QueryTermMeta, SparseRowScratch, host_cmd_despawn, host_cmd_insert,
        host_cmd_insert_default, host_cmd_remove, host_cmd_spawn, host_cmd_spawn_empty,
        host_get_proc_address, host_iter_column_info, host_iter_column_mutable,
        host_iter_column_mutable_untracked, host_iter_column_readonly,
        host_iter_column_rows_mutable, host_iter_column_rows_readonly, host_iter_entities,
        host_iter_mark_changed_range, host_iter_next_chunk, host_iter_reset, host_last_error,
        host_load_host_ctx, host_register_component, host_register_system, host_res_mut_ptr,
        host_res_ptr, host_resolve_component,
    };
    use bevy_abi_mod_sys::{
        AccessMode, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_SPARSE_SET, BamColumnInfo,
        BamComponent, BamQueryDesc, BamSystemCtx, BamSystemDesc, BamTermDesc, ComponentDesc,
        ComponentUuid, FfiBool, FfiEntity, FfiResult, MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY,
        ModComponent as _, ScheduleId,
    };
    use bevy_app::App;
    use bevy_ecs::change_detection::Tick;
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::query::QueryBuilder;
    use bevy_ecs::resource::Resource;
    use bevy_ecs::world::World;
    use core::ffi::{c_char, c_void};
    use core::mem::MaybeUninit;
    use core::sync::atomic::AtomicBool;
    use std::ffi::CString;
    use std::sync::Arc;

    #[cfg(not(miri))]
    const FUZZ_ITERS: usize = 2_000;
    #[cfg(miri)]
    const FUZZ_ITERS: usize = 120;

    /// Pure-Rust deterministic SplitMix64 PRNG.
    #[derive(Clone)]
    struct Rng(u64);

    impl Rng {
        const fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }

        fn range(&mut self, upper_exclusive: usize) -> usize {
            if upper_exclusive <= 1 {
                0
            } else {
                (self.next_u64() as usize) % upper_exclusive
            }
        }

        fn gen_bool(&mut self) -> bool {
            (self.next_u64() & 1) != 0
        }

        /// Generates a valid NUL-terminated byte buffer containing invalid UTF-8 sequences.
        fn invalid_utf8_cstring(&mut self) -> Vec<u8> {
            let len = 1 + self.range(12);
            let mut bytes = Vec::with_capacity(len + 1);
            bytes.push(if self.gen_bool() { 0xFF } else { 0x80 });
            for _ in 1..len {
                let mut b = (self.next_u32() & 0xFF) as u8;
                if b == 0 {
                    b = 0xFE;
                }
                bytes.push(b);
            }
            bytes.push(0);
            assert!(core::str::from_utf8(&bytes[..bytes.len() - 1]).is_err());
            bytes
        }
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq)]
    #[mod_component(name = "fuzz::FuzzBoolComp")]
    pub struct FuzzBoolComp {
        pub flag: bool,
        pub code: u32,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq)]
    #[component(storage = "SparseSet")]
    #[mod_component(name = "fuzz::FuzzSparseVal")]
    pub struct FuzzSparseVal {
        pub v: u64,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq)]
    #[mod_component(name = "fuzz::FuzzZst")]
    pub struct FuzzZst;

    #[repr(C)]
    #[derive(Resource, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
    #[mod_component(name = "fuzz::FuzzClockRes")]
    pub struct FuzzClockRes {
        pub ticks: u64,
    }

    unsafe extern "C" fn dummy_sys_cb(
        _sys_ctx: *mut BamSystemCtx,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    /// Serializes `template` into an exact-length byte buffer matching `declared_size` (at least 4 bytes for
    /// reading the `struct_size` header, capped at `size_of::<T>() + 64`), with optional byte misalignment.
    fn make_sized_descriptor_buffer<T: Copy>(
        template: &T,
        declared_size: u32,
        misalign_offset: usize,
    ) -> (Vec<u8>, *const c_void) {
        let full_size = core::mem::size_of::<T>();
        let copy_from_template = (declared_size as usize).min(full_size);
        let payload_alloc = (declared_size as usize).clamp(4, full_size + 64);
        let mut backing = vec![0u8; misalign_offset + payload_alloc];
        let dst = &mut backing[misalign_offset..misalign_offset + payload_alloc];

        unsafe {
            core::ptr::copy_nonoverlapping(
                (template as *const T).cast::<u8>(),
                dst.as_mut_ptr(),
                copy_from_template.min(payload_alloc),
            );
        }
        dst[..4].copy_from_slice(&declared_size.to_ne_bytes());
        let ptr = backing[misalign_offset..].as_ptr().cast::<c_void>();
        (backing, ptr)
    }

    // ============================================================================
    // 1. Fuzz `host_register_system` (Single-Query, Schedules, Ordering, Flags, Limits)
    // ============================================================================

    #[test]
    fn fuzz_host_register_system_family_adversarial_inputs() {
        let mut world = World::new();
        let mut registry = ModRegistry::new();
        let mut valid_handles = Vec::with_capacity(MAX_TERMS_PER_QUERY);

        // Register 32 distinct POD components so we can also fuzz the exact `MAX_TERMS_PER_QUERY = 32` boundary!
        {
            let mut seed_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            for i in 0..MAX_TERMS_PER_QUERY {
                let cname = CString::new(format!("fuzz::TermComp_{i}")).unwrap();
                let uuid = ComponentUuid::from_parts(cname.to_str().unwrap(), 4, 4, i as u64);
                let desc = ComponentDesc::new(cname.as_ptr(), uuid, 4, 4);
                let mut h = BamComponent::INVALID;
                assert_eq!(
                    unsafe { host_register_component(seed_ctx.as_ffi_ptr(), &desc, &mut h) },
                    FfiResult::OK
                );
                valid_handles.push(h);
            }
            registry = seed_ctx.registry.clone();
            seed_ctx.finish_init();
        }

        let mut load_ctx = LoadContext::new(registry);
        let ctx_ptr = load_ctx.as_ffi_ptr();

        // Exhaustive `struct_size` sweep `0..144` + `u32::MAX` (aligned and 1-byte misaligned)
        let valid_base = BamSystemDesc::new(
            c"valid_sys".as_ptr(),
            core::ptr::null(),
            0,
            dummy_sys_cb,
            core::ptr::null_mut(),
        )
        .with_schedule(ScheduleId::UPDATE);

        for misalign in [0usize, 1usize] {
            for sz in (0u32..144).chain(core::iter::once(u32::MAX)) {
                load_ctx.registered_systems.clear();
                let (_buf, raw_ptr) = make_sized_descriptor_buffer(&valid_base, sz, misalign);
                let min_v02 = BamSystemDesc::MIN_V0_2_SIZE as u32;
                let res = unsafe { host_register_system(ctx_ptr, raw_ptr.cast::<BamSystemDesc>()) };
                if sz < min_v02 {
                    assert_eq!(res, FfiResult::INVALID_VERSION, "struct_size={sz}");
                } else {
                    assert_eq!(res, FfiResult::OK, "struct_size={sz}");
                }
            }
        }

        // Null pointer & zeroed callback (`Option<BamSystemFn>::None`) checks
        unsafe {
            assert_eq!(
                host_register_system(core::ptr::null_mut(), core::ptr::null()),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_register_system(ctx_ptr, core::ptr::null()),
                FfiResult::NULL_POINTER
            );

            let mut zero_cb_bytes = vec![0u8; core::mem::size_of::<BamSystemDesc>()];
            let full_sz = core::mem::size_of::<BamSystemDesc>() as u32;
            zero_cb_bytes[..4].copy_from_slice(&full_sz.to_ne_bytes());
            assert_eq!(
                host_register_system(ctx_ptr, zero_cb_bytes.as_ptr().cast::<BamSystemDesc>()),
                FfiResult::NULL_POINTER
            );
        }

        let mut rng = Rng::new(0xC0FF_EE01_2026_1111);

        for step in 0..FUZZ_ITERS {
            load_ctx.registered_systems.clear();
            let mutation = rng.range(14);

            match mutation {
                0 => {
                    // `query_count > MAX_QUERIES_PER_SYSTEM` (`9..=usize::MAX`) with null `queries` -> `CAPACITY_EXCEEDED`
                    let bad_q_count = match rng.range(4) {
                        0 => MAX_QUERIES_PER_SYSTEM + 1,
                        1 => MAX_QUERIES_PER_SYSTEM + 1 + rng.range(10_000),
                        2 => usize::MAX,
                        _ => isize::MAX as usize,
                    };
                    let mut desc = valid_base;
                    desc.queries = core::ptr::null();
                    desc.query_count = bad_q_count;
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::CAPACITY_EXCEEDED, "step {step}");
                    assert!(load_ctx.registered_systems.is_empty());
                }
                1 => {
                    // `query_count in 1..=MAX_QUERIES_PER_SYSTEM` with null `queries` -> `NULL_POINTER`
                    let mut desc = valid_base;
                    desc.queries = core::ptr::null();
                    desc.query_count = 1 + rng.range(MAX_QUERIES_PER_SYSTEM);
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                }
                2 => {
                    // Unknown `BamSystemDesc::flags` bit (`C-09`) -> `UNSUPPORTED`
                    let bad_flag = 1u32 << (1 + rng.range(31));
                    let desc = valid_base.with_flags(bad_flag);
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::UNSUPPORTED, "step {step}");
                }
                3 => {
                    // Non-zero `BamSystemDesc::reserved0` (`C-09`) -> `INVALID_ARGUMENT`
                    let mut desc = valid_base;
                    desc.reserved0 = 1 + (rng.next_u32() % 1000);
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::INVALID_ARGUMENT, "step {step}");
                }
                4 => {
                    // Invalid `AccessMode(10..=u32::MAX)` inside `BamTermDesc` -> `INVALID_ARGUMENT`
                    let t_len = 1 + rng.range(8);
                    let bad_idx = rng.range(t_len);
                    let bad_mode = 10u32.saturating_add(rng.next_u32()).max(10);
                    let terms: Vec<BamTermDesc> = (0..t_len)
                        .map(|i| BamTermDesc {
                            component: valid_handles[i],
                            mode: if i == bad_idx {
                                AccessMode(bad_mode)
                            } else {
                                AccessMode(rng.range(4) as u32)
                            },
                            flags: 0,
                        })
                        .collect();
                    let q = BamQueryDesc::new(terms.as_ptr(), terms.len());
                    let desc = BamSystemDesc::new(
                        c"bad_mode_sys".as_ptr(),
                        &q,
                        1,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::INVALID_ARGUMENT, "step {step}");
                }
                5 => {
                    // Unknown `BamTermDesc::flags != 0` or `BamQueryDesc::flags != 0` -> `UNSUPPORTED`
                    let term = BamTermDesc {
                        component: valid_handles[0],
                        mode: AccessMode::READ,
                        flags: if rng.gen_bool() { 1 } else { 0 },
                    };
                    let mut q = BamQueryDesc::new(&term, 1);
                    if term.flags == 0 {
                        q.flags = 1 << rng.range(32);
                    }
                    let desc = BamSystemDesc::new(
                        c"bad_q_flags".as_ptr(),
                        &q,
                        1,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::UNSUPPORTED, "step {step}");
                }
                6 => {
                    // Null / empty / non-UTF-8 system `name` rejected cleanly (`C-01`)
                    let bad_sys_name = rng.invalid_utf8_cstring();
                    let mut desc = valid_base;
                    let expected = match rng.range(3) {
                        0 => {
                            desc.name = core::ptr::null();
                            FfiResult::NULL_POINTER
                        }
                        1 => {
                            desc.name = c"".as_ptr();
                            FfiResult::INVALID_ARGUMENT
                        }
                        _ => {
                            desc.name = bad_sys_name.as_ptr().cast::<c_char>();
                            FfiResult::INVALID_UTF8
                        }
                    };
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, expected, "step {step}");
                }
                7 => {
                    // Reserved `ScheduleId(6..1024)` rejected (`B-06`), while custom `>= 1024` accepted!
                    if rng.gen_bool() {
                        let reserved_sched = 6 + (rng.next_u32() % (1024 - 6));
                        let desc = valid_base.with_schedule(ScheduleId(reserved_sched));
                        let res = unsafe { host_register_system(ctx_ptr, &desc) };
                        assert_eq!(res, FfiResult::INVALID_ARGUMENT, "step {step}");
                    } else {
                        let custom_sched = ScheduleId::CUSTOM_BASE + (rng.next_u32() % 50_000);
                        let desc = valid_base.with_schedule(ScheduleId(custom_sched));
                        let res = unsafe { host_register_system(ctx_ptr, &desc) };
                        assert_eq!(res, FfiResult::OK, "step {step}");
                    }
                }
                8 => {
                    // Malformed `after` / `before`: non-zero count with null outer pointer
                    let mut desc = valid_base;
                    if rng.gen_bool() {
                        desc.after = core::ptr::null();
                        desc.after_count = 1 + rng.range(16);
                    } else {
                        desc.before = core::ptr::null();
                        desc.before_count = 1 + rng.range(16);
                    }
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                }
                9 => {
                    // Malformed `after` / `before`: array containing a null `*const c_char` element
                    let ptrs: [*const c_char; 2] = [c"physics".as_ptr(), core::ptr::null()];
                    let desc = if rng.gen_bool() {
                        valid_base.with_after(ptrs.as_ptr(), ptrs.len())
                    } else {
                        valid_base.with_before(ptrs.as_ptr(), ptrs.len())
                    };
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                }
                10 => {
                    // Malformed `after` / `before`: invalid UTF-8 or invalid target syntax (`""` / `"a::b::c"`)
                    let bad_set = rng.invalid_utf8_cstring();
                    let (bad_ptr, expected) = match rng.range(3) {
                        0 => (bad_set.as_ptr().cast::<c_char>(), FfiResult::INVALID_UTF8),
                        1 => (c"".as_ptr(), FfiResult::INVALID_ARGUMENT),
                        _ => (c"a::b::c".as_ptr(), FfiResult::INVALID_ARGUMENT),
                    };
                    let ptrs: [*const c_char; 2] = [c"valid_set".as_ptr(), bad_ptr];
                    let desc = if rng.gen_bool() {
                        valid_base.with_after(ptrs.as_ptr(), ptrs.len())
                    } else {
                        valid_base.with_before(ptrs.as_ptr(), ptrs.len())
                    };
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, expected, "step {step}");
                }
                11 => {
                    // Unregistered / `INVALID` `BamComponent` handle -> `INVALID_COMPONENT`
                    let bad_comp = if rng.gen_bool() {
                        BamComponent::INVALID
                    } else {
                        BamComponent::from_raw(999 + rng.next_u32() % 10_000)
                    };
                    let term = BamTermDesc::read(bad_comp);
                    let q = BamQueryDesc::new(&term, 1);
                    let desc = BamSystemDesc::new(
                        c"unknown_handle".as_ptr(),
                        &q,
                        1,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::INVALID_COMPONENT, "step {step}");
                }
                12 => {
                    // Truncated `BamSystemDesc::struct_size` randomly in `0..MIN_V0_2_SIZE`
                    let sz = rng.range(BamSystemDesc::MIN_V0_2_SIZE) as u32;
                    let (_buf, raw_ptr) =
                        make_sized_descriptor_buffer(&valid_base, sz, rng.range(4));
                    let res =
                        unsafe { host_register_system(ctx_ptr, raw_ptr.cast::<BamSystemDesc>()) };
                    assert_eq!(res, FfiResult::INVALID_VERSION, "step {step}");
                }
                _ => {
                    // Valid max-column boundary (`term_count == MAX_TERMS_PER_QUERY = 32` distinct components!)
                    let terms: Vec<BamTermDesc> = valid_handles
                        .iter()
                        .map(|&h| BamTermDesc {
                            component: h,
                            mode: AccessMode(rng.range(4) as u32),
                            flags: 0,
                        })
                        .collect();
                    let q = BamQueryDesc::new(terms.as_ptr(), terms.len());
                    let desc = BamSystemDesc::new(
                        c"max_32_terms_sys".as_ptr(),
                        &q,
                        1,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    )
                    .with_schedule(ScheduleId(rng.range(6) as u32));
                    let res = unsafe { host_register_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::OK, "step {step}");
                    assert_eq!(load_ctx.registered_systems.len(), 1);
                }
            }
        }
        load_ctx.finish_init();
    }

    // ============================================================================
    // 2. Fuzz Multi-Query `BamSystemDesc` (`0..=8` Queries, Conflicts, Immutability, Resources)
    // ============================================================================

    #[test]
    fn fuzz_host_register_multi_query_system_adversarial_inputs() {
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();
        let vel_id = world.register_component::<Velocity>();
        let res_id = world.register_component::<FuzzClockRes>();

        let mut registry = ModRegistry::new();
        let pos_h = registry
            .register_component::<Position>(pos_id, ComponentOptions::new())
            .unwrap();
        let vel_h = registry
            .register_component::<Velocity>(vel_id, ComponentOptions::new())
            .unwrap();
        let res_h = registry.register_resource::<FuzzClockRes>(res_id).unwrap();

        let mut immut_h = BamComponent::INVALID;
        let mut extra_handles = Vec::with_capacity(MAX_SYSTEM_QUERIES);
        {
            let mut seed_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            let immut_desc = ComponentDesc::new(
                c"fuzz::ImmutTeam".as_ptr(),
                ComponentUuid::from_parts("fuzz::ImmutTeam", 4, 4, 0),
                4,
                4,
            )
            .with_flags(BAM_COMPONENT_FLAG_IMMUTABLE);
            assert_eq!(
                unsafe {
                    host_register_component(seed_ctx.as_ffi_ptr(), &immut_desc, &mut immut_h)
                },
                FfiResult::OK
            );
            for i in 0..MAX_SYSTEM_QUERIES {
                let cname = CString::new(format!("fuzz::MqComp_{i}")).unwrap();
                let desc = ComponentDesc::new(
                    cname.as_ptr(),
                    ComponentUuid::from_parts(cname.to_str().unwrap(), 4, 4, i as u64),
                    4,
                    4,
                );
                let mut h = BamComponent::INVALID;
                assert_eq!(
                    unsafe { host_register_component(seed_ctx.as_ffi_ptr(), &desc, &mut h) },
                    FfiResult::OK
                );
                extra_handles.push(h);
            }
            registry = seed_ctx.registry.clone();
            seed_ctx.finish_init();
        }

        let mut load_ctx = LoadContext::new(registry);
        let ctx_ptr = load_ctx.as_ffi_ptr();

        let valid_term = BamTermDesc::read(pos_h);
        let valid_q = BamQueryDesc::new(&valid_term, 1);

        let mut rng = Rng::new(0xBADC_0FFE_E002_2222);
        for step in 0..FUZZ_ITERS {
            load_ctx.registered_systems.clear();
            match rng.range(9) {
                0 => {
                    // Truncated `BamQueryDesc::struct_size` inside `queries` array -> `INVALID_VERSION`
                    let mut bad_q = valid_q;
                    bad_q.struct_size = rng.range(BamQueryDesc::MIN_V0_2_SIZE) as u32;
                    let desc = BamSystemDesc::new(
                        c"trunc_q".as_ptr(),
                        &bad_q,
                        1,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    assert_eq!(
                        unsafe { host_register_system(ctx_ptr, &desc) },
                        FfiResult::INVALID_VERSION,
                        "step {step}"
                    );
                }
                1 => {
                    // Inner `BamQueryDesc` has `term_count > MAX_TERMS_PER_QUERY` -> `CAPACITY_EXCEEDED`
                    let bad_q = BamQueryDesc {
                        struct_size: BamQueryDesc::MIN_V0_2_SIZE as u32,
                        flags: 0,
                        terms: core::ptr::null(),
                        term_count: if rng.gen_bool() {
                            usize::MAX
                        } else {
                            MAX_TERMS_PER_QUERY + 1 + rng.range(500)
                        },
                    };
                    let qs = [valid_q, bad_q];
                    let desc = BamSystemDesc::new(
                        c"many_terms".as_ptr(),
                        qs.as_ptr(),
                        qs.len(),
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    assert_eq!(
                        unsafe { host_register_system(ctx_ptr, &desc) },
                        FfiResult::CAPACITY_EXCEEDED,
                        "step {step}"
                    );
                }
                2 => {
                    // Inner `BamQueryDesc` has `term_count > 0` with null `terms` -> `NULL_POINTER`
                    let bad_q = BamQueryDesc {
                        struct_size: BamQueryDesc::MIN_V0_2_SIZE as u32,
                        flags: 0,
                        terms: core::ptr::null(),
                        term_count: 1 + rng.range(MAX_TERMS_PER_QUERY),
                    };
                    let qs = [valid_q, bad_q];
                    let desc = BamSystemDesc::new(
                        c"null_terms".as_ptr(),
                        qs.as_ptr(),
                        qs.len(),
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    assert_eq!(
                        unsafe { host_register_system(ctx_ptr, &desc) },
                        FfiResult::NULL_POINTER,
                        "step {step}"
                    );
                }
                3 => {
                    // Inter-query borrow conflict (`WRITE(pos_h)` + `READ(pos_h)` without disjoint filter) -> `ACCESS_CONFLICT`
                    let t0 = BamTermDesc::write(pos_h);
                    let t1 = if rng.gen_bool() {
                        BamTermDesc::read(pos_h)
                    } else {
                        BamTermDesc::write(pos_h)
                    };
                    let qs = [BamQueryDesc::new(&t0, 1), BamQueryDesc::new(&t1, 1)];
                    let desc = BamSystemDesc::new(
                        c"inter_conflict".as_ptr(),
                        qs.as_ptr(),
                        qs.len(),
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    assert_eq!(
                        unsafe { host_register_system(ctx_ptr, &desc) },
                        FfiResult::ACCESS_CONFLICT,
                        "step {step}"
                    );
                }
                4 => {
                    // Intra-query borrow conflict (`[WRITE(vel_h), READ(vel_h)]`) -> `ACCESS_CONFLICT`
                    let terms = [BamTermDesc::write(vel_h), BamTermDesc::read(vel_h)];
                    let q = BamQueryDesc::new(terms.as_ptr(), 2);
                    let desc = BamSystemDesc::new(
                        c"intra_conflict".as_ptr(),
                        &q,
                        1,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    assert_eq!(
                        unsafe { host_register_system(ctx_ptr, &desc) },
                        FfiResult::ACCESS_CONFLICT,
                        "step {step}"
                    );
                }
                5 => {
                    // Kind mismatch: `RESOURCE_READ` on entity component or `READ` on Resource -> `INVALID_COMPONENT`
                    let bad_term = if rng.gen_bool() {
                        BamTermDesc::resource_read(pos_h)
                    } else {
                        BamTermDesc::read(res_h)
                    };
                    let q = BamQueryDesc::new(&bad_term, 1);
                    let desc = BamSystemDesc::new(
                        c"kind_mismatch".as_ptr(),
                        &q,
                        1,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    assert_eq!(
                        unsafe { host_register_system(ctx_ptr, &desc) },
                        FfiResult::INVALID_COMPONENT,
                        "step {step}"
                    );
                }
                6 => {
                    // Write to `BAM_COMPONENT_FLAG_IMMUTABLE` component (`B-08`) -> `PERMISSION_DENIED`
                    let bad_term = if rng.gen_bool() {
                        BamTermDesc::write(immut_h)
                    } else {
                        BamTermDesc::optional_write(immut_h)
                    };
                    let q = BamQueryDesc::new(&bad_term, 1);
                    let desc = BamSystemDesc::new(
                        c"immut_write".as_ptr(),
                        &q,
                        1,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    assert_eq!(
                        unsafe { host_register_system(ctx_ptr, &desc) },
                        FfiResult::PERMISSION_DENIED,
                        "step {step}"
                    );
                }
                7 => {
                    // Forward-extended `BamQueryDesc` element stride (`struct_size = 40` > `24`) across 2 queries!
                    #[repr(C)]
                    #[derive(Clone, Copy)]
                    struct ExtendedQueryDesc {
                        base: BamQueryDesc,
                        future_word_0: u64,
                        future_word_1: u64,
                    }
                    let t0 = BamTermDesc::write(extra_handles[0]);
                    let t1 = BamTermDesc::write(extra_handles[1]);
                    let ext_qs = [
                        ExtendedQueryDesc {
                            base: BamQueryDesc {
                                struct_size: core::mem::size_of::<ExtendedQueryDesc>() as u32,
                                flags: 0,
                                terms: &t0,
                                term_count: 1,
                            },
                            future_word_0: 0x1111,
                            future_word_1: 0x2222,
                        },
                        ExtendedQueryDesc {
                            base: BamQueryDesc {
                                struct_size: core::mem::size_of::<ExtendedQueryDesc>() as u32,
                                flags: 0,
                                terms: &t1,
                                term_count: 1,
                            },
                            future_word_0: 0x3333,
                            future_word_1: 0x4444,
                        },
                    ];
                    let desc = BamSystemDesc::new(
                        c"extended_q_stride".as_ptr(),
                        ext_qs.as_ptr().cast::<BamQueryDesc>(),
                        2,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    assert_eq!(
                        unsafe { host_register_system(ctx_ptr, &desc) },
                        FfiResult::OK,
                        "step {step}"
                    );
                }
                _ => {
                    // Valid `MAX_SYSTEM_QUERIES = 8` disjoint mutable queries + `RESOURCE_WRITE`!
                    let terms: [BamTermDesc; MAX_SYSTEM_QUERIES] = core::array::from_fn(|i| {
                        if i == MAX_SYSTEM_QUERIES - 1 {
                            BamTermDesc::resource_write(res_h)
                        } else {
                            BamTermDesc::write(extra_handles[i])
                        }
                    });
                    let qs: [BamQueryDesc; MAX_SYSTEM_QUERIES] =
                        core::array::from_fn(|i| BamQueryDesc::new(&terms[i], 1));
                    let desc = BamSystemDesc::new(
                        c"valid_8_queries".as_ptr(),
                        qs.as_ptr(),
                        MAX_SYSTEM_QUERIES,
                        dummy_sys_cb,
                        core::ptr::null_mut(),
                    );
                    assert_eq!(
                        unsafe { host_register_system(ctx_ptr, &desc) },
                        FfiResult::OK,
                        "step {step}"
                    );
                    assert_eq!(load_ctx.registered_systems.len(), 1);
                }
            }
        }
        load_ctx.finish_init();
    }

    // ============================================================================
    // 3. Fuzz `host_register_component` & `host_resolve_component`
    // ============================================================================

    #[test]
    fn fuzz_host_register_component_layouts_and_contexts() {
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());
        let is_active = Arc::new(AtomicBool::new(true));

        let seeded_uuid = ComponentUuid::from_parts("fuzz::SeededDynComp", 16, 8, 0);
        let mut seeded_handle = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::clone(&is_active),
                &mut world as *mut World,
                None,
            );
            let seed_desc = ComponentDesc::new(c"fuzz::SeededDynComp".as_ptr(), seeded_uuid, 16, 8);
            assert_eq!(
                unsafe {
                    host_register_component(load_ctx.as_ffi_ptr(), &seed_desc, &mut seeded_handle)
                },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        // 1. Exhaustive `struct_size` sweep `0..128` + `u32::MAX`
        let probe_uuid = ComponentUuid::from_parts("fuzz::SizedProbeComp", 8, 8, 0);
        let template_desc = ComponentDesc::new(c"fuzz::SizedProbeComp".as_ptr(), probe_uuid, 8, 8);
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                world.resource::<ModRegistry>().clone(),
                Arc::clone(&is_active),
                &mut world as *mut World,
                None,
            );
            let ctx_ptr = load_ctx.as_ffi_ptr();
            let mut out = BamComponent::INVALID;

            for sz in (0u32..128).chain(core::iter::once(u32::MAX)) {
                let (_buf, raw_ptr) = make_sized_descriptor_buffer(&template_desc, sz, 0);
                let min_v02 = ComponentDesc::MIN_V0_2_SIZE as u32;
                let res = unsafe {
                    host_register_component(ctx_ptr, raw_ptr.cast::<ComponentDesc>(), &mut out)
                };
                if sz < min_v02 {
                    assert_eq!(res, FfiResult::INVALID_VERSION, "sz={sz}");
                } else {
                    assert_eq!(res, FfiResult::OK, "sz={sz}");
                }
            }
            load_ctx.finish_init();

            // Poisoned `LoadContext` after `finish_init()` MUST return `INVALID_STATE` (`S-08`)
            assert_eq!(
                unsafe { host_register_component(ctx_ptr, &template_desc, &mut out) },
                FfiResult::INVALID_STATE
            );
        }

        // 2. Randomized layout, collision, and `host_resolve_component` fuzzing
        let mut rng = Rng::new(0xFACE_FEED_0003_3333);
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            world.resource::<ModRegistry>().clone(),
            Arc::clone(&is_active),
            &mut world as *mut World,
            None,
        );
        let ctx_ptr = load_ctx.as_ffi_ptr();
        let host_ctx = unsafe { host_load_host_ctx(ctx_ptr) };

        let non_pow2_aligns: [usize; 12] = [0, 3, 5, 6, 7, 9, 10, 12, 15, 24, 100, usize::MAX];

        for step in 0..FUZZ_ITERS {
            let name_cstr = CString::new(format!("fuzz::FuzzDyn_{step}")).unwrap();
            let uuid = ComponentUuid {
                bytes: {
                    let mut b = [0u8; 16];
                    b[..8].copy_from_slice(&rng.next_u64().to_le_bytes());
                    b[8..].copy_from_slice(&rng.next_u64().to_le_bytes());
                    b
                },
            };
            let mut out = BamComponent::INVALID;

            match rng.range(9) {
                0 => {
                    let desc = ComponentDesc::new(core::ptr::null(), uuid, 8, 8);
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc, &mut out) },
                        FfiResult::NULL_POINTER
                    );
                }
                1 => {
                    let bad_name = rng.invalid_utf8_cstring();
                    let desc = ComponentDesc::new(bad_name.as_ptr().cast::<c_char>(), uuid, 8, 8);
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc, &mut out) },
                        FfiResult::INVALID_UTF8
                    );
                }
                2 => {
                    let align = non_pow2_aligns[rng.range(non_pow2_aligns.len())];
                    let size = rng.range(128);
                    let desc = ComponentDesc::new(name_cstr.as_ptr(), uuid, size, align);
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc, &mut out) },
                        FfiResult::INVALID_ARGUMENT
                    );
                }
                3 => {
                    let shift = 1 + rng.range(10);
                    let align = 1usize << shift;
                    let rem = 1 + rng.range(align - 1);
                    let size = rng.range(16) * align + rem;
                    let desc = ComponentDesc::new(name_cstr.as_ptr(), uuid, size, align);
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc, &mut out) },
                        FfiResult::INVALID_ARGUMENT
                    );
                }
                4 => {
                    let shift = rng.range(8);
                    let align = 1usize << shift;
                    let overflow_size = match rng.range(3) {
                        0 => usize::MAX & !(align - 1),
                        1 => (isize::MAX as usize) + 1,
                        _ => usize::MAX,
                    };
                    let desc = ComponentDesc::new(name_cstr.as_ptr(), uuid, overflow_size, align);
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc, &mut out) },
                        FfiResult::INVALID_ARGUMENT
                    );
                }
                5 => {
                    // Conflicting re-registration of `fuzz::SeededDynComp` -> `TYPE_MISMATCH` (`C-10`)
                    let (bad_size, bad_align, bad_uuid) = match rng.range(3) {
                        0 => (32, 8, seeded_uuid),
                        1 => (16, 16, seeded_uuid),
                        _ => (
                            16,
                            8,
                            ComponentUuid::from_canonical_name("fuzz::DifferentUuid"),
                        ),
                    };
                    let conflict_desc = ComponentDesc::new(
                        c"fuzz::SeededDynComp".as_ptr(),
                        bad_uuid,
                        bad_size,
                        bad_align,
                    );
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &conflict_desc, &mut out) },
                        FfiResult::TYPE_MISMATCH
                    );
                }
                6 => {
                    // Idempotent re-registration -> `OK`
                    let same_desc =
                        ComponentDesc::new(c"fuzz::SeededDynComp".as_ptr(), seeded_uuid, 16, 8);
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &same_desc, &mut out) },
                        FfiResult::OK
                    );
                    assert_eq!(out, seeded_handle);
                }
                7 => {
                    // Fuzz `host_resolve_component` (`C-05`)
                    match rng.range(4) {
                        0 => {
                            assert_eq!(
                                unsafe {
                                    host_resolve_component(
                                        host_ctx,
                                        core::ptr::null(),
                                        core::ptr::null(),
                                        &mut out,
                                    )
                                },
                                FfiResult::INVALID_ARGUMENT
                            );
                        }
                        1 => {
                            assert_eq!(
                                unsafe {
                                    host_resolve_component(
                                        host_ctx,
                                        c"fuzz::SeededDynComp".as_ptr(),
                                        &seeded_uuid,
                                        &mut out,
                                    )
                                },
                                FfiResult::OK
                            );
                            assert_eq!(out, seeded_handle);
                        }
                        2 => {
                            assert_eq!(
                                unsafe {
                                    host_resolve_component(
                                        host_ctx,
                                        c"fuzz::SeededDynComp".as_ptr(),
                                        &uuid,
                                        &mut out,
                                    )
                                },
                                FfiResult::TYPE_MISMATCH
                            );
                        }
                        _ => {
                            assert_eq!(
                                unsafe {
                                    host_resolve_component(
                                        host_ctx,
                                        name_cstr.as_ptr(),
                                        core::ptr::null(),
                                        &mut out,
                                    )
                                },
                                FfiResult::NOT_FOUND
                            );
                        }
                    }
                }
                _ => {
                    let align = 1usize << rng.range(9);
                    let mult = rng.range(5);
                    let size = mult * align;
                    let desc = ComponentDesc::new(name_cstr.as_ptr(), uuid, size, align);
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc, &mut out) },
                        FfiResult::OK
                    );
                }
            }
        }
        load_ctx.finish_init();
    }

    // ============================================================================
    // 4. Fuzz `host_iter_*` (Dense Table & SparseSet Archetypal State Machines)
    // ============================================================================

    #[test]
    fn fuzz_host_iter_state_machine_and_bounds() {
        // 1. Null-pointer safety across every `host_iter_*` and `host_res_*` entry point
        unsafe {
            let mut len = 99usize;
            let mut info = MaybeUninit::<BamColumnInfo>::uninit();
            assert_eq!(
                host_iter_next_chunk(core::ptr::null_mut(), &mut len),
                FfiBool::FALSE
            );
            assert_eq!(
                host_iter_next_chunk(core::ptr::null_mut(), core::ptr::null_mut()),
                FfiBool::FALSE
            );
            assert_eq!(
                host_iter_reset(core::ptr::null_mut()),
                FfiResult::NULL_POINTER
            );
            assert!(host_iter_column_readonly(core::ptr::null(), 0).is_null());
            assert!(host_iter_column_readonly(core::ptr::null(), usize::MAX).is_null());
            assert!(host_iter_column_mutable(core::ptr::null_mut(), 0).is_null());
            assert!(host_iter_column_mutable_untracked(core::ptr::null_mut(), 0).is_null());
            assert!(host_iter_column_rows_readonly(core::ptr::null_mut(), 0).is_null());
            assert!(host_iter_column_rows_mutable(core::ptr::null_mut(), 0).is_null());
            assert_eq!(
                host_iter_mark_changed_range(core::ptr::null_mut(), 0, 0, 1),
                FfiResult::NULL_POINTER
            );
            assert!(host_iter_entities(core::ptr::null()).is_null());
            assert_eq!(
                host_iter_column_info(core::ptr::null(), 0, info.as_mut_ptr()),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_iter_column_info(core::ptr::null(), 0, core::ptr::null_mut()),
                FfiResult::NULL_POINTER
            );
            assert!(host_res_ptr(core::ptr::null(), 0).is_null());
            assert!(host_res_mut_ptr(core::ptr::null_mut(), 0).is_null());
        }

        // 2. Build a multi-archetype World with Table, SparseSet, and ZST columns
        let mut world = World::new();
        let pos_id = world.register_component::<Position>();
        let vel_id = world.register_component::<Velocity>();
        let zst_id = world.register_component::<FuzzZst>();
        let sparse_id = world.register_component::<FuzzSparseVal>();

        for i in 0..12u32 {
            let pos = Position {
                x: i as f32,
                y: (i * 2) as f32,
                z: 0.0,
            };
            let vel = Velocity {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            };
            match i % 3 {
                0 => {
                    world.spawn((pos, vel));
                }
                1 => {
                    world.spawn((pos, vel, FuzzZst, FuzzSparseVal { v: i as u64 }));
                }
                _ => {
                    world.spawn((pos, FuzzSparseVal { v: 100 + i as u64 }));
                }
            }
        }

        let mut dense_qb = QueryBuilder::<bevy_ecs::world::FilteredEntityMut>::new(&mut world);
        dense_qb.ref_id(pos_id);
        dense_qb.mut_id(vel_id);
        dense_qb.without_id(zst_id);
        let mut dense_qs = dense_qb.build();
        dense_qs.update_archetypes(&world);
        let dense_tables: Vec<_> = dense_qs.matched_tables().collect();
        let dense_plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: BamComponent::from_raw(1),
                component_id: pos_id,
                mode: AccessMode::READ,
                uuid: Position::UUID,
                size: core::mem::size_of::<Position>(),
                align: core::mem::align_of::<Position>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(2),
                component_id: vel_id,
                mode: AccessMode::WRITE,
                uuid: Velocity::UUID,
                size: core::mem::size_of::<Velocity>(),
                align: core::mem::align_of::<Velocity>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(3),
                component_id: zst_id,
                mode: AccessMode::WITHOUT,
                uuid: FuzzZst::UUID,
                size: 0,
                align: 1,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
        ]);

        let mut sparse_qb = QueryBuilder::<bevy_ecs::world::FilteredEntityMut>::new(&mut world);
        sparse_qb.ref_id(pos_id);
        sparse_qb.mut_id(sparse_id);
        sparse_qb.with_id(vel_id);
        let mut sparse_qs = sparse_qb.build();
        sparse_qs.update_archetypes(&world);
        let sparse_archs: Vec<_> = sparse_qs.matched_archetypes().collect();
        let sparse_plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: BamComponent::from_raw(1),
                component_id: pos_id,
                mode: AccessMode::READ,
                uuid: Position::UUID,
                size: core::mem::size_of::<Position>(),
                align: core::mem::align_of::<Position>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(4),
                component_id: sparse_id,
                mode: AccessMode::WRITE,
                uuid: FuzzSparseVal::UUID,
                size: core::mem::size_of::<FuzzSparseVal>(),
                align: core::mem::align_of::<FuzzSparseVal>(),
                storage_flags: BAM_STORAGE_FLAG_SPARSE_SET,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(2),
                component_id: vel_id,
                mode: AccessMode::FILTER_ONLY,
                uuid: Velocity::UUID,
                size: core::mem::size_of::<Velocity>(),
                align: core::mem::align_of::<Velocity>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
        ]);

        let this_run = world.increment_change_tick();
        let cell = world.as_unsafe_world_cell();
        let storages = unsafe { cell.storages() };
        let archetypes = cell.archetypes();

        let mut rng = Rng::new(0x17E4_F022_0004_4444);

        for use_sparse in [false, true] {
            let mut scratch = SparseRowScratch::default();
            let plan_ref = if use_sparse {
                &sparse_plan
            } else {
                &dense_plan
            };
            let mut host_iter = if use_sparse {
                HostIter::new_archetypal(
                    &storages.tables,
                    archetypes,
                    &storages.sparse_sets,
                    &sparse_archs,
                    plan_ref,
                    &mut scratch,
                    Tick::new(0),
                    this_run,
                )
            } else {
                HostIter::new_dense(
                    &storages.tables,
                    &dense_tables,
                    plan_ref,
                    Tick::new(0),
                    this_run,
                )
            };

            let raw = host_iter.as_ffi_ptr();
            let mut active_len: usize = 0;

            for step in 0..FUZZ_ITERS {
                let col_idx = match rng.range(6) {
                    0 => 0,
                    1 => 1,
                    2 => 2,
                    3 => 3 + rng.range(MAX_TERMS_PER_QUERY),
                    4 => MAX_TERMS_PER_QUERY + rng.range(1_000),
                    _ => usize::MAX,
                };

                match rng.range(7) {
                    0 => {
                        let mut out_len = 0usize;
                        let has_more = unsafe { host_iter_next_chunk(raw, &mut out_len) }.get();
                        active_len = if has_more { out_len } else { 0 };
                    }
                    1 => {
                        let r = unsafe { host_iter_reset(raw) };
                        assert_eq!(r, FfiResult::OK, "step {step}");
                        active_len = 0;
                    }
                    2 => {
                        // Readonly request (`column_readonly` for dense vs `column_rows_readonly` for sparse)
                        let dense_ptr = unsafe { host_iter_column_readonly(raw, col_idx) };
                        let sparse_rows = unsafe { host_iter_column_rows_readonly(raw, col_idx) };
                        if active_len == 0 || col_idx >= 2 {
                            assert!(dense_ptr.is_null());
                            assert!(sparse_rows.is_null());
                        } else if use_sparse && col_idx == 1 {
                            assert!(dense_ptr.is_null());
                            assert!(!sparse_rows.is_null());
                        } else {
                            assert!(!dense_ptr.is_null());
                            assert!(sparse_rows.is_null());
                        }
                    }
                    3 => {
                        let dense_mut = unsafe { host_iter_column_mutable(raw, col_idx) };
                        let sparse_mut = unsafe { host_iter_column_rows_mutable(raw, col_idx) };
                        if active_len > 0 && col_idx == 1 {
                            if use_sparse {
                                assert!(dense_mut.is_null());
                                assert!(!sparse_mut.is_null());
                            } else {
                                assert!(!dense_mut.is_null());
                                assert!(sparse_mut.is_null());
                            }
                        } else {
                            assert!(dense_mut.is_null());
                            assert!(sparse_mut.is_null());
                        }
                    }
                    4 => {
                        let row = match rng.range(4) {
                            0 => 0,
                            1 => active_len.saturating_sub(1),
                            2 => active_len + rng.range(64),
                            _ => usize::MAX,
                        };
                        let res = unsafe { host_iter_mark_changed_range(raw, col_idx, row, 1) };
                        if col_idx >= plan_ref.terms.len() {
                            assert_eq!(res, FfiResult::OUT_OF_RANGE, "step {step}");
                        } else if plan_ref.terms[col_idx].mode != AccessMode::WRITE {
                            assert_eq!(res, FfiResult::PERMISSION_DENIED, "step {step}");
                        } else if row.checked_add(1).is_none_or(|end| end > active_len) {
                            assert_eq!(res, FfiResult::OUT_OF_RANGE, "step {step}");
                        } else {
                            assert_eq!(res, FfiResult::OK, "step {step}");
                        }
                    }
                    5 => {
                        let ent_ptr = unsafe { host_iter_entities(raw) };
                        if active_len == 0 {
                            assert!(ent_ptr.is_null(), "step {step}");
                        } else {
                            assert!(!ent_ptr.is_null(), "step {step}");
                            for r in 0..active_len {
                                let ffi_e = unsafe { *ent_ptr.add(r) };
                                assert!(!ffi_e.is_null());
                            }
                        }
                    }
                    _ => {
                        let mut info = MaybeUninit::<BamColumnInfo>::uninit();
                        let rc = unsafe { host_iter_column_info(raw, col_idx, info.as_mut_ptr()) };
                        if col_idx < plan_ref.terms.len() {
                            assert_eq!(rc, FfiResult::OK, "step {step}");
                            assert_eq!(
                                unsafe { info.assume_init() }.size,
                                plan_ref.terms[col_idx].size
                            );
                        } else {
                            assert_eq!(rc, FfiResult::OUT_OF_RANGE, "step {step}");
                        }
                    }
                }
            }
        }
    }

    // ============================================================================
    // 5. Fuzz `host_cmd_*` and `host_get_proc_address` (including `apply_to_world`)
    // ============================================================================

    #[test]
    fn fuzz_host_cmd_and_proc_address_adversarial_inputs() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new())
            .unwrap();
        let zst_h = app
            .register_mod_component::<FuzzZst>(ComponentOptions::new())
            .unwrap();
        let nametag_h = app
            .register_mod_component::<NameTag>(ComponentOptions::new().with_default())
            .unwrap();
        let bool_h = app
            .register_mod_component::<FuzzBoolComp>(ComponentOptions::new())
            .unwrap();
        let pos_opaque_h = app
            .register_mod_component::<Position>(ComponentOptions::new().opaque())
            .unwrap();
        let res_h = app.register_mod_resource::<FuzzClockRes>().unwrap();

        let mut dyn_h = BamComponent::INVALID;
        {
            let world = app.world_mut();
            let reg = world.resource::<ModRegistry>().clone();
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::new(AtomicBool::new(true)),
                world as *mut World,
                None,
            );
            let desc = ComponentDesc::new(
                c"fuzz::DynPod8".as_ptr(),
                ComponentUuid::from_parts("fuzz::DynPod8", 8, 8, 0),
                8,
                8,
            );
            assert_eq!(
                unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut dyn_h) },
                FfiResult::OK
            );
            ctx.finish_init();
        }

        let mut rng = Rng::new(0xC0DE_CAFE_0005_5555);
        let mut live_entities: Vec<FfiEntity> = Vec::new();
        let scratch_payload = [0xA5u8; 256];
        let mut storage = HostCommandsStorage::new();
        let mut load_ctx = LoadContext::new(app.world().resource::<ModRegistry>().clone());
        let host_ctx = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };

        for step in 0..FUZZ_ITERS {
            {
                let world = app.world();
                let snap = world.resource::<ModRegistry>().snapshot_arc();
                let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                let raw_q = cmds.as_ffi_ptr();

                match rng.range(12) {
                    0 => {
                        let mut e = FfiEntity::NULL;
                        assert_eq!(
                            unsafe { host_cmd_spawn_empty(raw_q, &mut e) },
                            FfiResult::OK
                        );
                        assert!(!e.is_null());
                        assert_ne!(e.to_bits(), Entity::PLACEHOLDER.to_bits());
                        live_entities.push(e);
                    }
                    1 => {
                        // Batched `host_cmd_spawn` (`P-05`): valid vs duplicate component vs `count > 64`
                        let mut e = FfiEntity::NULL;
                        match rng.range(3) {
                            0 => {
                                let vel = Velocity {
                                    x: step as f32,
                                    y: 1.0,
                                    z: 2.0,
                                };
                                let comps = [vel_h, zst_h];
                                let ptrs =
                                    [(&vel as *const Velocity).cast::<u8>(), core::ptr::null()];
                                assert_eq!(
                                    unsafe {
                                        host_cmd_spawn(
                                            raw_q,
                                            comps.as_ptr(),
                                            ptrs.as_ptr(),
                                            2,
                                            &mut e,
                                        )
                                    },
                                    FfiResult::OK
                                );
                                live_entities.push(e);
                            }
                            1 => {
                                // Duplicate component handle in single spawn batch -> `INVALID_ARGUMENT`
                                let comps = [vel_h, vel_h];
                                let ptrs = [scratch_payload.as_ptr(), scratch_payload.as_ptr()];
                                assert_eq!(
                                    unsafe {
                                        host_cmd_spawn(
                                            raw_q,
                                            comps.as_ptr(),
                                            ptrs.as_ptr(),
                                            2,
                                            &mut e,
                                        )
                                    },
                                    FfiResult::INVALID_ARGUMENT
                                );
                            }
                            _ => {
                                // Exceed `MAX_SPAWN_BATCH_COMPONENTS` (`64`) -> `CAPACITY_EXCEEDED`
                                assert_eq!(
                                    unsafe {
                                        host_cmd_spawn(
                                            raw_q,
                                            core::ptr::null(),
                                            core::ptr::null(),
                                            65 + rng.range(100),
                                            &mut e,
                                        )
                                    },
                                    FfiResult::CAPACITY_EXCEEDED
                                );
                            }
                        }
                    }
                    2 => {
                        let bad_entity = match rng.range(5) {
                            0 => FfiEntity::NULL,
                            1 => FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()),
                            2 => FfiEntity::from_bits(u64::MAX),
                            3 => FfiEntity::from_bits(rng.next_u64()),
                            _ => FfiEntity::from_bits((rng.next_u32() as u64) << 32),
                        };
                        let res = unsafe { host_cmd_despawn(raw_q, bad_entity) };
                        if bad_entity.is_null()
                            || Entity::try_from_bits(bad_entity.to_bits()).is_none()
                            || Entity::try_from_bits(bad_entity.to_bits())
                                == Some(Entity::PLACEHOLDER)
                        {
                            assert_eq!(res, FfiResult::INVALID_ENTITY, "step {step}");
                        } else {
                            assert_eq!(res, FfiResult::OK, "step {step}");
                        }
                    }
                    3 => {
                        if !live_entities.is_empty() {
                            let idx = rng.range(live_entities.len());
                            let e = if rng.gen_bool() {
                                live_entities[idx]
                            } else {
                                live_entities.swap_remove(idx)
                            };
                            assert_eq!(unsafe { host_cmd_despawn(raw_q, e) }, FfiResult::OK);
                        }
                    }
                    4 => {
                        // `data_len > 0 && data_ptr.is_null()` -> `NULL_POINTER`
                        let target = live_entities
                            .first()
                            .copied()
                            .unwrap_or(FfiEntity::from_bits(42));
                        let bad_len = 1 + rng.range(1024);
                        assert_eq!(
                            unsafe {
                                host_cmd_insert(raw_q, target, vel_h, core::ptr::null(), bad_len)
                            },
                            FfiResult::NULL_POINTER
                        );
                    }
                    5 => {
                        // Forged / resource handle on `host_cmd_insert` -> `INVALID_COMPONENT`
                        let bad_h = match rng.range(3) {
                            0 => BamComponent::INVALID,
                            1 => res_h,
                            _ => BamComponent::from_raw(500 + rng.next_u32() % 10_000),
                        };
                        assert_eq!(
                            unsafe {
                                host_cmd_insert(
                                    raw_q,
                                    FfiEntity::from_bits(1),
                                    bad_h,
                                    scratch_payload.as_ptr(),
                                    8,
                                )
                            },
                            FfiResult::INVALID_COMPONENT
                        );
                    }
                    6 => {
                        // Raw byte insertion onto `String` (`NameTag`), `bool` (`FuzzBoolComp`), or `opaque` (`Position`) -> `PERMISSION_DENIED`
                        let (h, comp_size) = match rng.range(3) {
                            0 => (nametag_h, core::mem::size_of::<NameTag>()),
                            1 => (bool_h, core::mem::size_of::<FuzzBoolComp>()),
                            _ => (pos_opaque_h, core::mem::size_of::<Position>()),
                        };
                        let target = live_entities.first().copied().unwrap_or_else(|| {
                            let mut e = FfiEntity::NULL;
                            let _ = unsafe { host_cmd_spawn_empty(raw_q, &mut e) };
                            e
                        });
                        assert_eq!(
                            unsafe {
                                host_cmd_insert(
                                    raw_q,
                                    target,
                                    h,
                                    scratch_payload.as_ptr(),
                                    comp_size,
                                )
                            },
                            FfiResult::PERMISSION_DENIED
                        );
                    }
                    7 => {
                        // Wrong byte size -> `INVALID_ARGUMENT`
                        let (h, expected_sz) = if rng.gen_bool() {
                            (vel_h, core::mem::size_of::<Velocity>())
                        } else {
                            (dyn_h, 8usize)
                        };
                        let mut wrong_len = rng.range(scratch_payload.len() + 1);
                        if wrong_len == expected_sz {
                            wrong_len += 1;
                        }
                        let target = live_entities
                            .first()
                            .copied()
                            .unwrap_or(FfiEntity::from_bits(1));
                        assert_eq!(
                            unsafe {
                                host_cmd_insert(
                                    raw_q,
                                    target,
                                    h,
                                    scratch_payload.as_ptr(),
                                    wrong_len,
                                )
                            },
                            FfiResult::INVALID_ARGUMENT
                        );
                    }
                    8 => {
                        // `NULL` or `PLACEHOLDER` entity -> `INVALID_ENTITY`
                        let bad_ent = if rng.gen_bool() {
                            FfiEntity::NULL
                        } else {
                            FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits())
                        };
                        assert_eq!(
                            unsafe {
                                host_cmd_insert(
                                    raw_q,
                                    bad_ent,
                                    vel_h,
                                    scratch_payload.as_ptr(),
                                    core::mem::size_of::<Velocity>(),
                                )
                            },
                            FfiResult::INVALID_ENTITY
                        );
                    }
                    9 => {
                        // `host_cmd_insert_default` + `host_cmd_remove`
                        let target = if !live_entities.is_empty() {
                            live_entities[rng.range(live_entities.len())]
                        } else {
                            let mut e = FfiEntity::NULL;
                            let _ = unsafe { host_cmd_spawn_empty(raw_q, &mut e) };
                            live_entities.push(e);
                            e
                        };
                        assert_eq!(
                            unsafe { host_cmd_insert_default(raw_q, target, nametag_h) },
                            FfiResult::OK
                        );
                        assert_eq!(
                            unsafe { host_cmd_insert_default(raw_q, target, bool_h) },
                            FfiResult::PERMISSION_DENIED
                        );
                        assert_eq!(
                            unsafe { host_cmd_remove(raw_q, target, vel_h) },
                            FfiResult::OK
                        );
                    }
                    10 => {
                        // Valid POD / ZST / Dynamic insertion
                        let target = if !live_entities.is_empty() && rng.gen_bool() {
                            live_entities[rng.range(live_entities.len())]
                        } else {
                            let mut e = FfiEntity::NULL;
                            let _ = unsafe { host_cmd_spawn_empty(raw_q, &mut e) };
                            live_entities.push(e);
                            e
                        };
                        match rng.range(3) {
                            0 => {
                                let vel = Velocity {
                                    x: step as f32,
                                    y: -1.0,
                                    z: 2.5,
                                };
                                assert_eq!(
                                    unsafe {
                                        host_cmd_insert(
                                            raw_q,
                                            target,
                                            vel_h,
                                            (&vel as *const Velocity).cast::<u8>(),
                                            core::mem::size_of::<Velocity>(),
                                        )
                                    },
                                    FfiResult::OK
                                );
                            }
                            1 => {
                                assert_eq!(
                                    unsafe {
                                        host_cmd_insert(raw_q, target, zst_h, core::ptr::null(), 0)
                                    },
                                    FfiResult::OK
                                );
                            }
                            _ => {
                                let val = (step as u64).to_ne_bytes();
                                assert_eq!(
                                    unsafe {
                                        host_cmd_insert(
                                            raw_q,
                                            target,
                                            dyn_h,
                                            val.as_ptr(),
                                            val.len(),
                                        )
                                    },
                                    FfiResult::OK
                                );
                            }
                        }
                    }
                    _ => {
                        // Fuzz `host_get_proc_address` + `host_last_error` (`C-04`)
                        match rng.range(3) {
                            0 => {
                                let sym = unsafe {
                                    host_get_proc_address(
                                        host_ctx,
                                        c"bam.v0.testbed::Velocity.vtable".as_ptr(),
                                    )
                                };
                                assert_eq!(sym, Velocity::erased_vtable_ptr());
                            }
                            1 => {
                                let miss = CString::new(format!("bam.v0.Missing_{step}")).unwrap();
                                assert!(
                                    unsafe { host_get_proc_address(host_ctx, miss.as_ptr()) }
                                        .is_null()
                                );
                                assert!(
                                    unsafe { host_last_error(host_ctx, core::ptr::null_mut(), 0) }
                                        > 0
                                );
                            }
                            _ => {
                                let bad_sym = rng.invalid_utf8_cstring();
                                assert!(
                                    unsafe {
                                        host_get_proc_address(
                                            host_ctx,
                                            bad_sym.as_ptr().cast::<c_char>(),
                                        )
                                    }
                                    .is_null()
                                );
                            }
                        }
                    }
                }
            }

            if step % 25 == 24 {
                let snap = app.world().resource::<ModRegistry>().snapshot_arc();
                storage.apply_to_world(app.world_mut(), &snap);
            }
        }

        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        storage.apply_to_world(app.world_mut(), &snap);
        load_ctx.finish_init();
    }
}

mod host_api_boundary_and_proc_address {
    use crate::support::Position;
    use bevy_abi_mod_host::ModRegistry;
    use bevy_abi_mod_host_sys::{
        HOST_API, LoadContext, host_get_proc_address, host_load_host_ctx, host_register_component,
    };
    use bevy_abi_mod_sys::{
        ABI_VERSION, BamComponent, BamHostApi, ComponentDesc, ComponentUuid, FfiResult,
        ModComponent as _, abi_version_is_compatible, pack_abi_version, read_descriptor,
    };
    use core::ffi::{c_char, c_void};
    use std::ffi::CString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    /// Truncated 40-byte `BamHostApi` prefix simulating a corrupt/older host table.
    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    struct TruncatedHostApi40 {
        struct_size: u32,
        abi_version: u32,
        get_proc_address: usize,
        last_error: usize,
        load_host_ctx: usize,
        register_component: usize,
    }

    #[repr(C)]
    struct TruncatedHostMemoryBuffer {
        truncated_api: TruncatedHostApi40,
        canary_bytes: [u8; 8],
    }

    /// Extended 240-byte future minor `BamHostApi` (`struct_size > MIN_V0_2_SIZE`).
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ExtendedFutureHostApi {
        base: BamHostApi,
        future_slot_0: usize,
        future_slot_1: usize,
    }

    #[test]
    fn test_legacy_host_api_layout_assumptions() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(core::mem::size_of::<TruncatedHostApi40>(), 40);
            assert_eq!(core::mem::align_of::<TruncatedHostApi40>(), 8);
            assert_eq!(core::mem::size_of::<BamHostApi>(), 312);
            assert_eq!(BamHostApi::MIN_V0_2_SIZE, 224);
            assert_eq!(core::mem::align_of::<BamHostApi>(), 8);
        }
        // SemVer pre-1.0 vs post-1.0 rules (`C-06`)
        assert!(abi_version_is_compatible(
            pack_abi_version(0, 2, 0),
            pack_abi_version(0, 2, 5)
        ));
        assert!(!abi_version_is_compatible(
            pack_abi_version(0, 3, 0),
            pack_abi_version(0, 2, 0)
        ));
        assert!(abi_version_is_compatible(
            pack_abi_version(1, 3, 0),
            pack_abi_version(1, 2, 0)
        ));
        assert!(!abi_version_is_compatible(
            pack_abi_version(1, 1, 0),
            pack_abi_version(1, 2, 0)
        ));
    }

    #[test]
    fn test_legacy_mod_on_newer_72_byte_host() {
        // Verify a v0.2 guest safely reads an extended future `ExtendedFutureHostApi` (`240` bytes > `224`)
        // via bounded `read_extensible_struct::<BamHostApi>` (`C-02`, `C-08`).
        let mut future_api = ExtendedFutureHostApi {
            base: HOST_API,
            future_slot_0: 0xDEAD_BEEF,
            future_slot_1: 0xCAFE_BABE,
        };
        future_api.base.struct_size = core::mem::size_of::<ExtendedFutureHostApi>() as u32;

        let bounded: BamHostApi = unsafe {
            read_descriptor((&future_api as *const ExtendedFutureHostApi).cast::<c_void>())
                .expect("bounded read of extended BamHostApi must succeed")
        };
        assert_eq!(bounded.validate_for_guest(ABI_VERSION), FfiResult::OK);
        assert_eq!(
            bounded.register_component.unwrap() as *const () as usize,
            host_register_component as *const () as usize
        );
    }

    #[test]
    fn test_newer_mod_on_older_64_byte_host_boundary_guard() {
        // Truncated 40-byte buffer followed immediately by canary bytes: `read_descriptor`
        // and `covers_slot` must reject out-of-bounds slots without touching canary memory!
        let buffer = TruncatedHostMemoryBuffer {
            truncated_api: TruncatedHostApi40 {
                struct_size: core::mem::size_of::<TruncatedHostApi40>() as u32,
                abi_version: ABI_VERSION,
                get_proc_address: host_get_proc_address as *const () as usize,
                last_error: 0x1111,
                load_host_ctx: 0x2222,
                register_component: host_register_component as *const () as usize,
            },
            canary_bytes: [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE],
        };

        let raw_ptr = (&buffer.truncated_api as *const TruncatedHostApi40).cast::<c_void>();
        let copy_res = unsafe { read_descriptor::<BamHostApi>(raw_ptr) };
        assert_eq!(copy_res.unwrap_err(), FfiResult::INVALID_VERSION);

        let canary_read = unsafe {
            core::ptr::read_unaligned(
                (raw_ptr as *const u8)
                    .add(core::mem::size_of::<TruncatedHostApi40>())
                    .cast::<[u8; 8]>(),
            )
        };
        assert_eq!(
            canary_read,
            [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]
        );
    }

    #[test]
    fn test_host_get_proc_address_stress_and_adversarial_queries() {
        static CUSTOM_SYM_TARGET: u8 = 0x42;
        let mut registry = ModRegistry::new();
        registry.register_type::<Position>();
        unsafe {
            registry.register_custom_symbol(
                "bam.v0.stress.hook",
                (&CUSTOM_SYM_TARGET as *const u8).cast::<c_void>(),
            );
        }

        let mut load_ctx = LoadContext::new(registry);
        let hctx = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };

        // 1. Valid `"bam.v0.*"` symbols
        assert_eq!(
            unsafe { host_get_proc_address(hctx, c"bam.v0.testbed::Position.vtable".as_ptr()) },
            Position::erased_vtable_ptr()
        );
        assert_eq!(
            unsafe { host_get_proc_address(hctx, c"bam.v0.stress.hook".as_ptr()) },
            (&CUSTOM_SYM_TARGET as *const u8).cast::<c_void>()
        );

        // 2. Null & empty inputs
        assert!(unsafe { host_get_proc_address(hctx, core::ptr::null()) }.is_null());
        assert!(
            unsafe { host_get_proc_address(core::ptr::null(), c"bam.v0.stress.hook".as_ptr()) }
                .is_null()
        );
        assert!(unsafe { host_get_proc_address(hctx, c"".as_ptr()) }.is_null());

        // 3. Near misses, case-sensitivity, missing `"bam.v0."` prefix, and whitespace
        let rejected = [
            c"stress.hook".as_ptr(),
            c"testbed::Position.vtable".as_ptr(),
            c"bam.v0.stress.hoo".as_ptr(),
            c"bam.v0.stress.hooks".as_ptr(),
            c"bam.v0.Stress.Hook".as_ptr(),
            c"BAM.V0.stress.hook".as_ptr(),
            c" bam.v0.stress.hook".as_ptr(),
            c"bam.v0.stress.hook ".as_ptr(),
            c"bam.v0.stress.hook\n".as_ptr(),
        ];
        for &q in &rejected {
            assert!(unsafe { host_get_proc_address(hctx, q) }.is_null());
        }

        // 4. Invalid UTF-8 & 10,000-byte string
        let bad_utf8 = [0xffu8, 0xfe, 0xfd, 0x00];
        assert!(
            unsafe { host_get_proc_address(hctx, bad_utf8.as_ptr().cast::<c_char>()) }.is_null()
        );

        let c_long = CString::new(format!("bam.v0.{}", "A".repeat(10_000))).unwrap();
        assert!(unsafe { host_get_proc_address(hctx, c_long.as_ptr()) }.is_null());

        load_ctx.finish_init();
    }

    #[test]
    fn test_host_get_proc_address_high_concurrency() {
        let mut registry = ModRegistry::new();
        registry.register_type::<Position>();

        const NUM_THREADS: usize = 8;
        const QUERIES_PER_THREAD: usize = 5_000;

        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let reg_clone = registry.clone();
                thread::spawn(move || {
                    let mut load_ctx = LoadContext::new(reg_clone);
                    let hctx = unsafe { host_load_host_ctx(load_ctx.as_ffi_ptr()) };
                    for i in 0..QUERIES_PER_THREAD {
                        let vt = unsafe {
                            host_get_proc_address(hctx, c"bam.v0.testbed::Position.vtable".as_ptr())
                        };
                        assert_eq!(vt, Position::erased_vtable_ptr());

                        let gx = unsafe {
                            host_get_proc_address(hctx, c"bam.v0.testbed::Position.get_x".as_ptr())
                        };
                        assert!(!gx.is_null());

                        if i % 10 == 0 {
                            assert!(
                                unsafe {
                                    host_get_proc_address(
                                        hctx,
                                        c"bam.v0.non_existent_concurrent_query".as_ptr(),
                                    )
                                }
                                .is_null()
                            );
                        }
                    }
                    load_ctx.finish_init();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    unsafe fn call_register_component_fresh(desc: &ComponentDesc) -> FfiResult {
        let mut world = bevy_ecs::world::World::new();
        let reg = ModRegistry::new();
        world.insert_resource(reg.clone());
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            &mut world as *mut _,
            None,
        );
        let mut out = BamComponent::INVALID;
        let res = unsafe { host_register_component(load_ctx.as_ffi_ptr(), desc, &mut out) };
        load_ctx.finish_init();
        res
    }

    #[test]
    fn test_host_register_component_extensive_stress_matrix() {
        static DROP_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn custom_test_drop_hook(ptr: *mut c_void) {
            if !ptr.is_null() {
                DROP_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        let name_ascii = c"stress::StressComponent".as_ptr();
        let uuid = ComponentUuid::from_parts("stress::StressComponent", 16, 8, 0);

        // 1. ZST matrix with power-of-two alignments
        for align in [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096] {
            let desc = ComponentDesc::new(name_ascii, uuid, 0, align);
            assert!(desc.is_valid());
            assert!(desc.is_zst());
            assert!(desc.is_pod());
            assert_eq!(
                unsafe { call_register_component_fresh(&desc) },
                FfiResult::OK
            );
        }

        // 2. Over-aligned non-ZST types
        for (size, align) in [
            (16, 16),
            (32, 16),
            (64, 64),
            (128, 64),
            (256, 256),
            (1024, 512),
            (4096, 4096),
        ] {
            let desc = ComponentDesc::new(name_ascii, uuid, size, align);
            assert_eq!(
                unsafe { call_register_component_fresh(&desc) },
                FfiResult::OK
            );
        }

        // 3. Invalid non-power-of-two alignments
        for align in [0, 3, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 24, 100, 333] {
            let mut desc = ComponentDesc::new(name_ascii, uuid, align * 2, 1);
            desc.align = align;
            assert!(!desc.is_valid());
            assert_eq!(
                unsafe { call_register_component_fresh(&desc) },
                FfiResult::INVALID_ARGUMENT
            );
        }

        // 4. Misaligned sizes (`size % align != 0`)
        for (size, align) in [
            (1, 2),
            (3, 4),
            (7, 8),
            (15, 8),
            (17, 16),
            (31, 32),
            (65, 64),
        ] {
            let desc = ComponentDesc::new(name_ascii, uuid, size, align);
            assert!(!desc.is_valid());
            assert_eq!(
                unsafe { call_register_component_fresh(&desc) },
                FfiResult::INVALID_ARGUMENT
            );
        }

        // 5. Extended vs truncated `struct_size`
        for sz in [64u32, 72, 80, 96, 128, 256, 1024, 65536] {
            let mut desc = ComponentDesc::new(name_ascii, uuid, 16, 8);
            desc.struct_size = sz;
            assert_eq!(
                unsafe { call_register_component_fresh(&desc) },
                FfiResult::OK
            );
        }
        for sz in [
            0u32,
            1,
            4,
            8,
            16,
            32,
            40,
            (ComponentDesc::MIN_V0_2_SIZE - 1) as u32,
        ] {
            let mut desc = ComponentDesc::new(name_ascii, uuid, 16, 8);
            desc.struct_size = sz;
            assert_eq!(
                unsafe { call_register_component_fresh(&desc) },
                FfiResult::INVALID_VERSION
            );
        }

        // 6. Non-ASCII UTF-8 component names
        for s in [
            "stress::生命值",
            "stress::Компонент",
            "stress::CaféAuLaitTag",
        ] {
            let c_name = CString::new(s).unwrap();
            let desc = ComponentDesc::new(c_name.as_ptr(), uuid, 8, 8);
            assert_eq!(
                unsafe { call_register_component_fresh(&desc) },
                FfiResult::OK
            );
        }

        // 7. Drop hook + VTable pointer preservation
        let drop_desc =
            ComponentDesc::new(name_ascii, uuid, 16, 8).with_drop(custom_test_drop_hook);
        assert!(!drop_desc.is_pod());
        assert_eq!(
            unsafe { call_register_component_fresh(&drop_desc) },
            FfiResult::OK
        );

        DROP_CALL_COUNT.store(0, Ordering::SeqCst);
        let mut dummy_val = 0x1234_u64;
        unsafe {
            (drop_desc.drop_fn.unwrap())((&mut dummy_val as *mut u64).cast::<c_void>());
        }
        assert_eq!(DROP_CALL_COUNT.load(Ordering::SeqCst), 1);

        let dummy_vtable = 0x9999_usize as *const c_void;
        let vtable_desc = ComponentDesc::new(name_ascii, uuid, 8, 8).with_vtable(dummy_vtable);
        assert_eq!(vtable_desc.vtable_ptr, dummy_vtable);
        assert_eq!(
            unsafe { call_register_component_fresh(&vtable_desc) },
            FfiResult::OK
        );
    }
}

mod dynamic_descriptor_layouts {
    use bevy_abi_mod_host::ModRegistry;
    use bevy_abi_mod_host_sys::{HOST_API, LoadContext, host_register_component};
    use bevy_abi_mod_sys::{
        BamComponent, BamHostApi, BamRegisterComponentFn, ComponentDesc, ComponentUuid, FfiResult,
    };
    use bevy_ecs::world::World;
    use core::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    unsafe fn call_register_component_with_fn(
        reg_fn: BamRegisterComponentFn,
        desc: &ComponentDesc,
    ) -> FfiResult {
        let mut world = World::new();
        let reg = ModRegistry::new();
        world.insert_resource(reg.clone());
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            &mut world as *mut _,
            None,
        );
        let mut out = BamComponent::INVALID;
        let res = unsafe { reg_fn(load_ctx.as_ffi_ptr(), desc, &mut out) };
        load_ctx.finish_init();
        res
    }

    unsafe fn call_register_component_fresh(desc: &ComponentDesc) -> FfiResult {
        unsafe { call_register_component_with_fn(host_register_component, desc) }
    }

    #[test]
    fn test_massive_struct_descriptors() {
        let name = c"layout::MassiveComponent".as_ptr();
        let uuid = ComponentUuid::from_parts("layout::MassiveComponent", 1024 * 1024, 16, 0);

        let size_1mb = 1024 * 1024;
        let desc_1mb = ComponentDesc::new(name, uuid, size_1mb, 16);
        assert!(desc_1mb.is_valid());
        assert_eq!(
            unsafe { call_register_component_fresh(&desc_1mb) },
            FfiResult::OK
        );

        let size_64mb = 64 * 1024 * 1024;
        let desc_64mb = ComponentDesc::new(name, uuid, size_64mb, 64);
        assert!(desc_64mb.is_valid());
        assert_eq!(
            unsafe { call_register_component_fresh(&desc_64mb) },
            FfiResult::OK
        );
    }

    #[test]
    fn test_malformed_utf8_names() {
        let uuid = ComponentUuid::from_canonical_name("layout::BadUtf8Comp");
        let bad_utf8_sequences: [&[u8]; 6] = [
            &[0xFF, 0x00],
            &[0xFE, 0x00],
            &[0xC0, 0xAF, 0x00],
            &[0xED, 0xA0, 0x80, 0x00],
            &[0xF4, 0x90, 0x80, 0x80, 0x00],
            &[0xE2, 0x82, 0x00],
        ];

        for (idx, &bad_bytes) in bad_utf8_sequences.iter().enumerate() {
            let desc = ComponentDesc::new(bad_bytes.as_ptr().cast(), uuid, 8, 8);
            assert_eq!(
                unsafe { call_register_component_fresh(&desc) },
                FfiResult::INVALID_UTF8,
                "Sequence {idx} must be rejected with INVALID_UTF8"
            );
        }
    }

    #[test]
    fn test_host_api_layout_stability_and_byte_offsets() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(BamHostApi::MIN_V0_2_SIZE, 224);
            assert_eq!(core::mem::size_of::<BamHostApi>(), 312);
            assert_eq!(core::mem::align_of::<BamHostApi>(), 8);
            assert_eq!(core::mem::offset_of!(BamHostApi, struct_size), 0);
            assert_eq!(core::mem::offset_of!(BamHostApi, abi_version), 4);
            assert_eq!(core::mem::offset_of!(BamHostApi, get_proc_address), 8);
            assert_eq!(core::mem::offset_of!(BamHostApi, last_error), 16);
            assert_eq!(core::mem::offset_of!(BamHostApi, load_host_ctx), 24);
            assert_eq!(core::mem::offset_of!(BamHostApi, register_component), 32);
            assert_eq!(core::mem::offset_of!(BamHostApi, resolve_component), 40);
            assert_eq!(core::mem::offset_of!(BamHostApi, register_system), 48);
            assert_eq!(core::mem::offset_of!(BamHostApi, sys_host_ctx), 56);
            assert_eq!(core::mem::offset_of!(BamHostApi, sys_query), 64);
            assert_eq!(core::mem::offset_of!(BamHostApi, sys_commands), 72);
            assert_eq!(core::mem::offset_of!(BamHostApi, iter_next_chunk), 80);
            assert_eq!(core::mem::offset_of!(BamHostApi, iter_reset), 88);
            assert_eq!(core::mem::offset_of!(BamHostApi, iter_entities), 96);
            assert_eq!(core::mem::offset_of!(BamHostApi, iter_column_readonly), 104);
            assert_eq!(core::mem::offset_of!(BamHostApi, iter_column_mutable), 112);
            assert_eq!(
                core::mem::offset_of!(BamHostApi, iter_column_mutable_untracked),
                120
            );
            assert_eq!(
                core::mem::offset_of!(BamHostApi, iter_mark_changed_range),
                128
            );
            assert_eq!(
                core::mem::offset_of!(BamHostApi, iter_column_rows_readonly),
                136
            );
            assert_eq!(
                core::mem::offset_of!(BamHostApi, iter_column_rows_mutable),
                144
            );
            assert_eq!(core::mem::offset_of!(BamHostApi, iter_column_info), 152);
            assert_eq!(core::mem::offset_of!(BamHostApi, res_ptr), 160);
            assert_eq!(core::mem::offset_of!(BamHostApi, res_mut_ptr), 168);
            assert_eq!(core::mem::offset_of!(BamHostApi, cmd_spawn_empty), 176);
            assert_eq!(core::mem::offset_of!(BamHostApi, cmd_spawn), 184);
            assert_eq!(core::mem::offset_of!(BamHostApi, cmd_despawn), 192);
            assert_eq!(core::mem::offset_of!(BamHostApi, cmd_insert), 200);
            assert_eq!(core::mem::offset_of!(BamHostApi, cmd_insert_default), 208);
            assert_eq!(core::mem::offset_of!(BamHostApi, cmd_remove), 216);
            assert_eq!(core::mem::offset_of!(BamHostApi, par_for_each_chunk), 224);
            assert_eq!(core::mem::offset_of!(BamHostApi, sys_entity_get), 232);
            assert_eq!(core::mem::offset_of!(BamHostApi, sys_entity_get_mut), 240);
            assert_eq!(core::mem::offset_of!(BamHostApi, msg_read_batch), 248);
            assert_eq!(core::mem::offset_of!(BamHostApi, msg_send), 256);
        }
    }

    #[test]
    fn test_callable_invocation_parity_matrix() {
        let name = c"layout::ParityComponent".as_ptr();
        let uuid = ComponentUuid::from_parts("layout::ParityComponent", 32, 8, 0);

        let valid_desc = ComponentDesc::new(name, uuid, 32, 8);
        let mut truncated_desc = valid_desc;
        truncated_desc.struct_size = 32;
        let mut misaligned_desc = valid_desc;
        misaligned_desc.size = 15;
        let bad_utf8 = [0xFFu8, 0x00];
        let bad_utf8_desc = ComponentDesc::new(bad_utf8.as_ptr().cast(), uuid, 32, 8);
        let bad_flags_desc = valid_desc.with_flags(1 << 20);

        let descriptors: [(&str, ComponentDesc, FfiResult); 5] = [
            ("valid", valid_desc, FfiResult::OK),
            ("truncated", truncated_desc, FfiResult::INVALID_VERSION),
            ("misaligned", misaligned_desc, FfiResult::INVALID_ARGUMENT),
            ("bad_utf8", bad_utf8_desc, FfiResult::INVALID_UTF8),
            ("bad_flags", bad_flags_desc, FfiResult::UNSUPPORTED),
        ];

        let direct_fn: BamRegisterComponentFn = host_register_component;
        let host_api_fn: BamRegisterComponentFn = HOST_API.register_component.unwrap();
        assert_eq!(
            direct_fn as *const () as usize,
            host_api_fn as *const () as usize
        );

        for (label, desc, expected_res) in descriptors {
            let r1 = unsafe { call_register_component_with_fn(direct_fn, &desc) };
            let r2 = unsafe { call_register_component_with_fn(host_api_fn, &desc) };
            assert_eq!(r1, expected_res, "{label} via direct_fn");
            assert_eq!(r2, expected_res, "{label} via host_api_fn");
        }
    }

    #[test]
    fn test_descriptor_with_drop_and_vtable() {
        static DROP_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn test_custom_drop(ptr: *mut c_void) {
            if !ptr.is_null() {
                DROP_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        let name = c"layout::CustomDropComp".as_ptr();
        let uuid = ComponentUuid::from_parts("layout::CustomDropComp", 16, 8, 0);
        let fake_vtable: usize = 0xcafe_babe;
        let desc = ComponentDesc::new(name, uuid, 16, 8)
            .with_drop(test_custom_drop)
            .with_vtable(fake_vtable as *const c_void);

        assert!(!desc.is_pod());
        assert_eq!(desc.vtable_ptr, fake_vtable as *const c_void);
        assert_eq!(
            unsafe { call_register_component_fresh(&desc) },
            FfiResult::OK
        );

        let mut payload = [42u8; 16];
        let before = DROP_CALL_COUNT.load(Ordering::SeqCst);
        unsafe { (desc.drop_fn.unwrap())(payload.as_mut_ptr().cast::<c_void>()) };
        assert_eq!(DROP_CALL_COUNT.load(Ordering::SeqCst), before + 1);
    }

    #[test]
    fn test_extreme_size_and_alignment_limits() {
        let name = c"layout::ExtremeComp".as_ptr();
        let uuid = ComponentUuid::from_canonical_name("layout::ExtremeComp");

        let desc_usize_max = ComponentDesc::new(name, uuid, usize::MAX, 1);
        assert!(desc_usize_max.layout().is_none());
        assert_eq!(
            unsafe { call_register_component_fresh(&desc_usize_max) },
            FfiResult::INVALID_ARGUMENT
        );

        let desc_max_rust_align = ComponentDesc::new(name, uuid, 0, 1usize << 29);
        assert!(desc_max_rust_align.layout().is_some());
        assert_eq!(
            unsafe { call_register_component_fresh(&desc_max_rust_align) },
            FfiResult::OK
        );
    }
}

mod transparent_ffi_primitives {
    use bevy_abi_mod_sys::{AccessMode, BamComponent, FfiBool, FfiEntity, FfiResult, ScheduleId};

    #[test]
    fn test_ffi_transparent_type_safety() {
        let valid_ok = FfiResult::OK;
        assert!(valid_ok.is_ok());
        assert!(!valid_ok.is_err());

        let panic_res = FfiResult::PANIC_CAUGHT;
        assert!(!panic_res.is_ok());
        assert!(panic_res.is_err());

        let foreign_code = FfiResult(0xCAFE_BABE);
        assert!(foreign_code.is_err());
        assert!(FfiResult(0).is_ok());

        // `FfiBool(u8)` arbitrary foreign byte safety (`0` = false, `1..=255` = true)
        assert!(!FfiBool(0).get());
        assert!(FfiBool(1).get());
        assert!(FfiBool(2).get());
        assert!(FfiBool(255).get());

        assert_eq!(AccessMode::READ.0, 0);
        assert_eq!(AccessMode::WRITE.0, 1);
        assert_eq!(AccessMode::FILTER_ONLY.0, 2);
        assert_eq!(AccessMode::WITHOUT.0, 3);
        assert_eq!(AccessMode::OPTIONAL_READ.0, 4);
        assert_eq!(AccessMode::OPTIONAL_WRITE.0, 5);
        assert_eq!(AccessMode::CHANGED.0, 6);
        assert_eq!(AccessMode::ADDED.0, 7);
        assert_eq!(AccessMode::RESOURCE_READ.0, 8);
        assert_eq!(AccessMode::RESOURCE_WRITE.0, 9);

        assert!(!BamComponent::INVALID.is_valid());
        assert!(BamComponent::from_raw(1).is_valid());

        assert!(ScheduleId::UPDATE.is_valid());
        assert!(!ScheduleId(6).is_valid());
        assert!(ScheduleId::custom(1024).is_valid());

        let ent = FfiEntity(0x0000_0001_0000_0005);
        assert_eq!(ent.0, 0x0000_0001_0000_0005);
        assert_eq!(FfiEntity::NULL.0, 0);
        assert!(FfiEntity::NULL.is_null());
    }
}

mod query_limits_and_placeholder_entities {
    use crate::support::Position;
    use bevy_abi_mod_host::{ComponentOptions, ModRegistry};
    use bevy_abi_mod_host_sys::{
        HostCommands, HostCommandsStorage, LoadContext, host_cmd_despawn, host_cmd_insert,
        host_cmd_insert_default, host_cmd_remove, host_cmd_spawn, host_cmd_spawn_empty,
        host_register_component, host_register_system,
    };
    use bevy_abi_mod_sys::{
        BamComponent, BamQueryDesc, BamSystemCtx, BamSystemDesc, BamTermDesc, ComponentDesc,
        ComponentUuid, FfiEntity, FfiResult, MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY,
    };
    use bevy_ecs::entity::Entity;
    use bevy_ecs::world::World;
    use core::ffi::c_void;
    use core::sync::atomic::AtomicBool;
    use std::sync::Arc;

    unsafe extern "C" fn noop_cb(_sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        FfiResult::OK
    }

    #[test]
    fn d8_rejects_query_count_exceeding_max_query_components() {
        let mut load_ctx = LoadContext::new(ModRegistry::new());
        let ctx_ptr = load_ctx.as_ffi_ptr();

        // 1. `term_count == MAX_TERMS_PER_QUERY + 1` (`33`) -> `CAPACITY_EXCEEDED`
        let terms: Vec<BamTermDesc> = (0..=MAX_TERMS_PER_QUERY)
            .map(|i| BamTermDesc::read(BamComponent::from_raw((i + 1) as u32)))
            .collect();
        let over_terms_q = BamQueryDesc::new(terms.as_ptr(), terms.len());
        let single_desc = BamSystemDesc::new(
            c"too_many_cols".as_ptr(),
            &over_terms_q,
            1,
            noop_cb,
            core::ptr::null_mut(),
        );
        assert_eq!(
            unsafe { host_register_system(ctx_ptr, &single_desc) },
            FfiResult::CAPACITY_EXCEEDED
        );
        assert!(load_ctx.registered_systems.is_empty());

        // 2. `query_count == MAX_QUERIES_PER_SYSTEM + 1` (`9`) -> `CAPACITY_EXCEEDED`
        let dummy_q = BamQueryDesc::empty();
        let over_queries = [dummy_q; MAX_QUERIES_PER_SYSTEM + 1];
        let multi_desc = BamSystemDesc::new(
            c"too_many_queries".as_ptr(),
            over_queries.as_ptr(),
            over_queries.len(),
            noop_cb,
            core::ptr::null_mut(),
        );
        assert_eq!(
            unsafe { host_register_system(ctx_ptr, &multi_desc) },
            FfiResult::CAPACITY_EXCEEDED
        );
        assert!(load_ctx.registered_systems.is_empty());
        load_ctx.finish_init();
    }

    #[test]
    fn d9_and_d10_null_placeholder_entities_rejected_and_overaligned_zst_succeeds() {
        let mut world = World::new();
        let mut reg = ModRegistry::new();
        let pos_id = world.register_component::<Position>();
        let pos_h = reg
            .register_component::<Position>(pos_id, ComponentOptions::new().with_default())
            .unwrap();
        world.insert_resource(reg.clone());

        let mut zst_h = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            let zst_desc = ComponentDesc::new(
                c"d10::Aligned64Zst".as_ptr(),
                ComponentUuid::from_parts("d10::Aligned64Zst", 0, 64, 0),
                0,
                64,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &zst_desc, &mut zst_h) },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let zst_id = snap.get_by_handle(zst_h).unwrap().component_id;

        // D9: null queue returns `NULL_POINTER`
        let mut out_ent = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(core::ptr::null_mut(), &mut out_ent) },
            FfiResult::NULL_POINTER
        );

        let mut storage = HostCommandsStorage::new();
        let (valid_ffi, valid_batch_ffi) = {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_q = cmds.as_ffi_ptr();

            let placeholder_ffi = FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits());
            let pos_bytes = [0u8; core::mem::size_of::<Position>()];

            for bad in [FfiEntity::NULL, placeholder_ffi] {
                assert_eq!(
                    unsafe { host_cmd_despawn(raw_q, bad) },
                    FfiResult::INVALID_ENTITY
                );
                assert_eq!(
                    unsafe {
                        host_cmd_insert(raw_q, bad, pos_h, pos_bytes.as_ptr(), pos_bytes.len())
                    },
                    FfiResult::INVALID_ENTITY
                );
                assert_eq!(
                    unsafe { host_cmd_insert_default(raw_q, bad, pos_h) },
                    FfiResult::INVALID_ENTITY
                );
                assert_eq!(
                    unsafe { host_cmd_remove(raw_q, bad, pos_h) },
                    FfiResult::INVALID_ENTITY
                );
            }

            // D10: Insert 64-byte-aligned ZST onto spawned entities via both `cmd_insert` and `cmd_spawn`!
            let mut e1 = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_q, &mut e1) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe { host_cmd_insert(raw_q, e1, zst_h, core::ptr::null(), 0) },
                FfiResult::OK
            );

            let comps = [zst_h];
            let ptrs = [core::ptr::null::<u8>()];
            let mut e2 = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn(raw_q, comps.as_ptr(), ptrs.as_ptr(), 1, &mut e2) },
                FfiResult::OK
            );
            (e1, e2)
        };

        storage.apply_to_world(&mut world, &snap);
        for ffi_e in [valid_ffi, valid_batch_ffi] {
            let ent = Entity::try_from_bits(ffi_e.to_bits()).unwrap();
            assert!(world.entity(ent).contains_id(zst_id));
        }
    }
}
