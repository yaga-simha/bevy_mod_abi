//! Adversarial ABI boundary validation, struct-size negotiation, descriptor layout, and fuzz matrices.

mod fuzz_boundaries {
    #![allow(unused_imports, dead_code)]
    //! Adversarial / Fuzz-Style C-ABI Boundary Verification Suite.
    //!
    //! Deterministically fuzzes all 16 `host_*` `extern "C"` entry points across thousands of
    //! malformed, truncated, out-of-bounds, overflow, non-UTF-8, forged-entity, and permission-violating
    //! inputs using a pure-Rust SplitMix64 PRNG (zero external crate dependencies).
    //!
    //! Verified invariants across every generated input:
    //! - Zero panics across `extern "C"` boundaries (and zero panics when `CommandQueue` flushes to `World`).
    //! - Zero Undefined Behavior (exact-length truncated buffers, raw-byte zeroed `fn` pointer slots,
    //!   validated under Miri Tree Borrows).
    //! - Deterministic, clean `FfiResult` error codes on all invalid inputs.

    use crate::support::{NameTag, Position, Velocity};
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, HostChunkIter, MAX_QUERY_COMPONENTS, MAX_SYSTEM_QUERIES,
        ModInitContext, QueryColumnDesc, host_cmd_despawn, host_cmd_insert_component,
        host_cmd_spawn_empty, host_get_proc_address, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_column_stride, host_iter_entities,
        host_iter_mark_changed, host_iter_next_chunk, host_iter_reset, host_register_component,
        host_register_multi_query_system, host_register_scheduled_system, host_register_system,
        host_register_system_in_schedule,
    };
    use bevy_abi_mod_sys::{
        ComponentAccessDesc, ComponentAccessMode, ComponentUuid, ExposableComponent, FfiEntity,
        FfiResult, ModComponentDescriptor, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter,
        RawCommandQueue, ScheduleLabelId, ScheduledMultiQuerySystemDescriptor,
        ScheduledSystemDescriptor, SystemDescriptor,
    };
    use bevy_app::App;
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::query::QueryBuilder;
    use bevy_ecs::world::{CommandQueue, World};
    use core::ffi::{c_char, c_void};
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
            // Ensure at least one guaranteed invalid UTF-8 byte (0xFF or lone continuation 0x80).
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
    #[derive(Component, bevy_abi_mod_host::ModExposable, Debug, Clone, Copy, PartialEq)]
    pub struct FuzzBoolComp {
        pub flag: bool,
        pub code: u32,
    }

    #[repr(C)]
    #[derive(Component, bevy_abi_mod_host::ModExposable, Debug, Clone, Copy, PartialEq)]
    #[component(storage = "SparseSet")]
    pub struct FuzzSparseVal {
        pub v: u64,
    }

    #[repr(C)]
    #[derive(Component, bevy_abi_mod_host::ModExposable, Debug, Clone, Copy, PartialEq)]
    pub struct FuzzZst;

    unsafe extern "C" fn dummy_single_cb(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    unsafe extern "C" fn dummy_multi_cb(
        _iters: *const *mut RawChunkIter,
        _iter_count: usize,
        _commands: *mut RawCommandQueue,
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

        // SAFETY: `template` is `Copy` and initialized for `full_size` bytes (`#[repr(C)]` descriptor).
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
    // 1. Fuzz `host_register_system`, `host_register_scheduled_system`, & `host_register_system_in_schedule`
    // ============================================================================

    #[test]
    fn fuzz_host_register_system_family_adversarial_inputs() {
        let registry = Arc::new(ModRegistry::new());
        let mut init_ctx = ModInitContext::new(Arc::clone(&registry));

        // Exhaustive `struct_size` sweep 0..128 + u32::MAX (both aligned and 1-byte misaligned!)
        let valid_base = SystemDescriptor::new(
            c"valid_sys".as_ptr(),
            core::ptr::null(),
            0,
            dummy_single_cb,
            core::ptr::null_mut(),
        )
        .with_schedule(ScheduleLabelId::UPDATE);

        for misalign in [0usize, 1usize] {
            for sz in (0u32..128).chain(core::iter::once(u32::MAX)) {
                let _guard = init_ctx.make_active();
                let ctx_ptr = core::ptr::addr_of_mut!(init_ctx).cast::<c_void>();
                let (_buf, raw_ptr) = make_sized_descriptor_buffer(&valid_base, sz, misalign);
                let min_v1 = SystemDescriptor::MIN_V1_SIZE as u32;

                // SAFETY: `raw_ptr` has exact readable prefix length for `read_descriptor`.
                let r_legacy =
                    unsafe { host_register_system(ctx_ptr, raw_ptr.cast::<SystemDescriptor>()) };
                // SAFETY: Same valid buffer passed to scheduled entry point.
                let r_sched = unsafe {
                    host_register_scheduled_system(
                        ctx_ptr,
                        raw_ptr.cast::<ScheduledSystemDescriptor>(),
                    )
                };

                if sz < min_v1 {
                    assert_eq!(
                        r_legacy,
                        FfiResult::INVALID_VERSION,
                        "legacy register_system must reject truncated struct_size={sz}"
                    );
                    assert_eq!(
                        r_sched,
                        FfiResult::INVALID_VERSION,
                        "register_scheduled_system must reject truncated struct_size={sz} with INVALID_VERSION"
                    );
                } else {
                    assert_eq!(r_legacy, FfiResult::OK, "struct_size={sz} should succeed");
                    assert_eq!(r_sched, FfiResult::OK, "struct_size={sz} should succeed");
                }
            }
        }

        // Null pointer & zeroed callback slot (niche UB protection!) checks
        // SAFETY: Testing explicit null guards.
        {
            let _guard = init_ctx.make_active();
            let ctx_ptr = core::ptr::addr_of_mut!(init_ctx).cast::<c_void>();
            unsafe {
                assert_eq!(
                    host_register_system(core::ptr::null_mut(), core::ptr::null()),
                    FfiResult::NULL_POINTER
                );
                assert_eq!(
                    host_register_system(ctx_ptr, core::ptr::null()),
                    FfiResult::NULL_POINTER
                );
                assert_eq!(
                    host_register_scheduled_system(core::ptr::null_mut(), core::ptr::null()),
                    FfiResult::NULL_POINTER
                );
                assert_eq!(
                    host_register_scheduled_system(ctx_ptr, core::ptr::null()),
                    FfiResult::NULL_POINTER
                );
                assert_eq!(
                    host_register_system_in_schedule(ctx_ptr, core::ptr::null(), 0),
                    FfiResult::NULL_POINTER
                );
            }

            // Zeroed callback pointer inside raw byte buffer (C caller passing `callback = NULL`)
            let mut zero_cb_bytes = vec![0u8; core::mem::size_of::<ScheduledSystemDescriptor>()];
            let full_sz = core::mem::size_of::<ScheduledSystemDescriptor>() as u32;
            zero_cb_bytes[..4].copy_from_slice(&full_sz.to_ne_bytes());
            // SAFETY: Fully zeroed buffer with valid `struct_size` tests `validate_raw_slot` null-fn guard.
            unsafe {
                assert_eq!(
                    host_register_system(
                        ctx_ptr,
                        zero_cb_bytes.as_ptr().cast::<SystemDescriptor>()
                    ),
                    FfiResult::NULL_POINTER
                );
                assert_eq!(
                    host_register_scheduled_system(
                        ctx_ptr,
                        zero_cb_bytes.as_ptr().cast::<ScheduledSystemDescriptor>()
                    ),
                    FfiResult::NULL_POINTER
                );
            }
        }

        // Randomized fuzz loop over malformed combinations
        let mut rng = Rng::new(0xC0FF_EE01_2026_1111);
        let valid_comp_names = [c"Position", c"Velocity", c"NameTag", c"FuzzSparseVal"];

        for step in 0..FUZZ_ITERS {
            init_ctx.registered_systems.clear();
            let _guard = init_ctx.make_active();
            let ctx_ptr = core::ptr::addr_of_mut!(init_ctx).cast::<c_void>();
            let mutation = rng.range(12);

            match mutation {
                0 => {
                    // Invalid `query_count > MAX_QUERY_COMPONENTS` (including `usize::MAX`) without dereferencing `queries`!
                    let bad_count = match rng.range(4) {
                        0 => MAX_QUERY_COMPONENTS + 1,
                        1 => MAX_QUERY_COMPONENTS + 1 + rng.range(10_000),
                        2 => usize::MAX,
                        _ => isize::MAX as usize,
                    };
                    let mut desc = valid_base;
                    desc.base.queries = core::ptr::null();
                    desc.base.query_count = bad_count;
                    // SAFETY: `query_count > MAX_QUERY_COMPONENTS` is validated before slice construction.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::OUT_OF_RANGE, "step {step}");
                    drop(_guard);
                    assert!(init_ctx.registered_systems.is_empty());
                }
                1 => {
                    // Non-zero `query_count` (`1..=MAX_QUERY_COMPONENTS`) with null `queries` pointer
                    let mut desc = valid_base;
                    desc.base.queries = core::ptr::null();
                    desc.base.query_count = 1 + rng.range(MAX_QUERY_COMPONENTS);
                    // SAFETY: Null `queries` check precedes `slice::from_raw_parts`.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                }
                2 => {
                    // Invalid `ComponentAccessMode(10..=u32::MAX)` inside query array
                    let q_len = 1 + rng.range(MAX_QUERY_COMPONENTS);
                    let bad_idx = rng.range(q_len);
                    let bad_mode = 10u32.saturating_add(rng.next_u32()).max(10);
                    let queries: Vec<ComponentAccessDesc> = (0..q_len)
                        .map(|i| ComponentAccessDesc {
                            name: valid_comp_names[i % valid_comp_names.len()].as_ptr(),
                            mode: if i == bad_idx {
                                ComponentAccessMode(bad_mode)
                            } else {
                                ComponentAccessMode(rng.range(4) as u32)
                            },
                        })
                        .collect();
                    let mut desc = valid_base;
                    desc.base.queries = queries.as_ptr();
                    desc.base.query_count = queries.len();
                    // SAFETY: `queries` slice and C strings are valid.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::INVALID_ARGUMENT, "step {step}");
                }
                3 => {
                    // Null component `name` inside `ComponentAccessDesc`
                    let q_len = 1 + rng.range(8);
                    let null_idx = rng.range(q_len);
                    let queries: Vec<ComponentAccessDesc> = (0..q_len)
                        .map(|i| ComponentAccessDesc {
                            name: if i == null_idx {
                                core::ptr::null()
                            } else {
                                valid_comp_names[i % valid_comp_names.len()].as_ptr()
                            },
                            mode: ComponentAccessMode::READ,
                        })
                        .collect();
                    let mut desc = valid_base;
                    desc.base.queries = queries.as_ptr();
                    desc.base.query_count = queries.len();
                    // SAFETY: Null `q.name` is explicitly checked.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                }
                4 => {
                    // Non-UTF8 component `name` inside `ComponentAccessDesc`
                    let bad_bytes = rng.invalid_utf8_cstring();
                    let q = ComponentAccessDesc {
                        name: bad_bytes.as_ptr().cast::<c_char>(),
                        mode: ComponentAccessMode(rng.range(4) as u32),
                    };
                    let mut desc = valid_base;
                    desc.base.queries = &q;
                    desc.base.query_count = 1;
                    // SAFETY: `bad_bytes` is NUL-terminated.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::INVALID_UTF8, "step {step}");
                }
                5 => {
                    // Non-UTF8 or null *system* `name` with valid queries falls back cleanly to `"unnamed_guest_system"`
                    let bad_sys_name = rng.invalid_utf8_cstring();
                    let mut desc = valid_base;
                    desc.base.name = if rng.gen_bool() {
                        core::ptr::null()
                    } else {
                        bad_sys_name.as_ptr().cast::<c_char>()
                    };
                    // SAFETY: `bad_sys_name` is NUL-terminated (or null).
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::OK, "step {step}");
                    drop(_guard);
                    assert_eq!(init_ctx.registered_systems.len(), 1);
                }
                6 => {
                    // Invalid `ScheduleLabelId(6..=u32::MAX)` in `ScheduledSystemDescriptor`
                    let bad_sched = match rng.range(3) {
                        0 => 6 + (rng.next_u32() % 1_000),
                        1 => u32::MAX,
                        _ => 6u32.saturating_add(rng.next_u32()),
                    }
                    .max(6);
                    let mut desc = valid_base;
                    desc.schedule = ScheduleLabelId(bad_sched);
                    // SAFETY: Valid descriptor buffer with invalid schedule discriminant.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::INVALID_VERSION, "step {step}");

                    let legacy_desc = SystemDescriptor::new(
                        c"legacy_bad_sched".as_ptr(),
                        core::ptr::null(),
                        0,
                        dummy_single_cb,
                        core::ptr::null_mut(),
                    );
                    // SAFETY: Valid `SystemDescriptor` with invalid `schedule_raw`.
                    let res_in_sched = unsafe {
                        host_register_system_in_schedule(ctx_ptr, &legacy_desc, bad_sched)
                    };
                    assert_eq!(res_in_sched, FfiResult::INVALID_ARGUMENT, "step {step}");
                }
                7 => {
                    // Malformed `after` / `before`: non-zero count with null outer pointer
                    let mut desc = valid_base;
                    if rng.gen_bool() {
                        desc.after = core::ptr::null();
                        desc.after_count = 1 + rng.range(16);
                    } else {
                        desc.before = core::ptr::null();
                        desc.before_count = 1 + rng.range(16);
                    }
                    // SAFETY: Null pointer check precedes slice read.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                }
                8 => {
                    // Malformed `after` / `before`: array containing a null `*const c_char` element
                    let ptrs: [*const c_char; 2] = [c"physics".as_ptr(), core::ptr::null()];
                    let mut desc = valid_base;
                    if rng.gen_bool() {
                        desc.after = ptrs.as_ptr();
                        desc.after_count = ptrs.len();
                    } else {
                        desc.before = ptrs.as_ptr();
                        desc.before_count = ptrs.len();
                    }
                    // SAFETY: `ptrs` slice has length 2; element 1 is null.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                }
                9 => {
                    // Malformed `after` / `before`: array containing non-UTF8 NUL-terminated C string
                    let bad_set = rng.invalid_utf8_cstring();
                    let ptrs: [*const c_char; 2] =
                        [c"valid_set".as_ptr(), bad_set.as_ptr().cast::<c_char>()];
                    let mut desc = valid_base;
                    if rng.gen_bool() {
                        desc.after = ptrs.as_ptr();
                        desc.after_count = ptrs.len();
                    } else {
                        desc.before = ptrs.as_ptr();
                        desc.before_count = ptrs.len();
                    }
                    // SAFETY: Both C strings are NUL-terminated.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::INVALID_UTF8, "step {step}");
                }
                10 => {
                    // Truncated `struct_size` generated randomly in `0..SystemDescriptor::MIN_V1_SIZE`
                    let sz = rng.range(SystemDescriptor::MIN_V1_SIZE) as u32;
                    let (_buf, raw_ptr) =
                        make_sized_descriptor_buffer(&valid_base, sz, rng.range(4));
                    // SAFETY: `_buf` holds at least `max(sz, 4)` bytes.
                    let res = unsafe {
                        host_register_scheduled_system(
                            ctx_ptr,
                            raw_ptr.cast::<ScheduledSystemDescriptor>(),
                        )
                    };
                    assert_eq!(res, FfiResult::INVALID_VERSION, "step {step}");
                }
                _ => {
                    // Valid max-column boundary (`query_count == MAX_QUERY_COMPONENTS`) with valid modes `0..=3`
                    let names: Vec<CString> = (0..MAX_QUERY_COMPONENTS)
                        .map(|i| CString::new(format!("Comp_{i}")).unwrap())
                        .collect();
                    let queries: Vec<ComponentAccessDesc> = names
                        .iter()
                        .map(|n| ComponentAccessDesc {
                            name: n.as_ptr(),
                            mode: ComponentAccessMode(rng.range(4) as u32),
                        })
                        .collect();
                    let mut desc = valid_base;
                    desc.base.queries = queries.as_ptr();
                    desc.base.query_count = queries.len();
                    desc.schedule = ScheduleLabelId(rng.range(6) as u32);
                    // SAFETY: Valid max-size query array and schedule ID.
                    let res = unsafe { host_register_scheduled_system(ctx_ptr, &desc) };
                    assert_eq!(res, FfiResult::OK, "step {step}");
                    drop(_guard);
                    assert_eq!(init_ctx.registered_systems.len(), 1);
                }
            }
        }
    }

    // ============================================================================
    // 2. Fuzz `host_register_multi_query_system`
    // ============================================================================

    #[test]
    fn fuzz_host_register_multi_query_system_adversarial_inputs() {
        let registry = Arc::new(ModRegistry::new());
        let mut init_ctx = ModInitContext::new(Arc::clone(&registry));

        let valid_q = ComponentAccessDesc::read(c"Position".as_ptr());
        let valid_spec = QuerySpec {
            queries: &valid_q,
            query_count: 1,
        };
        let valid_base = MultiQuerySystemDescriptor::new(
            c"multi_sys".as_ptr(),
            &valid_spec,
            1,
            dummy_multi_cb,
            core::ptr::null_mut(),
        )
        .with_schedule(ScheduleLabelId::UPDATE);

        // Exhaustive `struct_size` sweep 0..128 + u32::MAX
        for sz in (0u32..128).chain(core::iter::once(u32::MAX)) {
            let _guard = init_ctx.make_active();
            let ctx_ptr = core::ptr::addr_of_mut!(init_ctx).cast::<c_void>();
            let (_buf, raw_ptr) = make_sized_descriptor_buffer(&valid_base, sz, 0);
            let min_v1 = MultiQuerySystemDescriptor::MIN_V1_SIZE as u32;
            // SAFETY: `_buf` is readable for `read_descriptor`.
            let res = unsafe {
                host_register_multi_query_system(
                    ctx_ptr,
                    raw_ptr.cast::<MultiQuerySystemDescriptor>(),
                )
            };
            if sz < min_v1 {
                assert_eq!(res, FfiResult::INVALID_VERSION, "sz={sz}");
            } else {
                assert_eq!(res, FfiResult::OK, "sz={sz}");
            }
        }

        // Null & zero-callback checks
        // SAFETY: Testing explicit null guards and zeroed callback slot.
        {
            let _guard = init_ctx.make_active();
            let ctx_ptr = core::ptr::addr_of_mut!(init_ctx).cast::<c_void>();
            unsafe {
                assert_eq!(
                    host_register_multi_query_system(core::ptr::null_mut(), core::ptr::null()),
                    FfiResult::NULL_POINTER
                );
                assert_eq!(
                    host_register_multi_query_system(ctx_ptr, core::ptr::null()),
                    FfiResult::NULL_POINTER
                );

                let mut zero_cb =
                    vec![0u8; core::mem::size_of::<ScheduledMultiQuerySystemDescriptor>()];
                let full_sz = core::mem::size_of::<ScheduledMultiQuerySystemDescriptor>() as u32;
                zero_cb[..4].copy_from_slice(&full_sz.to_ne_bytes());
                assert_eq!(
                    host_register_multi_query_system(
                        ctx_ptr,
                        zero_cb.as_ptr().cast::<MultiQuerySystemDescriptor>()
                    ),
                    FfiResult::NULL_POINTER
                );
            }
        }

        let mut rng = Rng::new(0xBADC_0FFE_E002_2222);
        for step in 0..FUZZ_ITERS {
            init_ctx.registered_systems.clear();
            let _guard = init_ctx.make_active();
            let ctx_ptr = core::ptr::addr_of_mut!(init_ctx).cast::<c_void>();
            match rng.range(9) {
                0 => {
                    // `query_spec_count == 0` or `> MAX_SYSTEM_QUERIES` (`9..=usize::MAX`)
                    let bad_spec_count = match rng.range(4) {
                        0 => 0,
                        1 => MAX_SYSTEM_QUERIES + 1,
                        2 => MAX_SYSTEM_QUERIES + 1 + rng.range(50_000),
                        _ => usize::MAX,
                    };
                    let mut desc = valid_base;
                    desc.base.query_specs = core::ptr::null();
                    desc.base.query_spec_count = bad_spec_count;
                    // SAFETY: `query_spec_count` bounds check runs before dereferencing `query_specs`.
                    let res =
                        unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                    assert_eq!(res, FfiResult::OUT_OF_RANGE, "step {step}");
                }
                1 => {
                    // Valid `query_spec_count` (`1..=8`) with null `query_specs`
                    let mut desc = valid_base;
                    desc.base.query_specs = core::ptr::null();
                    desc.base.query_spec_count = 1 + rng.range(MAX_SYSTEM_QUERIES);
                    // SAFETY: Null `query_specs` check precedes slice read.
                    let res =
                        unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                    assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                }
                2 => {
                    // Inner spec has `query_count > MAX_QUERY_COMPONENTS` (`33..=usize::MAX`)
                    let spec_cnt = 1 + rng.range(MAX_SYSTEM_QUERIES);
                    let bad_spec_idx = rng.range(spec_cnt);
                    let specs: Vec<QuerySpec> = (0..spec_cnt)
                        .map(|i| {
                            if i == bad_spec_idx {
                                QuerySpec {
                                    queries: core::ptr::null(),
                                    query_count: if rng.gen_bool() {
                                        usize::MAX
                                    } else {
                                        MAX_QUERY_COMPONENTS + 1 + rng.range(500)
                                    },
                                }
                            } else {
                                valid_spec
                            }
                        })
                        .collect();
                    let mut desc = valid_base;
                    desc.base.query_specs = specs.as_ptr();
                    desc.base.query_spec_count = specs.len();
                    // SAFETY: `spec.query_count > MAX_QUERY_COMPONENTS` checked before `from_raw_parts`.
                    let res =
                        unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                    assert_eq!(res, FfiResult::OUT_OF_RANGE, "step {step}");
                }
                3 => {
                    // Inner spec has `query_count > 0` with null `queries`
                    let bad_spec = QuerySpec {
                        queries: core::ptr::null(),
                        query_count: 1 + rng.range(MAX_QUERY_COMPONENTS),
                    };
                    let specs = [valid_spec, bad_spec];
                    let mut desc = valid_base;
                    desc.base.query_specs = specs.as_ptr();
                    desc.base.query_spec_count = specs.len();
                    // SAFETY: Null check precedes inner slice read.
                    let res =
                        unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                    assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                }
                4 => {
                    // Invalid `ComponentAccessMode(10..=u32::MAX)` in one of the specs
                    let bad_mode = (10 + (rng.next_u32() % (u32::MAX - 10))).max(10);
                    let bad_q = ComponentAccessDesc {
                        name: c"Velocity".as_ptr(),
                        mode: ComponentAccessMode(bad_mode),
                    };
                    let bad_spec = QuerySpec {
                        queries: &bad_q,
                        query_count: 1,
                    };
                    let specs = [valid_spec, bad_spec];
                    let mut desc = valid_base;
                    desc.base.query_specs = specs.as_ptr();
                    desc.base.query_spec_count = specs.len();
                    // SAFETY: Valid slices and C strings.
                    let res =
                        unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                    assert_eq!(res, FfiResult::INVALID_ARGUMENT, "step {step}");
                }
                5 => {
                    // Non-UTF8 component name in one of the specs
                    let bad_cstr = rng.invalid_utf8_cstring();
                    let bad_q = ComponentAccessDesc {
                        name: bad_cstr.as_ptr().cast::<c_char>(),
                        mode: ComponentAccessMode::WRITE,
                    };
                    let bad_spec = QuerySpec {
                        queries: &bad_q,
                        query_count: 1,
                    };
                    let mut desc = valid_base;
                    desc.base.query_specs = &bad_spec;
                    desc.base.query_spec_count = 1;
                    // SAFETY: NUL-terminated byte array.
                    let res =
                        unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                    assert_eq!(res, FfiResult::INVALID_UTF8, "step {step}");
                }
                6 => {
                    // Invalid `ScheduleLabelId` (`6..=u32::MAX`)
                    let mut desc = valid_base;
                    desc.schedule = ScheduleLabelId(6 + (rng.next_u32() % 50_000));
                    // SAFETY: Valid descriptor with invalid schedule ID.
                    let res =
                        unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                    assert_eq!(res, FfiResult::INVALID_VERSION, "step {step}");
                }
                7 => {
                    // Malformed `before` array with invalid UTF-8 set name
                    let bad_set = rng.invalid_utf8_cstring();
                    let sets = [bad_set.as_ptr().cast::<c_char>()];
                    let mut desc = valid_base;
                    desc.before = sets.as_ptr();
                    desc.before_count = sets.len();
                    // SAFETY: Valid pointer array to NUL-terminated non-UTF8 bytes.
                    let res =
                        unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                    assert_eq!(res, FfiResult::INVALID_UTF8, "step {step}");
                }
                _ => {
                    // Valid `MAX_SYSTEM_QUERIES` (`8`) specs
                    let specs = [valid_spec; MAX_SYSTEM_QUERIES];
                    let mut desc = valid_base;
                    desc.base.query_specs = specs.as_ptr();
                    desc.base.query_spec_count = specs.len();
                    desc.schedule = ScheduleLabelId(rng.range(6) as u32);
                    // SAFETY: Valid 8-spec multi-query descriptor.
                    let res =
                        unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                    assert_eq!(res, FfiResult::OK, "step {step}");
                    drop(_guard);
                    assert_eq!(init_ctx.registered_systems.len(), 1);
                }
            }
        }
    }

    // ============================================================================
    // 3. Fuzz `host_register_component`
    // ============================================================================

    #[test]
    fn fuzz_host_register_component_layouts_and_contexts() {
        let mut world = World::new();
        let registry = Arc::new(ModRegistry::new());
        world.insert_resource((*registry).clone());
        let is_active = Arc::new(AtomicBool::new(true));

        // Seed one valid dynamic component so we can also fuzz conflicting re-registrations!
        {
            let mut init_ctx = ModInitContext::with_world(
                Arc::clone(&registry),
                Arc::clone(&is_active),
                &mut world as *mut World,
            );
            let _guard = init_ctx.make_active();
            let seed_desc = ModComponentDescriptor::new(
                c"SeededDynComp".as_ptr(),
                ComponentUuid::from_name("SeededDynComp"),
                16,
                8,
            );
            // SAFETY: Valid active `ModInitContext` and `ModComponentDescriptor`.
            assert_eq!(
                unsafe {
                    host_register_component(
                        (&mut init_ctx as *mut ModInitContext).cast::<c_void>(),
                        &seed_desc,
                    )
                },
                FfiResult::OK
            );
        }

        // 1. Exhaustive `struct_size` sweep `0..128` + `u32::MAX`
        let template_desc = ModComponentDescriptor::new(
            c"SizedProbeComp".as_ptr(),
            ComponentUuid::from_name("SizedProbeComp"),
            8,
            8,
        );
        {
            let mut init_ctx = ModInitContext::with_world(
                Arc::new(world.resource::<ModRegistry>().clone()),
                Arc::clone(&is_active),
                &mut world as *mut World,
            );
            let _guard = init_ctx.make_active();
            let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

            for sz in (0u32..128).chain(core::iter::once(u32::MAX)) {
                let (_buf, raw_ptr) = make_sized_descriptor_buffer(&template_desc, sz, 0);
                let min_v1 = ModComponentDescriptor::MIN_V1_SIZE as u32;
                // SAFETY: `_buf` holds at least `max(sz, 4)` readable bytes.
                let res = unsafe {
                    host_register_component(ctx_ptr, raw_ptr.cast::<ModComponentDescriptor>())
                };
                if sz < min_v1 {
                    assert_eq!(res, FfiResult::INVALID_VERSION, "sz={sz}");
                } else {
                    assert_eq!(res, FfiResult::OK, "sz={sz}");
                }
            }
        }

        // 2. Explicit null, mismatched context, and null-world checks
        // SAFETY: Testing explicit pointer and context validation branches.
        unsafe {
            assert_eq!(
                host_register_component(core::ptr::null_mut(), core::ptr::null()),
                FfiResult::NULL_POINTER
            );
            let mut dummy_ctx = ModInitContext::new(Arc::clone(&registry));
            let dummy_ptr = (&mut dummy_ctx as *mut ModInitContext).cast::<c_void>();
            assert_eq!(
                host_register_component(dummy_ptr, core::ptr::null()),
                FfiResult::NULL_POINTER
            );

            // Active context has null `world` -> INVALID_STATE
            let _guard = dummy_ctx.make_active();
            let dummy_ptr = (&mut dummy_ctx as *mut ModInitContext).cast::<c_void>();
            assert_eq!(
                host_register_component(dummy_ptr, &template_desc),
                FfiResult::INVALID_STATE
            );
            // Passing a forged/different non-null `ctx` while `dummy_ctx` is active -> INVALID_STATE
            let mut other_ctx = ModInitContext::new(Arc::clone(&registry));
            assert_eq!(
                host_register_component(
                    (&mut other_ctx as *mut ModInitContext).cast::<c_void>(),
                    &template_desc
                ),
                FfiResult::INVALID_STATE
            );
        }

        // 3. Randomized layout & conflict fuzzing
        let mut rng = Rng::new(0xFACE_FEED_0003_3333);
        let mut init_ctx = ModInitContext::with_world(
            Arc::new(world.resource::<ModRegistry>().clone()),
            Arc::clone(&is_active),
            &mut world as *mut World,
        );
        let _guard = init_ctx.make_active();
        let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

        let non_pow2_aligns: [usize; 12] = [0, 3, 5, 6, 7, 9, 10, 12, 15, 24, 100, usize::MAX];

        for step in 0..FUZZ_ITERS {
            let name_cstr = CString::new(format!("FuzzDyn_{step}")).unwrap();
            let uuid = ComponentUuid {
                bytes: {
                    let mut b = [0u8; 16];
                    b[..8].copy_from_slice(&rng.next_u64().to_le_bytes());
                    b[8..].copy_from_slice(&rng.next_u64().to_le_bytes());
                    b
                },
            };

            match rng.range(8) {
                0 => {
                    // Null `name` pointer -> NULL_POINTER
                    let desc = ModComponentDescriptor::new(core::ptr::null(), uuid, 8, 8);
                    // SAFETY: Valid `ctx_ptr` and `desc`.
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc) },
                        FfiResult::NULL_POINTER
                    );
                }
                1 => {
                    // Non-UTF8 `name` -> INVALID_UTF8
                    let bad_name = rng.invalid_utf8_cstring();
                    let desc =
                        ModComponentDescriptor::new(bad_name.as_ptr().cast::<c_char>(), uuid, 8, 8);
                    // SAFETY: NUL-terminated invalid UTF-8 bytes.
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc) },
                        FfiResult::INVALID_UTF8
                    );
                }
                2 => {
                    // Non-power-of-two or zero alignment -> INVALID_ARGUMENT
                    let align = non_pow2_aligns[rng.range(non_pow2_aligns.len())];
                    let size = rng.range(128);
                    let desc = ModComponentDescriptor::new(name_cstr.as_ptr(), uuid, size, align);
                    // SAFETY: Valid descriptor with invalid alignment.
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc) },
                        FfiResult::INVALID_ARGUMENT,
                        "step {step}: align={align} size={size}"
                    );
                }
                3 => {
                    // Valid power-of-two `align >= 2`, but `size % align != 0` -> INVALID_ARGUMENT
                    let shift = 1 + rng.range(10); // align in 2..=1024
                    let align = 1usize << shift;
                    let rem = 1 + rng.range(align - 1);
                    let size = rng.range(16) * align + rem;
                    let desc = ModComponentDescriptor::new(name_cstr.as_ptr(), uuid, size, align);
                    // SAFETY: Valid descriptor with unpadded size (`size % align != 0`).
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc) },
                        FfiResult::INVALID_ARGUMENT,
                        "step {step}: size={size} align={align}"
                    );
                }
                4 => {
                    // `usize::MAX` or `> isize::MAX` overflow sizes -> INVALID_ARGUMENT
                    let shift = rng.range(8);
                    let align = 1usize << shift;
                    let overflow_size = match rng.range(3) {
                        0 => usize::MAX & !(align - 1),
                        1 => (isize::MAX as usize) + 1,
                        _ => usize::MAX,
                    };
                    let desc =
                        ModComponentDescriptor::new(name_cstr.as_ptr(), uuid, overflow_size, align);
                    // SAFETY: Overflow layout rejected cleanly before allocation.
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc) },
                        FfiResult::INVALID_ARGUMENT,
                        "step {step}: overflow_size={overflow_size} align={align}"
                    );
                }
                5 => {
                    // Conflicting re-registration of `SeededDynComp` (size=16, align=8) -> INVALID_COMPONENT
                    let (bad_size, bad_align, bad_uuid) = match rng.range(3) {
                        0 => (32, 8, ComponentUuid::from_name("SeededDynComp")),
                        1 => (16, 16, ComponentUuid::from_name("SeededDynComp")),
                        _ => (16, 8, ComponentUuid::from_name("DifferentUuidForSameName")),
                    };
                    let conflict_desc = ModComponentDescriptor::new(
                        c"SeededDynComp".as_ptr(),
                        bad_uuid,
                        bad_size,
                        bad_align,
                    );
                    // SAFETY: Valid descriptor testing schema conflict rejection.
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &conflict_desc) },
                        FfiResult::INVALID_COMPONENT,
                        "step {step}"
                    );
                }
                6 => {
                    // Idempotent re-registration of `SeededDynComp` with exact matching layout & UUID -> OK
                    let same_desc = ModComponentDescriptor::new(
                        c"SeededDynComp".as_ptr(),
                        ComponentUuid::from_name("SeededDynComp"),
                        16,
                        8,
                    );
                    // SAFETY: Identical layout re-registration.
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &same_desc) },
                        FfiResult::OK
                    );
                }
                _ => {
                    // Valid ZST or over-aligned POD component (`size = k * align`, `align = 1 << (0..=8)`)
                    let align = 1usize << rng.range(9);
                    let mult = rng.range(5); // includes 0 -> over-aligned ZST!
                    let size = mult * align;
                    let desc = ModComponentDescriptor::new(name_cstr.as_ptr(), uuid, size, align);
                    // SAFETY: Valid layout.
                    assert_eq!(
                        unsafe { host_register_component(ctx_ptr, &desc) },
                        FfiResult::OK,
                        "step {step}: size={size} align={align}"
                    );
                }
            }
        }
    }

    // ============================================================================
    // 4. Fuzz `host_iter_*` (Dense Table & SparseSet Archetypal State Machines)
    // ============================================================================

    #[test]
    fn fuzz_host_iter_state_machine_and_bounds() {
        // 1. Null-pointer safety across every `host_iter_*` entry point
        // SAFETY: Testing explicit null guards on all 7 iterator FFI functions.
        unsafe {
            assert_eq!(host_iter_next_chunk(core::ptr::null_mut()), 0);
            assert_eq!(
                host_iter_reset(core::ptr::null_mut()),
                FfiResult::NULL_POINTER
            );
            assert!(host_iter_column_readonly(core::ptr::null(), 0).is_null());
            assert!(host_iter_column_readonly(core::ptr::null(), usize::MAX).is_null());
            assert!(host_iter_column_mutable(core::ptr::null_mut(), 0).is_null());
            assert!(host_iter_column_mutable(core::ptr::null_mut(), usize::MAX).is_null());
            assert_eq!(
                host_iter_mark_changed(core::ptr::null_mut(), 0, 0),
                FfiResult::NULL_POINTER
            );
            assert!(host_iter_entities(core::ptr::null()).is_null());
            assert_eq!(host_iter_column_stride(core::ptr::null(), 0), 0);
            assert_eq!(host_iter_column_stride(core::ptr::null(), usize::MAX), 0);
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

        // Build dense query state: Col 0 = READ Position, Col 1 = WRITE Velocity, Col 2 = WITHOUT FuzzZst
        let mut dense_qb = QueryBuilder::<bevy_ecs::world::FilteredEntityMut>::new(&mut world);
        dense_qb.ref_id(pos_id);
        dense_qb.mut_id(vel_id);
        dense_qb.without_id(zst_id);
        let mut dense_qs = dense_qb.build();
        dense_qs.update_archetypes(&world);
        let dense_tables: Vec<_> = dense_qs.matched_tables().collect();
        let dense_cols = [
            QueryColumnDesc {
                component_id: pos_id,
                mode: ComponentAccessMode::READ,
                stride: core::mem::size_of::<Position>(),
            },
            QueryColumnDesc {
                component_id: vel_id,
                mode: ComponentAccessMode::WRITE,
                stride: core::mem::size_of::<Velocity>(),
            },
            QueryColumnDesc {
                component_id: zst_id,
                mode: ComponentAccessMode::WITHOUT,
                stride: 0,
            },
        ];

        // Build sparse archetypal query state: Col 0 = READ Position, Col 1 = WRITE FuzzSparseVal, Col 2 = FILTER_ONLY Velocity
        let mut sparse_qb = QueryBuilder::<bevy_ecs::world::FilteredEntityMut>::new(&mut world);
        sparse_qb.ref_id(pos_id);
        sparse_qb.mut_id(sparse_id);
        sparse_qb.with_id(vel_id);
        let mut sparse_qs = sparse_qb.build();
        sparse_qs.update_archetypes(&world);
        let sparse_archs: Vec<_> = sparse_qs.matched_archetypes().collect();
        let sparse_cols = [
            QueryColumnDesc {
                component_id: pos_id,
                mode: ComponentAccessMode::READ,
                stride: core::mem::size_of::<Position>(),
            },
            QueryColumnDesc {
                component_id: sparse_id,
                mode: ComponentAccessMode::WRITE,
                stride: core::mem::size_of::<FuzzSparseVal>(),
            },
            QueryColumnDesc {
                component_id: vel_id,
                mode: ComponentAccessMode::FILTER_ONLY,
                stride: core::mem::size_of::<Velocity>(),
            },
        ];

        let this_run = world.increment_change_tick();
        let cell = world.as_unsafe_world_cell();
        // SAFETY: `storages` and `archetypes` metadata borrows live for the scope of the iterators below;
        // we do not mutate structural world state during chunk iterator fuzzing.
        let storages = unsafe { cell.storages() };
        let archetypes = cell.archetypes();

        let mut rng = Rng::new(0x17E4_F022_0004_4444);

        for use_sparse in [false, true] {
            let cols_ref: &[QueryColumnDesc] = if use_sparse {
                &sparse_cols
            } else {
                &dense_cols
            };
            let mut host_iter = if use_sparse {
                HostChunkIter::new_archetypal(
                    &storages.tables,
                    archetypes,
                    &storages.sparse_sets,
                    &sparse_archs,
                    cols_ref,
                    1 << 1, // col 1 (`FuzzSparseVal`) is SparseSet
                    this_run,
                )
            } else {
                HostChunkIter::new(&storages.tables, &dense_tables, cols_ref, this_run)
            };

            let raw = (&mut host_iter as *mut HostChunkIter<'_>).cast::<RawChunkIter>();
            let mut active_len: usize = 0;

            for step in 0..FUZZ_ITERS {
                let col_idx = match rng.range(6) {
                    0 => 0,
                    1 => 1,
                    2 => 2,
                    3 => 3 + rng.range(MAX_QUERY_COMPONENTS),
                    4 => MAX_QUERY_COMPONENTS + rng.range(1_000),
                    _ => usize::MAX,
                };

                match rng.range(7) {
                    0 => {
                        // Advance chunk
                        // SAFETY: `raw` points to valid `host_iter`.
                        active_len = unsafe { host_iter_next_chunk(raw) };
                    }
                    1 => {
                        // Reset iterator -> active chunk must immediately become empty (`active_len == 0`)
                        // SAFETY: `raw` points to valid `host_iter`.
                        let r = unsafe { host_iter_reset(raw) };
                        assert_eq!(r, FfiResult::OK, "step {step}");
                        active_len = 0;
                    }
                    2 => {
                        // Readonly column request
                        // SAFETY: `raw` points to valid `host_iter`.
                        let ptr = unsafe { host_iter_column_readonly(raw, col_idx) };
                        if active_len == 0 || col_idx >= 2 {
                            // Col 2 is WITHOUT or FILTER_ONLY; col >= 3 is out-of-bounds
                            assert!(
                                ptr.is_null(),
                                "step {step} (sparse={use_sparse}): expected null readonly for active_len={active_len}, col={col_idx}"
                            );
                        } else {
                            assert!(!ptr.is_null(), "step {step}: expected valid readonly ptr");
                        }
                    }
                    3 => {
                        // Mutable column request
                        // SAFETY: `raw` points to valid `host_iter`.
                        let ptr = unsafe { host_iter_column_mutable(raw, col_idx) };
                        if active_len > 0 && col_idx == 1 {
                            // Only Col 1 is WRITE!
                            assert!(!ptr.is_null(), "step {step}: expected valid mutable ptr");
                        } else {
                            assert!(
                                ptr.is_null(),
                                "step {step} (sparse={use_sparse}): expected null mutable for active_len={active_len}, col={col_idx}"
                            );
                        }
                    }
                    4 => {
                        // `host_iter_mark_changed` with valid and out-of-bounds `(col_idx, row)`
                        let row = match rng.range(4) {
                            0 => 0,
                            1 => active_len.saturating_sub(1),
                            2 => active_len + rng.range(64),
                            _ => usize::MAX,
                        };
                        // SAFETY: `raw` points to valid `host_iter`.
                        let res = unsafe { host_iter_mark_changed(raw, col_idx, row) };
                        if col_idx >= cols_ref.len() || col_idx >= MAX_QUERY_COMPONENTS {
                            assert_eq!(res, FfiResult::INVALID_COMPONENT, "step {step}");
                        } else if cols_ref[col_idx].mode != ComponentAccessMode::WRITE {
                            assert_eq!(res, FfiResult::PERMISSION_DENIED, "step {step}");
                        } else if row >= active_len {
                            assert_eq!(res, FfiResult::OUT_OF_RANGE, "step {step}");
                        } else {
                            assert_eq!(res, FfiResult::OK, "step {step}");
                        }
                    }
                    5 => {
                        // `host_iter_entities`
                        // SAFETY: `raw` points to valid `host_iter`.
                        let ent_ptr = unsafe { host_iter_entities(raw) };
                        if active_len == 0 {
                            assert!(ent_ptr.is_null(), "step {step}");
                        } else {
                            assert!(!ent_ptr.is_null(), "step {step}");
                            for r in 0..active_len {
                                // SAFETY: `r < active_len` within current chunk entity slice.
                                let ffi_e = unsafe { *ent_ptr.add(r) };
                                assert!(!ffi_e.is_null());
                            }
                        }
                    }
                    _ => {
                        // `host_iter_column_stride`
                        // SAFETY: `raw` points to valid `host_iter`.
                        let stride = unsafe { host_iter_column_stride(raw, col_idx) };
                        if col_idx < cols_ref.len() {
                            assert_eq!(stride, cols_ref[col_idx].stride, "step {step}");
                        } else {
                            assert_eq!(stride, 0, "step {step}");
                        }
                    }
                }
            }
        }
    }

    // ============================================================================
    // 5. Fuzz `host_cmd_*` and `host_get_proc_address` (including `CommandQueue::apply`)
    // ============================================================================

    #[test]
    fn fuzz_host_cmd_and_proc_address_adversarial_inputs() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Velocity>(); // FfiLayout, size = 12, raw-insertable
        app.register_mod_component::<FuzzZst>(); // ZST, size = 0, raw-insertable
        app.register_mod_component::<NameTag>(); // String-owning, NOT raw-insertable (`PERMISSION_DENIED`)
        app.register_mod_component::<FuzzBoolComp>(); // bool-containing, NOT raw-insertable (`PERMISSION_DENIED`)
        app.register_mod_component_opaque::<Position>(); // Explicitly opaque, NOT raw-insertable (`PERMISSION_DENIED`)

        // Also register one runtime dynamic component (`DynPod8`, size = 8)
        {
            let world = app.world_mut();
            let reg = Arc::new(world.resource::<ModRegistry>().clone());
            let active = Arc::new(AtomicBool::new(true));
            let mut ctx = ModInitContext::with_world(reg, active, world as *mut World);
            let _g = ctx.make_active();
            let desc = ModComponentDescriptor::new(
                c"DynPod8".as_ptr(),
                ComponentUuid::from_name("DynPod8"),
                8,
                8,
            );
            // SAFETY: Valid `ModInitContext` and descriptor.
            assert_eq!(
                unsafe {
                    host_register_component(
                        (&mut ctx as *mut ModInitContext).cast::<c_void>(),
                        &desc,
                    )
                },
                FfiResult::OK
            );
        }

        // Explicit null checks for `host_cmd_*` and `host_get_proc_address`
        // SAFETY: Testing explicit null guards.
        unsafe {
            assert_eq!(host_cmd_spawn_empty(core::ptr::null_mut()), FfiEntity::NULL);
            assert_eq!(
                host_cmd_despawn(core::ptr::null_mut(), FfiEntity::from_bits(42)),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_insert_component(
                    core::ptr::null_mut(),
                    FfiEntity::from_bits(42),
                    c"Velocity".as_ptr(),
                    [0u8; 12].as_ptr(),
                    12
                ),
                FfiResult::NULL_POINTER
            );
            assert!(host_get_proc_address(core::ptr::null()).is_null());
        }

        let mut rng = Rng::new(0xC0DE_CAFE_0005_5555);
        let mut live_entities: Vec<FfiEntity> = Vec::new();
        let scratch_payload = [0xA5u8; 256];

        // Bind thread-local registry so `host_get_proc_address` also exercises `CURRENT_REGISTRY`
        let reg_arc = Arc::new(app.world().resource::<ModRegistry>().clone());
        let _reg_guard = reg_arc.make_current();

        let mut queue = CommandQueue::default();

        for step in 0..FUZZ_ITERS {
            {
                let world = app.world();
                let reg = world.resource::<ModRegistry>();
                let mut bridge = CommandQueueBridge::new(&mut queue, world.entity_allocator(), reg);
                let raw_q = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

                match rng.range(11) {
                    0 => {
                        // Valid `host_cmd_spawn_empty`
                        // SAFETY: Valid `raw_q`.
                        let e = unsafe { host_cmd_spawn_empty(raw_q) };
                        assert!(!e.is_null(), "step {step}");
                        assert_ne!(e.to_bits(), Entity::PLACEHOLDER.to_bits());
                        live_entities.push(e);
                    }
                    1 => {
                        // `host_cmd_despawn` with forged / null / placeholder / unallocated / stale entity bits
                        let bad_entity = match rng.range(5) {
                            0 => FfiEntity::NULL,
                            1 => FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()),
                            2 => FfiEntity::from_bits(u64::MAX),
                            3 => FfiEntity::from_bits(rng.next_u64()),
                            _ => {
                                // Generation-0 invalid bits (lower 32-bit zero in Bevy NonZero encoding)
                                FfiEntity::from_bits((rng.next_u32() as u64) << 32)
                            }
                        };
                        // SAFETY: Valid `raw_q`; entity validity checked by host.
                        let res = unsafe { host_cmd_despawn(raw_q, bad_entity) };
                        if bad_entity.is_null()
                            || Entity::try_from_bits(bad_entity.to_bits()).is_none()
                            || Entity::try_from_bits(bad_entity.to_bits())
                                == Some(Entity::PLACEHOLDER)
                        {
                            assert_eq!(res, FfiResult::INVALID_ENTITY, "step {step}");
                        } else {
                            // Valid `Entity` bit encoding (even if unspawned/stale) queues silent Bevy despawn safely!
                            assert_eq!(res, FfiResult::OK, "step {step}");
                        }
                    }
                    2 => {
                        // Despawn a real spawned entity (including occasional double-despawn!)
                        if !live_entities.is_empty() {
                            let idx = rng.range(live_entities.len());
                            let e = if rng.gen_bool() {
                                live_entities[idx] // keep in list to provoke later double-despawn / stale insert!
                            } else {
                                live_entities.swap_remove(idx)
                            };
                            // SAFETY: Valid `raw_q`.
                            assert_eq!(unsafe { host_cmd_despawn(raw_q, e) }, FfiResult::OK);
                        }
                    }
                    3 => {
                        // `host_cmd_insert_component`: null `name` or (`len > 0 && data.is_null()`)
                        let target = live_entities
                            .first()
                            .copied()
                            .unwrap_or_else(|| FfiEntity::from_bits(42));
                        if rng.gen_bool() {
                            // SAFETY: Null `name` check runs first.
                            let res = unsafe {
                                host_cmd_insert_component(
                                    raw_q,
                                    target,
                                    core::ptr::null(),
                                    scratch_payload.as_ptr(),
                                    12,
                                )
                            };
                            assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                        } else {
                            let bad_len = match rng.range(3) {
                                0 => 12,
                                1 => 1 + rng.range(1024),
                                _ => usize::MAX,
                            };
                            // SAFETY: `len > 0 && data.is_null()` rejected before `slice::from_raw_parts`.
                            let res = unsafe {
                                host_cmd_insert_component(
                                    raw_q,
                                    target,
                                    c"Velocity".as_ptr(),
                                    core::ptr::null(),
                                    bad_len,
                                )
                            };
                            assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
                        }
                    }
                    4 => {
                        // Non-UTF8 component name -> INVALID_UTF8
                        let bad_name = rng.invalid_utf8_cstring();
                        // SAFETY: NUL-terminated non-UTF8 bytes.
                        let res = unsafe {
                            host_cmd_insert_component(
                                raw_q,
                                FfiEntity::NULL,
                                bad_name.as_ptr().cast::<c_char>(),
                                scratch_payload.as_ptr(),
                                8,
                            )
                        };
                        assert_eq!(res, FfiResult::INVALID_UTF8, "step {step}");
                    }
                    5 => {
                        // Unregistered component name -> INVALID_COMPONENT
                        let unknown = CString::new(format!("NoSuchComp_{step}")).unwrap();
                        // SAFETY: Valid C string and payload slice.
                        let res = unsafe {
                            host_cmd_insert_component(
                                raw_q,
                                FfiEntity::NULL,
                                unknown.as_ptr(),
                                scratch_payload.as_ptr(),
                                8,
                            )
                        };
                        assert_eq!(res, FfiResult::INVALID_COMPONENT, "step {step}");
                    }
                    6 => {
                        // Raw byte insertion onto `String` (`NameTag`), `bool` (`FuzzBoolComp`), or `opaque` (`Position`) -> PERMISSION_DENIED
                        let (comp_name, comp_size) = match rng.range(3) {
                            0 => (c"NameTag".as_ptr(), core::mem::size_of::<NameTag>()),
                            1 => (
                                c"FuzzBoolComp".as_ptr(),
                                core::mem::size_of::<FuzzBoolComp>(),
                            ),
                            _ => (c"Position".as_ptr(), core::mem::size_of::<Position>()),
                        };
                        let target = live_entities
                            .first()
                            .copied()
                            .unwrap_or_else(|| unsafe { host_cmd_spawn_empty(raw_q) });
                        // SAFETY: `scratch_payload` is 256 bytes (>= size of all three components).
                        let res = unsafe {
                            host_cmd_insert_component(
                                raw_q,
                                target,
                                comp_name,
                                scratch_payload.as_ptr(),
                                comp_size,
                            )
                        };
                        assert_eq!(res, FfiResult::PERMISSION_DENIED, "step {step}");
                    }
                    7 => {
                        // Wrong byte size on `Velocity` (size 12) or `DynPod8` (size 8) -> INTERNAL_ERROR
                        let (comp_name, expected_sz) = if rng.gen_bool() {
                            (c"Velocity".as_ptr(), core::mem::size_of::<Velocity>())
                        } else {
                            (c"DynPod8".as_ptr(), 8usize)
                        };
                        let mut wrong_len = rng.range(scratch_payload.len() + 1);
                        if wrong_len == expected_sz {
                            wrong_len += 1;
                        }
                        let target = live_entities
                            .first()
                            .copied()
                            .unwrap_or_else(|| unsafe { host_cmd_spawn_empty(raw_q) });
                        // SAFETY: `wrong_len <= scratch_payload.len()`.
                        let res = unsafe {
                            host_cmd_insert_component(
                                raw_q,
                                target,
                                comp_name,
                                scratch_payload.as_ptr(),
                                wrong_len,
                            )
                        };
                        assert_eq!(res, FfiResult::INVALID_ARGUMENT, "step {step}");
                    }
                    8 => {
                        // Invalid entity (`NULL` or `PLACEHOLDER`) with otherwise valid component & size -> INVALID_ENTITY
                        let bad_ent = if rng.gen_bool() {
                            FfiEntity::NULL
                        } else {
                            FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits())
                        };
                        // SAFETY: Valid payload slice; invalid entity rejected cleanly.
                        let res = unsafe {
                            host_cmd_insert_component(
                                raw_q,
                                bad_ent,
                                c"Velocity".as_ptr(),
                                scratch_payload.as_ptr(),
                                core::mem::size_of::<Velocity>(),
                            )
                        };
                        assert_eq!(res, FfiResult::INVALID_ENTITY, "step {step}");
                    }
                    9 => {
                        // Valid POD / ZST / Dynamic insertion (even onto potentially despawned/forged valid-bits entity!)
                        let target = if !live_entities.is_empty() && rng.gen_bool() {
                            live_entities[rng.range(live_entities.len())]
                        } else {
                            // SAFETY: Valid `raw_q`.
                            let e = unsafe { host_cmd_spawn_empty(raw_q) };
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
                                // SAFETY: `Velocity` implements `FfiLayout` of size 12.
                                let res = unsafe {
                                    host_cmd_insert_component(
                                        raw_q,
                                        target,
                                        c"Velocity".as_ptr(),
                                        (&vel as *const Velocity).cast::<u8>(),
                                        core::mem::size_of::<Velocity>(),
                                    )
                                };
                                assert_eq!(res, FfiResult::OK, "step {step}");
                            }
                            1 => {
                                // ZST insertion with null `data_ptr` and `len == 0`
                                // SAFETY: `len == 0` permits null `data_ptr`.
                                let res = unsafe {
                                    host_cmd_insert_component(
                                        raw_q,
                                        target,
                                        c"FuzzZst".as_ptr(),
                                        core::ptr::null(),
                                        0,
                                    )
                                };
                                assert_eq!(res, FfiResult::OK, "step {step}");
                            }
                            _ => {
                                let val = (step as u64).to_ne_bytes();
                                // SAFETY: `DynPod8` has size 8.
                                let res = unsafe {
                                    host_cmd_insert_component(
                                        raw_q,
                                        target,
                                        c"DynPod8".as_ptr(),
                                        val.as_ptr(),
                                        val.len(),
                                    )
                                };
                                assert_eq!(res, FfiResult::OK, "step {step}");
                            }
                        }
                    }
                    _ => {
                        // Fuzz `host_get_proc_address`: valid VTable symbols, unknown symbols, and invalid UTF-8
                        match rng.range(3) {
                            0 => {
                                // SAFETY: Valid C string literal registered by `register_mod_component::<Velocity>()`.
                                let sym =
                                    unsafe { host_get_proc_address(c"Velocity_vtable".as_ptr()) };
                                assert_eq!(sym, Velocity::vtable_ptr().cast::<c_void>());
                            }
                            1 => {
                                let miss = CString::new(format!("MissingSym_{step}")).unwrap();
                                // SAFETY: Valid NUL-terminated C string.
                                assert!(unsafe { host_get_proc_address(miss.as_ptr()) }.is_null());
                            }
                            _ => {
                                let bad_sym = rng.invalid_utf8_cstring();
                                // SAFETY: Valid NUL-terminated non-UTF8 bytes.
                                assert!(
                                    unsafe {
                                        host_get_proc_address(bad_sym.as_ptr().cast::<c_char>())
                                    }
                                    .is_null()
                                );
                            }
                        }
                    }
                }
            }

            // Periodically flush the `CommandQueue` onto `World` to prove zero deferred panics or corruption
            if step % 25 == 24 {
                queue.apply(app.world_mut());
            }
        }

        queue.apply(app.world_mut());
    }
}

mod host_api_boundary_and_proc_address {
    #![allow(unused_imports, dead_code)]
    use bevy_abi_mod_host::ModRegistry;
    use bevy_abi_mod_host_sys::{
        HOST_API, host_get_proc_address, host_iter_column_stride, host_register_component,
    };
    use bevy_abi_mod_sys::{
        ABI_VERSION_PACKED, ComponentUuid, FfiResult, HostApi, ModComponentDescriptor,
    };
    use core::ffi::{c_char, c_void};
    use std::ffi::CString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    // ============================================================================
    // 1. ABI Boundary & Monotonic Version Compatibility Tests
    // ============================================================================

    /// Memory layout mirroring the older 64-byte HostApi prior to register_component.
    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    struct LegacyHostApi64 {
        struct_size: u32,
        abi_version: u32,
        register_system: usize,
        iter_next_chunk: usize,
        iter_column_readonly: usize,
        iter_column_mutable: usize,
        iter_mark_changed: usize,
        iter_entities: usize,
        iter_column_stride: usize,
    }

    #[repr(C)]
    struct LegacyHostMemoryBuffer {
        legacy_api: LegacyHostApi64,
        canary_bytes: [u8; 8],
    }

    #[test]
    fn test_legacy_host_api_layout_assumptions() {
        assert_eq!(core::mem::size_of::<LegacyHostApi64>(), 64);
        assert_eq!(core::mem::align_of::<LegacyHostApi64>(), 8);
        assert_eq!(core::mem::size_of::<HostApi>(), 72);
        assert_eq!(core::mem::align_of::<HostApi>(), 8);
        assert_eq!(core::mem::offset_of!(HostApi, register_component), 64);
    }

    #[test]
    fn test_legacy_mod_on_newer_72_byte_host() {
        // Host provides modern 72-byte HOST_API
        assert_eq!(HOST_API.struct_size, 72);
        assert_eq!(HOST_API.abi_version, ABI_VERSION_PACKED);

        // Older mod compiled against LegacyHostApi64 receives HOST_API pointer
        let host_ptr = &HOST_API as *const HostApi as *const LegacyHostApi64;
        let legacy_view = unsafe { &*host_ptr };

        // Legacy mod performs monotonic negotiation: host_size >= legacy_size (72 >= 64)
        assert!(legacy_view.struct_size >= core::mem::size_of::<LegacyHostApi64>() as u32);
        assert_eq!(legacy_view.struct_size, 72);
        assert_eq!(legacy_view.abi_version, ABI_VERSION_PACKED);

        // Verify all legacy function pointers point to the exact expected host functions
        assert_eq!(
            legacy_view.iter_column_stride,
            host_iter_column_stride as *const () as usize
        );
    }

    #[test]
    fn test_newer_mod_on_older_64_byte_host_boundary_guard() {
        // Older host allocates exactly 64 bytes for LegacyHostApi64.
        // We place a canary immediately following it in memory to simulate unmapped/foreign data.
        let buffer = LegacyHostMemoryBuffer {
            legacy_api: LegacyHostApi64 {
                struct_size: 64,
                abi_version: ABI_VERSION_PACKED,
                register_system: 0x1111,
                iter_next_chunk: 0x2222,
                iter_column_readonly: 0x3333,
                iter_column_mutable: 0x4444,
                iter_mark_changed: 0x5555,
                iter_entities: 0x6666,
                iter_column_stride: host_iter_column_stride as *const () as usize,
            },
            canary_bytes: [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE],
        };

        let host_api_ptr = &buffer.legacy_api as *const LegacyHostApi64 as *const HostApi;
        let modern_view = unsafe { &*host_api_ptr };

        // Modern mod must inspect struct_size before touching register_component
        let host_struct_size = modern_view.struct_size;
        assert_eq!(host_struct_size, 64);

        let has_register_component = host_struct_size as usize >= core::mem::size_of::<HostApi>();
        assert!(
            !has_register_component,
            "Modern mod must detect that older host lacks register_component"
        );

        // Older functionality remains fully operational
        assert_eq!(
            modern_view.iter_column_stride as *const () as usize,
            host_iter_column_stride as *const () as usize
        );

        // Canary bytes at offset 64 remain untouched
        let canary_read = unsafe {
            core::ptr::read_unaligned((host_api_ptr as *const u8).add(64) as *const [u8; 8])
        };
        assert_eq!(
            canary_read,
            [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]
        );
    }

    // ============================================================================
    // 2. Dynamic Symbol Resolution Stress Tests
    // ============================================================================

    #[test]
    fn test_host_get_proc_address_stress_and_adversarial_queries() {
        let registry = Arc::new(ModRegistry::new());
        let _guard = registry.make_current();

        // 1. Valid registered symbols
        let sym_reg = unsafe { host_get_proc_address(c"register_component".as_ptr()) };
        assert_eq!(sym_reg, host_register_component as *const c_void);

        let sym_host_reg = unsafe { host_get_proc_address(c"host_register_component".as_ptr()) };
        assert_eq!(sym_host_reg, host_register_component as *const c_void);

        // 2. Null pointer input
        let null_res = unsafe { host_get_proc_address(core::ptr::null()) };
        assert_eq!(null_res, core::ptr::null());

        // 3. Empty string
        let empty_res = unsafe { host_get_proc_address(c"".as_ptr()) };
        assert_eq!(empty_res, core::ptr::null());

        // 4. Non-existent symbol
        let missing_res = unsafe { host_get_proc_address(c"some_non_existent_symbol".as_ptr()) };
        assert_eq!(missing_res, core::ptr::null());

        // 5. Near matches / Off-by-one prefix queries
        let near_misses = [
            c"register_componen".as_ptr(),
            c"register_components".as_ptr(),
            c"register_component_".as_ptr(),
            c"host_register_comp".as_ptr(),
            c"host_register_components".as_ptr(),
            c"Register_Component".as_ptr(), // case sensitivity check
            c"REGISTER_COMPONENT".as_ptr(),
        ];
        for &query in &near_misses {
            let res = unsafe { host_get_proc_address(query) };
            assert_eq!(
                res,
                core::ptr::null(),
                "Query {:?} must resolve to NULL",
                unsafe { core::ffi::CStr::from_ptr(query) }
            );
        }

        // 6. Whitespace and formatting variations
        let whitespace_queries = [
            c" register_component".as_ptr(),
            c"register_component ".as_ptr(),
            c"register_component\n".as_ptr(),
            c"register_component\t".as_ptr(),
        ];
        for &query in &whitespace_queries {
            let res = unsafe { host_get_proc_address(query) };
            assert_eq!(res, core::ptr::null());
        }

        // 7. Invalid UTF-8 sequence
        let bad_utf8_bytes = [0xff, 0xfe, 0xfd, 0x00];
        let bad_utf8_ptr = bad_utf8_bytes.as_ptr() as *const c_char;
        let res = unsafe { host_get_proc_address(bad_utf8_ptr) };
        assert_eq!(res, core::ptr::null());

        // 8. Extremely long query string (10,000 bytes)
        let long_string = "A".repeat(10_000);
        let c_long = CString::new(long_string).unwrap();
        let res = unsafe { host_get_proc_address(c_long.as_ptr()) };
        assert_eq!(res, core::ptr::null());
    }

    #[test]
    fn test_host_get_proc_address_high_concurrency() {
        let registry = Arc::new(ModRegistry::new());
        let _guard = registry.make_current();

        const NUM_THREADS: usize = 8;
        const QUERIES_PER_THREAD: usize = 5_000;

        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                thread::spawn(|| {
                    for i in 0..QUERIES_PER_THREAD {
                        let sym1 = unsafe { host_get_proc_address(c"register_component".as_ptr()) };
                        assert_eq!(sym1, host_register_component as *const c_void);

                        let sym2 =
                            unsafe { host_get_proc_address(c"host_register_component".as_ptr()) };
                        assert_eq!(sym2, host_register_component as *const c_void);

                        if i % 10 == 0 {
                            let bad = unsafe {
                                host_get_proc_address(c"non_existent_concurrent_query".as_ptr())
                            };
                            assert_eq!(bad, core::ptr::null());
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    // ============================================================================
    // 3. Dynamic Component Descriptor Stress Matrix
    // ============================================================================

    static DROP_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn custom_test_drop_hook(ptr: *mut c_void) {
        if !ptr.is_null() {
            DROP_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    unsafe fn call_register_component_fresh(desc: &ModComponentDescriptor) -> FfiResult {
        let mut world = bevy_ecs::world::World::new();
        let reg = Arc::new(ModRegistry::new());
        world.insert_resource((*reg).clone());
        let active = Arc::new(core::sync::atomic::AtomicBool::new(true));
        let mut init_ctx =
            bevy_abi_mod_host_sys::ModInitContext::with_world(reg, active, &mut world as *mut _);
        let _guard = init_ctx.make_active();
        let ctx_ptr =
            (&mut init_ctx as *mut bevy_abi_mod_host_sys::ModInitContext).cast::<c_void>();
        unsafe { host_register_component(ctx_ptr, desc) }
    }

    #[test]
    fn test_host_register_component_extensive_stress_matrix() {
        let name_ascii = c"StressComponent".as_ptr();
        let uuid = ComponentUuid::from_name("StressComponent");

        // 1. ZST matrix with various power-of-two alignments
        let zst_alignments = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
        for &align in &zst_alignments {
            let desc = ModComponentDescriptor::new(name_ascii, uuid, 0, align);
            assert!(desc.is_valid());
            assert_eq!(desc.validate(), FfiResult::OK);
            assert!(desc.is_zst());
            assert!(desc.is_pod());
            assert!(desc.layout().is_some());

            let res = unsafe { call_register_component_fresh(&desc) };
            assert_eq!(
                res,
                FfiResult::OK,
                "Failed registering ZST with align {}",
                align
            );
        }

        // 2. Over-aligned non-ZST types
        let overaligned_configs = [
            (16, 16),
            (32, 16),
            (64, 64),
            (128, 64),
            (256, 256),
            (1024, 512),
            (4096, 4096),
        ];
        for &(size, align) in &overaligned_configs {
            let desc = ModComponentDescriptor::new(name_ascii, uuid, size, align);
            assert!(desc.is_valid());
            assert_eq!(desc.validate(), FfiResult::OK);
            let res = unsafe { call_register_component_fresh(&desc) };
            assert_eq!(res, FfiResult::OK);
        }

        // 3. Invalid non-power-of-two alignments
        let invalid_alignments = [0, 3, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 24, 100, 333];
        for &align in &invalid_alignments {
            let desc = ModComponentDescriptor {
                struct_size: 64,
                name: name_ascii,
                uuid,
                size: align * 2,
                align,
                drop_fn: None,
                vtable_ptr: core::ptr::null(),
            };
            assert!(!desc.is_valid());
            let res = unsafe { call_register_component_fresh(&desc) };
            assert_eq!(
                res,
                FfiResult::INVALID_ARGUMENT,
                "Alignment {} must be rejected with INVALID_ARGUMENT",
                align
            );
        }

        // 4. Misaligned sizes (size % align != 0)
        let misaligned_configs = [
            (1, 2),
            (3, 4),
            (7, 8),
            (15, 8),
            (17, 16),
            (31, 32),
            (65, 64),
        ];
        for &(size, align) in &misaligned_configs {
            let desc = ModComponentDescriptor {
                struct_size: 64,
                name: name_ascii,
                uuid,
                size,
                align,
                drop_fn: None,
                vtable_ptr: core::ptr::null(),
            };
            assert!(!desc.is_valid());
            let res = unsafe { call_register_component_fresh(&desc) };
            assert_eq!(
                res,
                FfiResult::INVALID_ARGUMENT,
                "Size {} with align {} must be rejected with INVALID_ARGUMENT",
                size,
                align
            );
        }

        // 5. Valid forward-compatible struct_size
        let extended_sizes = [64, 72, 80, 96, 128, 256, 1024, 65536];
        for &sz in &extended_sizes {
            let mut desc = ModComponentDescriptor::new(name_ascii, uuid, 16, 8);
            desc.struct_size = sz;
            let res = unsafe { call_register_component_fresh(&desc) };
            assert_eq!(res, FfiResult::OK);
        }

        // 6. Truncated struct_size
        let truncated_sizes = [0, 1, 4, 8, 16, 32, 48, 63];
        for &sz in &truncated_sizes {
            let mut desc = ModComponentDescriptor::new(name_ascii, uuid, 16, 8);
            desc.struct_size = sz;
            let res = unsafe { call_register_component_fresh(&desc) };
            assert_eq!(res, FfiResult::INVALID_VERSION);
        }

        // 7. Non-ASCII UTF-8 component names
        let utf8_names = [
            CString::new("生命值").unwrap(),
            CString::new("Компонент").unwrap(),
            CString::new("CaféAuLaitTag").unwrap(),
        ];
        for c_name in &utf8_names {
            let desc = ModComponentDescriptor::new(c_name.as_ptr(), uuid, 8, 8);
            let res = unsafe { call_register_component_fresh(&desc) };
            assert_eq!(res, FfiResult::OK);
        }

        // 8. Lifecycle drop hook execution
        let drop_desc =
            ModComponentDescriptor::new(name_ascii, uuid, 16, 8).with_drop(custom_test_drop_hook);
        assert!(!drop_desc.is_pod());
        assert!(drop_desc.drop_fn.is_some());

        let res = unsafe { call_register_component_fresh(&drop_desc) };
        assert_eq!(res, FfiResult::OK);

        DROP_CALL_COUNT.store(0, Ordering::SeqCst);
        let mut dummy_val = 0x1234_u64;
        unsafe {
            (drop_desc.drop_fn.unwrap())(&mut dummy_val as *mut u64 as *mut c_void);
        }
        assert_eq!(DROP_CALL_COUNT.load(Ordering::SeqCst), 1);

        // 9. Static VTable pointer preservation
        let dummy_vtable = 0x9999_usize as *const c_void;
        let vtable_desc =
            ModComponentDescriptor::new(name_ascii, uuid, 8, 8).with_vtable(dummy_vtable);
        assert_eq!(vtable_desc.vtable_ptr, dummy_vtable);
        let res = unsafe { call_register_component_fresh(&vtable_desc) };
        assert_eq!(res, FfiResult::OK);
    }
}

mod dynamic_descriptor_layouts {
    #![allow(unused_imports, dead_code)]
    //! Empirical Adversarial Stress Test Suite for Milestone 1:
    //! C-ABI Dynamic Component Contract & C Headers (`ModComponentDescriptor`, `HostApi`).

    use bevy_abi_mod_host::ModRegistry;
    use bevy_abi_mod_host_sys::{
        HOST_API, ModInitContext, host_get_proc_address, host_register_component,
    };
    use bevy_abi_mod_sys::{
        ComponentUuid, FfiResult, HostApi, ModComponentDescriptor, RegisterComponentFn,
    };
    use bevy_ecs::world::World;
    use core::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    unsafe fn call_register_component_with_fn(
        reg_fn: RegisterComponentFn,
        desc: &ModComponentDescriptor,
    ) -> FfiResult {
        let mut world = World::new();
        let reg = Arc::new(ModRegistry::new());
        world.insert_resource((*reg).clone());
        let mut init_ctx =
            ModInitContext::with_world(reg, Arc::new(AtomicBool::new(true)), &mut world as *mut _);
        let _guard = init_ctx.make_active();
        let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();
        unsafe { reg_fn(ctx_ptr, desc) }
    }

    unsafe fn call_register_component_fresh(desc: &ModComponentDescriptor) -> FfiResult {
        unsafe { call_register_component_with_fn(host_register_component, desc) }
    }

    // ============================================================================
    // 2. Massive Structs Stress Tests
    // ============================================================================

    #[test]
    fn test_massive_struct_descriptors() {
        let name = c"MassiveComponent".as_ptr();
        let uuid = ComponentUuid::from_name("MassiveComponent");

        // 1MB struct with 16-byte alignment
        let size_1mb = 1024 * 1024;
        let desc_1mb = ModComponentDescriptor::new(name, uuid, size_1mb, 16);
        assert!(!desc_1mb.is_zst());
        assert!(desc_1mb.is_valid());
        assert_eq!(desc_1mb.validate(), FfiResult::OK);
        let layout = desc_1mb.layout().expect("1MB layout must be valid");
        assert_eq!(layout.size(), size_1mb);
        assert_eq!(layout.align(), 16);
        let res = unsafe { call_register_component_fresh(&desc_1mb) };
        assert_eq!(res, FfiResult::OK);

        // 64MB struct with 64-byte alignment
        let size_64mb = 64 * 1024 * 1024;
        let desc_64mb = ModComponentDescriptor::new(name, uuid, size_64mb, 64);
        assert!(!desc_64mb.is_zst());
        assert!(desc_64mb.is_valid());
        assert_eq!(desc_64mb.validate(), FfiResult::OK);
        let res = unsafe { call_register_component_fresh(&desc_64mb) };
        assert_eq!(res, FfiResult::OK);
    }

    // ============================================================================
    // 7. Malformed UTF-8 Component Names
    // ============================================================================

    #[test]
    fn test_malformed_utf8_names() {
        let uuid = ComponentUuid::from_name("BadUtf8Comp");

        let bad_utf8_sequences: [&[u8]; 6] = [
            &[0xFF, 0x00],                   // Invalid starting byte
            &[0xFE, 0x00],                   // Invalid starting byte
            &[0xC0, 0xAF, 0x00],             // Overlong ASCII slash
            &[0xED, 0xA0, 0x80, 0x00],       // UTF-16 surrogate half
            &[0xF4, 0x90, 0x80, 0x80, 0x00], // Beyond Unicode maximum (U+10FFFF)
            &[0xE2, 0x82, 0x00],             // Truncated multi-byte sequence
        ];

        for (idx, &bad_bytes) in bad_utf8_sequences.iter().enumerate() {
            let desc = ModComponentDescriptor::new(
                bad_bytes.as_ptr() as *const core::ffi::c_char,
                uuid,
                8,
                8,
            );

            let res = unsafe { call_register_component_fresh(&desc) };
            assert_eq!(
                res,
                FfiResult::INVALID_UTF8,
                "Sequence {idx} must be rejected with INVALID_UTF8"
            );
        }
    }

    // ============================================================================
    // 8. HostApi Layout Stability & Byte Offsets 0..64
    // ============================================================================

    #[test]
    fn test_host_api_layout_stability_and_byte_offsets() {
        // Assert 64-bit layout
        assert_eq!(core::mem::size_of::<HostApi>(), 72);
        assert_eq!(core::mem::align_of::<HostApi>(), 8);

        // Verify historical offsets 0..64 are completely preserved:
        assert_eq!(core::mem::offset_of!(HostApi, struct_size), 0);
        assert_eq!(core::mem::offset_of!(HostApi, abi_version), 4);
        assert_eq!(core::mem::offset_of!(HostApi, register_system), 8);
        assert_eq!(core::mem::offset_of!(HostApi, iter_next_chunk), 16);
        assert_eq!(core::mem::offset_of!(HostApi, iter_column_readonly), 24);
        assert_eq!(core::mem::offset_of!(HostApi, iter_column_mutable), 32);
        assert_eq!(core::mem::offset_of!(HostApi, iter_mark_changed), 40);
        assert_eq!(core::mem::offset_of!(HostApi, iter_entities), 48);
        assert_eq!(core::mem::offset_of!(HostApi, iter_column_stride), 56);

        // Offset 64 is the new register_component field:
        assert_eq!(core::mem::offset_of!(HostApi, register_component), 64);

        // Global static HOST_API instance
        assert_eq!(HOST_API.struct_size, 72);
        assert_ne!(HOST_API.register_component as *const () as usize, 0);
        assert_eq!(
            HOST_API.register_component as *const () as usize,
            host_register_component as *const () as usize
        );
    }

    // ============================================================================
    // 9. Callable Invocation Parity Matrix Across All Resolution Paths
    // ============================================================================

    #[test]
    fn test_callable_invocation_parity_matrix() {
        let registry = ModRegistry::new();
        let reg_arc = Arc::new(registry);
        let _guard = reg_arc.make_current();

        let name = c"ParityComponent".as_ptr();
        let uuid = ComponentUuid::from_name("ParityComponent");

        // Build a matrix of descriptors (both valid and invalid)
        let valid_desc = ModComponentDescriptor::new(name, uuid, 32, 8);

        let mut truncated_desc = valid_desc;
        truncated_desc.struct_size = 32;

        let mut misaligned_desc = valid_desc;
        misaligned_desc.size = 15;

        let bad_utf8 = [0xFF, 0x00];
        let bad_utf8_desc =
            ModComponentDescriptor::new(bad_utf8.as_ptr() as *const core::ffi::c_char, uuid, 32, 8);

        let descriptors: [(&str, ModComponentDescriptor, FfiResult); 4] = [
            ("valid", valid_desc, FfiResult::OK),
            ("truncated", truncated_desc, FfiResult::INVALID_VERSION),
            ("misaligned", misaligned_desc, FfiResult::INVALID_ARGUMENT),
            ("bad_utf8", bad_utf8_desc, FfiResult::INVALID_UTF8),
        ];

        // Resolve pointers through all 5 mechanisms
        let direct_fn = host_register_component;
        let host_api_fn = HOST_API.register_component;
        let reg_sym_fn = reg_arc
            .resolve_symbol("register_component")
            .expect("symbol register_component must resolve");
        let reg_sym_fn: RegisterComponentFn = unsafe { core::mem::transmute(reg_sym_fn) };

        let reg_host_sym_fn = reg_arc
            .resolve_symbol("host_register_component")
            .expect("symbol host_register_component must resolve");
        let reg_host_sym_fn: RegisterComponentFn = unsafe { core::mem::transmute(reg_host_sym_fn) };

        let gpa_sym_ptr = unsafe { host_get_proc_address(c"register_component".as_ptr()) };
        assert!(!gpa_sym_ptr.is_null());
        let gpa_sym_fn: RegisterComponentFn = unsafe { core::mem::transmute(gpa_sym_ptr) };

        let gpa_host_sym_ptr =
            unsafe { host_get_proc_address(c"host_register_component".as_ptr()) };
        assert!(!gpa_host_sym_ptr.is_null());
        let gpa_host_sym_fn: RegisterComponentFn =
            unsafe { core::mem::transmute(gpa_host_sym_ptr) };

        // Verify all function pointer addresses are bitwise identical
        let target_addr = direct_fn as *const () as usize;
        assert_eq!(host_api_fn as *const () as usize, target_addr);
        assert_eq!(reg_sym_fn as *const () as usize, target_addr);
        assert_eq!(reg_host_sym_fn as *const () as usize, target_addr);
        assert_eq!(gpa_sym_fn as *const () as usize, target_addr);
        assert_eq!(gpa_host_sym_fn as *const () as usize, target_addr);

        // Execute each descriptor across all 6 invocation paths
        for (label, desc, expected_res) in descriptors {
            let r1 = unsafe { call_register_component_with_fn(direct_fn, &desc) };
            let r2 = unsafe { call_register_component_with_fn(host_api_fn, &desc) };
            let r3 = unsafe { call_register_component_with_fn(reg_sym_fn, &desc) };
            let r4 = unsafe { call_register_component_with_fn(reg_host_sym_fn, &desc) };
            let r5 = unsafe { call_register_component_with_fn(gpa_sym_fn, &desc) };
            let r6 = unsafe { call_register_component_with_fn(gpa_host_sym_fn, &desc) };

            assert_eq!(r1, expected_res, "{label} via direct_fn");
            assert_eq!(r2, expected_res, "{label} via host_api_fn");
            assert_eq!(r3, expected_res, "{label} via reg_sym_fn");
            assert_eq!(r4, expected_res, "{label} via reg_host_sym_fn");
            assert_eq!(r5, expected_res, "{label} via gpa_sym_fn");
            assert_eq!(r6, expected_res, "{label} via gpa_host_sym_fn");
        }
    }

    // ============================================================================
    // 10. Lifecycle Hooks & Custom Drop Callback Invocation
    // ============================================================================

    static DROP_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn test_custom_drop(ptr: *mut c_void) {
        if !ptr.is_null() {
            DROP_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn test_descriptor_with_drop_and_vtable() {
        let name = c"CustomDropComp".as_ptr();
        let uuid = ComponentUuid::from_name("CustomDropComp");

        let fake_vtable: usize = 0xcafe_babe;
        let desc = ModComponentDescriptor::new(name, uuid, 16, 8)
            .with_drop(test_custom_drop)
            .with_vtable(fake_vtable as *const c_void);

        assert!(!desc.is_pod());
        assert!(desc.drop_fn.is_some());
        assert_eq!(
            desc.drop_fn.unwrap() as *const () as usize,
            test_custom_drop as *const () as usize
        );
        assert_eq!(desc.vtable_ptr, fake_vtable as *const c_void);

        let res = unsafe { call_register_component_fresh(&desc) };
        assert_eq!(res, FfiResult::OK);

        // Exercise calling the drop callback safely
        let mut payload = [42u8; 16];
        let before_count = DROP_CALL_COUNT.load(Ordering::SeqCst);
        if let Some(drop_fn) = desc.drop_fn {
            unsafe { drop_fn(payload.as_mut_ptr() as *mut c_void) };
        }
        assert_eq!(DROP_CALL_COUNT.load(Ordering::SeqCst), before_count + 1);
    }

    // ============================================================================
    // 11. Extreme Limits & Edge Cases (usize::MAX and Maximum Alignments)
    // ============================================================================

    #[test]
    fn test_extreme_size_and_alignment_limits() {
        let name = c"ExtremeComp".as_ptr();
        let uuid = ComponentUuid::from_name("ExtremeComp");

        // Rust Layout limit on 64-bit platforms:
        // Layout requires size <= isize::MAX as usize
        // If size == usize::MAX, Layout::from_size_align returns Err(LayoutError).
        let desc_usize_max = ModComponentDescriptor::new(name, uuid, usize::MAX, 1);
        assert!(
            desc_usize_max.layout().is_none(),
            "usize::MAX layout must be None"
        );
        let res_max = unsafe { call_register_component_fresh(&desc_usize_max) };
        assert_eq!(res_max, FfiResult::INVALID_ARGUMENT);

        // Alignment 1 << 63
        let layout_res = core::alloc::Layout::from_size_align(0, 1usize << 63);
        let _ = layout_res;
        let desc_high_align = ModComponentDescriptor::new(name, uuid, 0, 1usize << 63);
        assert!(desc_high_align.align.is_power_of_two());

        // Alignment 1 << 29 (536,870,912) is the maximum supported Rust alignment
        let desc_max_rust_align = ModComponentDescriptor::new(name, uuid, 0, 1usize << 29);
        assert!(desc_max_rust_align.align.is_power_of_two());
        assert!(desc_max_rust_align.layout().is_some());
        let res = unsafe { call_register_component_fresh(&desc_max_rust_align) };
        assert_eq!(res, FfiResult::OK);
    }
}

mod transparent_ffi_primitives {
    #![allow(unused_imports, dead_code)]
    use crate::support::*;
    use bevy_abi_mod_host::{AbiModPlugin, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        ComponentAccessDescOwned, DynamicCSystem, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_column_stride, host_iter_mark_changed,
        host_iter_next_chunk,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
        ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue,
    };
    use bevy_app::App;
    use bevy_ecs::component::Component;
    use bevy_ecs::schedule::{LogLevel, Schedule, ScheduleBuildSettings};
    use bevy_ecs::system::{System, SystemAccess};
    use bevy_ecs::world::World;
    use libloading::{Library, Symbol};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ============================================================================
    // Alignment Verification Components
    // ============================================================================

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C, align(1))]
    pub struct AlignByte {
        pub val: u8,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
    #[repr(C, align(4))]
    pub struct AlignWord {
        pub val: u32,
    }

    /// Test 3: FFI transparent enum and newtype safety.
    #[test]
    fn test_ffi_transparent_type_safety() {
        let valid_ok = FfiResult::OK;
        assert!(valid_ok.is_ok());
        assert!(!valid_ok.is_err());

        let panic_res = FfiResult::PANIC_CAUGHT;
        assert!(!panic_res.is_ok());
        assert!(panic_res.is_err());

        // Foreign arbitrary u32 values
        let foreign_code = FfiResult(0xCAFEBABE);
        assert!(foreign_code.is_err());
        assert!(!foreign_code.is_ok());

        let foreign_zero = FfiResult(0);
        assert!(foreign_zero.is_ok());

        // ComponentAccessMode transparent u32
        let read_mode = ComponentAccessMode::READ;
        assert_eq!(read_mode.0, 0);
        let write_mode = ComponentAccessMode::WRITE;
        assert_eq!(write_mode.0, 1);
        let filter_mode = ComponentAccessMode::FILTER_ONLY;
        assert_eq!(filter_mode.0, 2);
        let without_mode = ComponentAccessMode::WITHOUT;
        assert_eq!(without_mode.0, 3);

        // FfiEntity bits transparency
        let ent = FfiEntity(0x0000000100000005);
        assert_eq!(ent.0, 0x0000000100000005);
        assert_eq!(FfiEntity::NULL.0, 0);
        #[allow(deprecated)]
        let legacy_placeholder = FfiEntity::PLACEHOLDER;
        assert_eq!(legacy_placeholder.0, u64::MAX);
    }
}

mod query_limits_and_placeholder_entities {
    #![allow(unused_imports, dead_code)]
    use crate::support::{NameTag, Position};
    use bevy_abi_mod_host::{
        AbiModPlugin, LoadedModInfo, ModAppExt, ModErrorPolicy, ModId, ModLoadError, ModLoaded,
        ModRegistry, ModSchedule, ModSystemFaulted, ModSystemSet, ModUnloaded,
        is_abi_version_compatible,
    };
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, ComponentAccessDescOwned, DynamicCSystem, MAX_QUERY_COMPONENTS,
        ModInitContext, host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty,
        host_get_proc_address, host_iter_column_mutable, host_iter_column_readonly,
        host_iter_entities, host_iter_next_chunk, host_iter_reset, host_register_component,
        host_register_multi_query_system, host_register_scheduled_system, host_register_system,
        host_register_system_in_schedule,
    };
    use bevy_abi_mod_sys::{
        ABI_VERSION_PACKED, ComponentAccessDesc, ComponentAccessMode, ComponentUuid,
        ExposableComponent, FfiEntity, FfiResult, ModComponentDescriptor,
        MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue,
        RegisterScheduledSystemFn, ScheduleLabelId, ScheduledMultiQuerySystemDescriptor,
        ScheduledSystemDescriptor, SystemDescriptor,
    };
    use bevy_app::{App, Startup, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::message::Messages;
    use bevy_ecs::observer::On;
    use bevy_ecs::query::{Changed, With};
    use bevy_ecs::schedule::{
        IntoScheduleConfigs, ScheduleBuildSettings, ScheduleCleanupPolicy, Schedules,
    };
    use bevy_ecs::system::RunSystemOnce;
    use bevy_ecs::world::{CommandQueue, World};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    unsafe extern "C" fn noop_single_cb(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    unsafe extern "C" fn noop_multi_cb(
        _iters: *const *mut RawChunkIter,
        _count: usize,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    #[test]
    fn d8_rejects_query_count_exceeding_max_query_components() {
        let registry = Arc::new(ModRegistry::new());
        let mut init_ctx = ModInitContext::new(registry);
        let _guard = init_ctx.make_active();
        let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

        let names: Vec<std::ffi::CString> = (0..=MAX_QUERY_COMPONENTS)
            .map(|i| std::ffi::CString::new(format!("Comp_{i}")).unwrap())
            .collect();
        let queries: Vec<ComponentAccessDesc> = names
            .iter()
            .map(|n| ComponentAccessDesc::read(n.as_ptr()))
            .collect();
        assert_eq!(queries.len(), MAX_QUERY_COMPONENTS + 1);

        let single_desc = SystemDescriptor {
            struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
            name: c"too_many_cols".as_ptr(),
            queries: queries.as_ptr(),
            query_count: queries.len(),
            callback: noop_single_cb,
            user_data: core::ptr::null_mut(),
        };
        // SAFETY: Valid pointers and initialized slices.
        assert_eq!(
            unsafe { host_register_system(ctx_ptr, &single_desc) },
            FfiResult::OUT_OF_RANGE
        );
        drop(_guard);
        assert!(init_ctx.registered_systems.is_empty());

        let _guard = init_ctx.make_active();
        let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();
        let spec = QuerySpec {
            queries: queries.as_ptr(),
            query_count: queries.len(),
        };
        let multi_desc = MultiQuerySystemDescriptor {
            struct_size: core::mem::size_of::<MultiQuerySystemDescriptor>() as u32,
            name: c"too_many_multi_cols".as_ptr(),
            query_specs: &spec,
            query_spec_count: 1,
            callback: noop_multi_cb,
            user_data: core::ptr::null_mut(),
        };
        // SAFETY: Valid pointers and initialized slices.
        assert_eq!(
            unsafe { host_register_multi_query_system(ctx_ptr, &multi_desc) },
            FfiResult::OUT_OF_RANGE
        );
        drop(_guard);
        assert!(init_ctx.registered_systems.is_empty());
    }

    #[test]
    fn d9_and_d10_null_placeholder_entities_rejected_and_overaligned_zst_succeeds() {
        let mut world = World::new();
        let mut reg = ModRegistry::new();
        let pos_id = world.register_component::<Position>();
        reg.register_component::<Position>(pos_id);
        world.insert_resource(reg);

        // Register a 64-byte aligned ZST dynamic component (D10)
        let reg_arc = Arc::new(world.resource::<ModRegistry>().clone());
        let is_active = Arc::new(AtomicBool::new(true));
        let mut init_ctx = ModInitContext::with_world(reg_arc, is_active, &mut world as *mut World);
        let _guard = init_ctx.make_active();
        let zst_desc = ModComponentDescriptor::new(
            c"Aligned64Zst".as_ptr(),
            ComponentUuid::from_name("Aligned64Zst"),
            0,
            64,
        );
        // SAFETY: Valid ModInitContext and descriptor.
        assert_eq!(
            unsafe {
                host_register_component(
                    &mut init_ctx as *mut ModInitContext as *mut c_void,
                    &zst_desc,
                )
            },
            FfiResult::OK
        );
        drop(_guard);

        let zst_id = world
            .resource::<ModRegistry>()
            .get_by_name("Aligned64Zst")
            .unwrap()
            .component_id;

        // D9: null queue returns FfiEntity(0)
        assert_eq!(
            unsafe { host_cmd_spawn_empty(core::ptr::null_mut()) },
            FfiEntity(0)
        );

        let reg_snap = world.resource::<ModRegistry>().clone();
        let mut queue = CommandQueue::default();
        let mut bridge = CommandQueueBridge::new(&mut queue, world.entity_allocator(), &reg_snap);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        let placeholder_ffi = FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits());
        let pos_bytes = [0u8; core::mem::size_of::<Position>()];

        // Reject FfiEntity(0) and Entity::PLACEHOLDER in both despawn and insert_component!
        assert_eq!(
            unsafe { host_cmd_despawn(raw_queue, FfiEntity(0)) },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe { host_cmd_despawn(raw_queue, placeholder_ffi) },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    FfiEntity(0),
                    c"Position".as_ptr(),
                    pos_bytes.as_ptr(),
                    pos_bytes.len(),
                )
            },
            FfiResult::INVALID_ENTITY
        );
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    placeholder_ffi,
                    c"Position".as_ptr(),
                    pos_bytes.as_ptr(),
                    pos_bytes.len(),
                )
            },
            FfiResult::INVALID_ENTITY
        );

        // D10: Insert 64-byte-aligned ZST onto a real spawned entity without panicking in OwningPtr::new!
        let valid_ffi = unsafe { host_cmd_spawn_empty(raw_queue) };
        assert_ne!(valid_ffi, FfiEntity(0));
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    valid_ffi,
                    c"Aligned64Zst".as_ptr(),
                    core::ptr::null(),
                    0,
                )
            },
            FfiResult::OK
        );

        queue.apply(&mut world);
        let valid_entity = Entity::try_from_bits(valid_ffi.to_bits()).unwrap();
        assert!(world.entity(valid_entity).contains_id(zst_id));
    }
}
