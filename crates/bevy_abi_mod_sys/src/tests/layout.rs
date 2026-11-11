use core::{
    ffi::{c_char, c_void},
    fmt::Write,
};

use crate::*;

struct StackBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> StackBuf<N> {
    const fn new() -> Self {
        Self {
            bytes: [0u8; N],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap()
    }
}

impl<const N: usize> Write for StackBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let src = s.as_bytes();
        if self.len + src.len() > N {
            return Err(core::fmt::Error);
        }
        self.bytes[self.len..self.len + src.len()].copy_from_slice(src);
        self.len += src.len();
        Ok(())
    }
}

#[test]
fn test_ffi_entity_bits_roundtrip() {
    let bits = 0x0000_0007_0000_002a_u64;
    let entity = FfiEntity::from_bits(bits);
    assert_eq!(entity.to_bits(), bits);
}

#[test]
fn test_ffi_result_transparent_soundness_and_taxonomy() {
    assert!(FfiResult::OK.is_ok());
    assert!(!FfiResult::OK.is_err());
    assert!(FfiResult::PANIC_CAUGHT.is_err());

    let known = [
        (FfiResult::OK, 0, "OK"),
        (FfiResult::INVALID_COMPONENT, 1, "INVALID_COMPONENT"),
        (FfiResult::ACCESS_CONFLICT, 2, "ACCESS_CONFLICT"),
        (FfiResult::PANIC_CAUGHT, 3, "PANIC_CAUGHT"),
        (FfiResult::NULL_POINTER, 4, "NULL_POINTER"),
        (FfiResult::INVALID_UTF8, 5, "INVALID_UTF8"),
        (FfiResult::PERMISSION_DENIED, 6, "PERMISSION_DENIED"),
        (FfiResult::INVALID_VERSION, 7, "INVALID_VERSION"),
        (FfiResult::INVALID_ARGUMENT, 8, "INVALID_ARGUMENT"),
        (FfiResult::OUT_OF_RANGE, 9, "OUT_OF_RANGE"),
        (FfiResult::NOT_FOUND, 10, "NOT_FOUND"),
        (FfiResult::INVALID_STATE, 11, "INVALID_STATE"),
        (FfiResult::INVALID_ENTITY, 12, "INVALID_ENTITY"),
        (FfiResult::CAPACITY_EXCEEDED, 13, "CAPACITY_EXCEEDED"),
        (FfiResult::UNSUPPORTED, 14, "UNSUPPORTED"),
        (FfiResult::INTERNAL_ERROR, 99, "INTERNAL_ERROR"),
    ];

    for (i, &(lhs, expected_code, expected_name)) in known.iter().enumerate() {
        assert_eq!(lhs.0, expected_code);
        assert_eq!(lhs.name(), expected_name);
        if expected_code == 0 {
            assert!(lhs.is_ok());
            assert!(!lhs.is_err());
        } else {
            assert!(!lhs.is_ok());
            assert!(lhs.is_err());
        }
        let mut buf = StackBuf::<64>::new();
        write!(&mut buf, "{lhs}").unwrap();
        assert_eq!(buf.as_str(), expected_name);

        for &(rhs, _, _) in &known[i + 1..] {
            assert_ne!(lhs, rhs, "FfiResult codes must not collide");
        }
    }

    // Arbitrary unknown integer from C must never cause UB
    let unknown_from_c = FfiResult(12345);
    assert!(unknown_from_c.is_err());
    assert_eq!(unknown_from_c.0, 12345);
    assert_eq!(unknown_from_c.name(), "UNKNOWN");
    let mut unknown_buf = StackBuf::<64>::new();
    write!(&mut unknown_buf, "{unknown_from_c}").unwrap();
    assert_eq!(unknown_buf.as_str(), "UNKNOWN(12345)");
}

#[test]
fn test_component_access_mode_validity() {
    assert!(ComponentAccessMode::READ.is_valid());
    assert!(!ComponentAccessMode::READ.is_mutable());
    assert!(ComponentAccessMode::WRITE.is_valid());
    assert!(ComponentAccessMode::WRITE.is_mutable());
    assert!(ComponentAccessMode::FILTER_ONLY.is_valid());
    assert!(ComponentAccessMode::FILTER_ONLY.is_filter_only());
    assert!(!ComponentAccessMode(99).is_valid());
}

#[test]
fn test_ffi_slice_and_str_view() {
    let text = "hello_mod";
    let slice = FfiSlice::from_str(text);
    assert!(!slice.is_empty());
    assert_eq!(slice.len, 9);
    // SAFETY: `slice` was constructed from a valid static `&str`.
    let recovered = unsafe { slice.as_str().unwrap() };
    assert_eq!(recovered, "hello_mod");

    let view: StrView = slice;
    // SAFETY: `view` points to the same valid static `&str`.
    assert_eq!(unsafe { view.as_str().unwrap() }, "hello_mod");

    let empty = FfiSlice::<u8>::empty();
    assert!(empty.is_empty());
    // SAFETY: `empty` has length 0.
    assert_eq!(unsafe { empty.as_slice() }, &[] as &[u8]);
}

#[test]
fn test_component_uuid_deterministic() {
    let u1 = ComponentUuid::from_name("Position");
    let u2 = ComponentUuid::from_name("Position");
    let u3 = ComponentUuid::from_name("Velocity");
    assert_eq!(u1, u2);
    assert_ne!(u1, u3);
}

#[test]
fn test_mod_component_descriptor_layout_and_offsets() {
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(core::mem::size_of::<ModComponentDescriptor>(), 64);
        assert_eq!(core::mem::align_of::<ModComponentDescriptor>(), 8);

        assert_eq!(
            core::mem::offset_of!(ModComponentDescriptor, struct_size),
            0
        );
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, name), 8);
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, uuid), 16);
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, size), 32);
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, align), 40);
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, drop_fn), 48);
        assert_eq!(
            core::mem::offset_of!(ModComponentDescriptor, vtable_ptr),
            56
        );
    }
}

#[test]
fn test_host_api_layout_and_offsets() {
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(core::mem::size_of::<HostApi>(), 72);
        assert_eq!(core::mem::align_of::<HostApi>(), 8);

        assert_eq!(core::mem::offset_of!(HostApi, struct_size), 0);
        assert_eq!(core::mem::offset_of!(HostApi, abi_version), 4);
        assert_eq!(core::mem::offset_of!(HostApi, register_system), 8);
        assert_eq!(core::mem::offset_of!(HostApi, iter_next_chunk), 16);
        assert_eq!(core::mem::offset_of!(HostApi, iter_column_readonly), 24);
        assert_eq!(core::mem::offset_of!(HostApi, iter_column_mutable), 32);
        assert_eq!(core::mem::offset_of!(HostApi, iter_mark_changed), 40);
        assert_eq!(core::mem::offset_of!(HostApi, iter_entities), 48);
        assert_eq!(core::mem::offset_of!(HostApi, iter_column_stride), 56);
        assert_eq!(core::mem::offset_of!(HostApi, register_component), 64);
    }
}

#[test]
fn test_mod_component_descriptor_validation_matrix() {
    let name = c"CustomData".as_ptr();
    let uuid = ComponentUuid::from_name("CustomData");

    // 1. Valid standard POD component
    let valid_desc = ModComponentDescriptor::new(name, uuid, 16, 8);
    assert!(valid_desc.is_valid());
    assert_eq!(valid_desc.validate(), FfiResult::OK);
    assert!(!valid_desc.is_zst());
    assert!(valid_desc.is_pod());
    assert!(valid_desc.layout().is_some());
    // SAFETY: `valid_desc.name` points to the static C string literal `"CustomData"`.
    unsafe {
        assert_eq!(
            valid_desc.name_cstr().unwrap().to_str().unwrap(),
            "CustomData"
        );
    }

    // 2. Valid Zero-Sized Type (ZST)
    let zst_desc = ModComponentDescriptor::new(name, uuid, 0, 1);
    assert!(zst_desc.is_valid());
    assert_eq!(zst_desc.validate(), FfiResult::OK);
    assert!(zst_desc.is_zst());
    assert!(zst_desc.is_pod());
    assert_eq!(zst_desc.layout().unwrap().size(), 0);
    assert_eq!(zst_desc.layout().unwrap().align(), 1);

    // 3. Valid over-aligned ZST
    let overaligned_zst = ModComponentDescriptor::new(name, uuid, 0, 16);
    assert!(overaligned_zst.is_valid());
    assert_eq!(overaligned_zst.validate(), FfiResult::OK);
    assert_eq!(overaligned_zst.layout().unwrap().align(), 16);

    // 4. Invalid null name
    let null_name = ModComponentDescriptor::new(core::ptr::null(), uuid, 16, 8);
    assert!(!null_name.is_valid());
    assert_eq!(null_name.validate(), FfiResult::NULL_POINTER);
    // SAFETY: `null_name.name` is null and `name_cstr` checks for null before `CStr::from_ptr`.
    unsafe {
        assert!(null_name.name_cstr().is_none());
    }

    // 5. Invalid zero alignment
    let mut zero_align = valid_desc;
    zero_align.align = 0;
    assert!(!zero_align.is_valid());
    assert_eq!(zero_align.validate(), FfiResult::INVALID_COMPONENT);
    assert!(zero_align.layout().is_none());

    // 6. Invalid non-power-of-two alignment
    let mut non_pot_align = valid_desc;
    non_pot_align.align = 7;
    assert!(!non_pot_align.is_valid());
    assert_eq!(non_pot_align.validate(), FfiResult::INVALID_COMPONENT);
    assert!(non_pot_align.layout().is_none());

    // 7. Invalid misaligned size (size % align != 0)
    let mut misaligned = valid_desc;
    misaligned.size = 10;
    misaligned.align = 4;
    assert!(!misaligned.is_valid());
    assert_eq!(misaligned.validate(), FfiResult::INVALID_COMPONENT);
    assert!(misaligned.layout().is_none());

    // 8. Invalid truncated struct_size (< MIN_V1_SIZE)
    let mut truncated = valid_desc;
    truncated.struct_size = (ModComponentDescriptor::MIN_V1_SIZE - 8) as u32;
    assert!(!truncated.is_valid());
    assert_eq!(truncated.validate(), FfiResult::INVALID_VERSION);

    // 9. Valid forward-compatible struct_size (> MIN_V1_SIZE)
    let mut forward_compat = valid_desc;
    forward_compat.struct_size = 128;
    assert!(forward_compat.is_valid());
    assert_eq!(forward_compat.validate(), FfiResult::OK);

    // 10. with_drop and with_vtable
    unsafe extern "C" fn dummy_drop(_ptr: *mut core::ffi::c_void) {}
    let drop_fn: ComponentDropFn = dummy_drop;
    let with_drop = valid_desc.with_drop(drop_fn);
    assert!(!with_drop.is_pod());
    assert_eq!(
        with_drop.drop_fn.map(|f| f as *const () as usize),
        Some(drop_fn as *const () as usize)
    );

    let vtable_dummy = core::ptr::without_provenance::<core::ffi::c_void>(0x1234);
    let with_vtable = valid_desc.with_vtable(vtable_dummy);
    assert_eq!(with_vtable.vtable_ptr, vtable_dummy);
}

#[test]
fn test_ffi_entity_null_sentinel() {
    assert_eq!(FfiEntity::NULL.to_bits(), 0);
    assert!(FfiEntity::NULL.is_null());
    assert_eq!(FfiEntity::default(), FfiEntity::NULL);

    let valid_first_entity = FfiEntity::from_bits(0xFFFF_FFFE);
    assert!(!valid_first_entity.is_null());
    let bevy_placeholder_bits = FfiEntity::from_bits(1);
    assert!(!bevy_placeholder_bits.is_null());
}

#[test]
fn regression_c06_zero_major_semver() {
    let v0_0_9 = pack_abi_version(0, 0, 9);
    let v0_1_0 = pack_abi_version(0, 1, 0);
    let v0_1_9 = pack_abi_version(0, 1, 9);
    let v0_2_0 = pack_abi_version(0, 2, 0);
    let v0_2_4 = pack_abi_version(0, 2, 4);

    let v1_0_0 = pack_abi_version(1, 0, 0);
    let v1_1_0 = pack_abi_version(1, 1, 0);
    let v1_1_7 = pack_abi_version(1, 1, 7);
    let v1_2_0 = pack_abi_version(1, 2, 0);
    let v2_0_0 = pack_abi_version(2, 0, 0);

    // Pre-1.0 (`major == 0`): exact minor match required; patch differences remain compatible.
    assert!(abi_version_is_compatible(v0_1_0, v0_1_0));
    assert!(abi_version_is_compatible(v0_1_0, v0_1_9));
    assert!(abi_version_is_compatible(v0_1_9, v0_1_0));
    assert!(abi_version_is_compatible(v0_2_0, v0_2_4));
    assert!(!abi_version_is_compatible(v0_1_0, v0_2_0));
    assert!(!abi_version_is_compatible(v0_2_0, v0_1_0));
    assert!(!abi_version_is_compatible(v0_1_0, v0_0_9));
    assert!(!abi_version_is_compatible(v0_0_9, v0_1_0));

    // Post-1.0 (`major >= 1`): `host_minor >= guest_minor` is compatible; newer guest minor is rejected.
    assert!(abi_version_is_compatible(v1_1_0, v1_1_7));
    assert!(abi_version_is_compatible(v1_2_0, v1_1_0));
    assert!(abi_version_is_compatible(v1_2_0, v1_0_0));
    assert!(!abi_version_is_compatible(v1_1_0, v1_2_0));

    // Distinct major versions are always incompatible.
    assert!(!abi_version_is_compatible(v1_0_0, v0_1_0));
    assert!(!abi_version_is_compatible(v0_2_0, v1_0_0));
    assert!(!abi_version_is_compatible(v2_0_0, v1_2_0));

    // Boundary values at max packed field widths succeed without truncation.
    let max_ver = pack_abi_version(255, 255, 65535);
    assert_eq!(abi_version_major(max_ver), 255);
    assert_eq!(abi_version_minor(max_ver), 255);
    assert_eq!(abi_version_patch(max_ver), 65535);
}

#[test]
#[should_panic(expected = "ABI major version exceeds 8-bit range")]
fn test_pack_abi_version_major_overflow_panics() {
    let _ = pack_abi_version(256, 0, 0);
}

#[test]
#[should_panic(expected = "ABI minor version exceeds 8-bit range")]
fn test_pack_abi_version_minor_overflow_panics() {
    let _ = pack_abi_version(0, 256, 0);
}

#[test]
#[should_panic(expected = "ABI patch version exceeds 16-bit range")]
fn test_pack_abi_version_patch_overflow_panics() {
    let _ = pack_abi_version(0, 0, 65536);
}

#[test]
fn regression_c02_min_v1_size_frozen() {
    unsafe extern "C" fn dummy_reg_sys(
        _ctx: *mut c_void,
        _desc: *const SystemDescriptor,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn dummy_next_chunk(_iter: *mut RawChunkIter) -> usize {
        0
    }
    unsafe extern "C" fn dummy_col_ro(_iter: *const RawChunkIter, _idx: usize) -> *const u8 {
        core::ptr::null()
    }
    unsafe extern "C" fn dummy_col_mut(_iter: *mut RawChunkIter, _idx: usize) -> *mut u8 {
        core::ptr::null_mut()
    }
    unsafe extern "C" fn dummy_mark(
        _iter: *mut RawChunkIter,
        _idx: usize,
        _row: usize,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn dummy_entities(_iter: *const RawChunkIter) -> *const FfiEntity {
        core::ptr::null()
    }
    unsafe extern "C" fn dummy_stride(_iter: *const RawChunkIter, _idx: usize) -> usize {
        0
    }
    unsafe extern "C" fn dummy_reg_comp(
        _ctx: *mut c_void,
        _desc: *const ModComponentDescriptor,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn dummy_sys_cb(
        _iter: *mut RawChunkIter,
        _cmds: *mut RawCommandQueue,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn dummy_mq_cb(
        _iters: *const *mut RawChunkIter,
        _cnt: usize,
        _cmds: *mut RawCommandQueue,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    // Simulated future extended copies with appended zero-valid trailing fields:
    // `MIN_V1_SIZE` stays pinned via `offset_of!` to the end of the v1 field prefix.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct ExtendedHostApi {
        struct_size: u32,
        abi_version: u32,
        register_system: RegisterSystemFn,
        iter_next_chunk: IterNextChunkFn,
        iter_column_readonly: IterColumnReadonlyFn,
        iter_column_mutable: IterColumnMutableFn,
        iter_mark_changed: IterMarkChangedFn,
        iter_entities: IterEntitiesFn,
        iter_column_stride: IterColumnStrideFn,
        register_component: RegisterComponentFn,
        future_optional_hook: Option<unsafe extern "C" fn()>,
        future_flags: u64,
    }

    // SAFETY: `ExtendedHostApi` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0,
    // trailing fields past `register_component` are zero-valid, and `validate_raw_slot`
    // delegates to `HostApi::validate_raw_slot` over the identical v1 prefix.
    unsafe impl ExtensibleStruct for ExtendedHostApi {
        const MIN_V1_SIZE: usize = core::mem::offset_of!(Self, register_component)
            + core::mem::size_of::<RegisterComponentFn>();

        unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
            // SAFETY: Prefix `0..HostApi::MIN_V1_SIZE` has identical layout to `HostApi`.
            unsafe { HostApi::validate_raw_slot(slot_ptr.cast::<HostApi>()) }
        }
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct ExtendedSystemDescriptor {
        struct_size: u32,
        name: *const c_char,
        queries: *const ComponentAccessDesc,
        query_count: usize,
        callback: DynamicSystemCallback,
        user_data: *mut c_void,
        future_priority: u64,
        future_hook: Option<unsafe extern "C" fn()>,
    }

    // SAFETY: `ExtendedSystemDescriptor` starts with `struct_size: u32`, trailing fields past
    // `user_data` are zero-valid, and `validate_raw_slot` checks `callback != 0`.
    unsafe impl ExtensibleStruct for ExtendedSystemDescriptor {
        const MIN_V1_SIZE: usize =
            core::mem::offset_of!(Self, user_data) + core::mem::size_of::<*mut c_void>();

        unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
            // SAFETY: Prefix `0..SystemDescriptor::MIN_V1_SIZE` matches `SystemDescriptor`.
            unsafe { SystemDescriptor::validate_raw_slot(slot_ptr.cast::<SystemDescriptor>()) }
        }
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct ExtendedMultiQuerySystemDescriptor {
        struct_size: u32,
        name: *const c_char,
        query_specs: *const QuerySpec,
        query_spec_count: usize,
        callback: MultiQuerySystemCallback,
        user_data: *mut c_void,
        future_tag: u64,
    }

    // SAFETY: `ExtendedMultiQuerySystemDescriptor` starts with `struct_size: u32`, trailing fields
    // past `user_data` are zero-valid, and `validate_raw_slot` checks `callback != 0`.
    unsafe impl ExtensibleStruct for ExtendedMultiQuerySystemDescriptor {
        const MIN_V1_SIZE: usize =
            core::mem::offset_of!(Self, user_data) + core::mem::size_of::<*mut c_void>();

        unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
            // SAFETY: Prefix `0..MultiQuerySystemDescriptor::MIN_V1_SIZE` matches `MultiQuerySystemDescriptor`.
            unsafe {
                MultiQuerySystemDescriptor::validate_raw_slot(
                    slot_ptr.cast::<MultiQuerySystemDescriptor>(),
                )
            }
        }
    }

    assert_eq!(ExtendedHostApi::MIN_V1_SIZE, HostApi::MIN_V1_SIZE);
    assert!(ExtendedHostApi::MIN_V1_SIZE < core::mem::size_of::<ExtendedHostApi>());
    assert_eq!(
        ExtendedSystemDescriptor::MIN_V1_SIZE,
        SystemDescriptor::MIN_V1_SIZE
    );
    assert!(
        ExtendedSystemDescriptor::MIN_V1_SIZE < core::mem::size_of::<ExtendedSystemDescriptor>()
    );
    assert_eq!(
        ExtendedMultiQuerySystemDescriptor::MIN_V1_SIZE,
        MultiQuerySystemDescriptor::MIN_V1_SIZE
    );
    assert!(
        ExtendedMultiQuerySystemDescriptor::MIN_V1_SIZE
            < core::mem::size_of::<ExtendedMultiQuerySystemDescriptor>()
    );

    let v1_api = HostApi {
        struct_size: HostApi::MIN_V1_SIZE as u32,
        abi_version: ABI_VERSION_PACKED,
        register_system: dummy_reg_sys,
        iter_next_chunk: dummy_next_chunk,
        iter_column_readonly: dummy_col_ro,
        iter_column_mutable: dummy_col_mut,
        iter_mark_changed: dummy_mark,
        iter_entities: dummy_entities,
        iter_column_stride: dummy_stride,
        register_component: dummy_reg_comp,
    };
    // SAFETY: `v1_api` is initialized for `HostApi::MIN_V1_SIZE` bytes on the stack.
    let ext_api =
        unsafe { read_descriptor::<ExtendedHostApi>((&raw const v1_api).cast::<c_void>()) }
            .expect("extended HostApi must accept frozen MIN_V1_SIZE payload");
    assert!(ext_api.future_optional_hook.is_none());
    assert_eq!(ext_api.future_flags, 0);

    let v1_sys = SystemDescriptor::new(
        c"v1_sys".as_ptr(),
        core::ptr::null(),
        0,
        dummy_sys_cb,
        core::ptr::without_provenance_mut(0xCAFE),
    );
    // SAFETY: `v1_sys` is initialized for `SystemDescriptor::MIN_V1_SIZE` bytes on the stack.
    let ext_sys = unsafe {
        read_descriptor::<ExtendedSystemDescriptor>((&raw const v1_sys).cast::<c_void>())
    }
    .expect("extended SystemDescriptor must accept frozen MIN_V1_SIZE payload");
    assert_eq!(ext_sys.user_data.addr(), 0xCAFE);
    assert_eq!(ext_sys.future_priority, 0);
    assert!(ext_sys.future_hook.is_none());

    let v1_mq = MultiQuerySystemDescriptor::new(
        c"v1_mq".as_ptr(),
        core::ptr::null(),
        0,
        dummy_mq_cb,
        core::ptr::without_provenance_mut(0xBEEF),
    );
    // SAFETY: `v1_mq` is initialized for `MultiQuerySystemDescriptor::MIN_V1_SIZE` bytes on the stack.
    let ext_mq = unsafe {
        read_descriptor::<ExtendedMultiQuerySystemDescriptor>((&raw const v1_mq).cast::<c_void>())
    }
    .expect("extended MultiQuerySystemDescriptor must accept frozen MIN_V1_SIZE payload");
    assert_eq!(ext_mq.user_data.addr(), 0xBEEF);
    assert_eq!(ext_mq.future_tag, 0);
}

#[test]
fn regression_c09_unknown_flags_rejected() {
    unsafe extern "C" fn dummy_sys_cb(
        _iter: *mut RawChunkIter,
        _cmds: *mut RawCommandQueue,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn dummy_mq_cb(
        _iters: *const *mut RawChunkIter,
        _cnt: usize,
        _cmds: *mut RawCommandQueue,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn dummy_cond(_ud: *mut c_void) -> FfiBool {
        FFI_BOOL_TRUE
    }

    let base_sys = SystemDescriptor::new(
        c"flag_check_sys".as_ptr(),
        core::ptr::null(),
        0,
        dummy_sys_cb,
        core::ptr::null_mut(),
    );
    let base_mq = MultiQuerySystemDescriptor::new(
        c"flag_check_mq".as_ptr(),
        core::ptr::null(),
        0,
        dummy_mq_cb,
        core::ptr::null_mut(),
    );

    for bad_flag in [1u32, 1 << 7, 1 << 31, 0xDEAD_BEEF, u32::MAX] {
        let mut sched_sys = base_sys.with_schedule(ScheduleLabelId::UPDATE);
        sched_sys.flags = bad_flag;
        assert_eq!(sched_sys.validate(), FfiResult::UNSUPPORTED);

        let mut cond_sys = base_sys.with_run_if(dummy_cond, core::ptr::null_mut());
        cond_sys.sched.flags = bad_flag;
        assert_eq!(cond_sys.validate(), FfiResult::UNSUPPORTED);

        let mut sched_mq = base_mq.with_schedule(ScheduleLabelId::UPDATE);
        sched_mq.flags = bad_flag;
        assert_eq!(sched_mq.validate(), FfiResult::UNSUPPORTED);

        let mut cond_mq = base_mq.with_run_if(dummy_cond, core::ptr::null_mut());
        cond_mq.sched.flags = bad_flag;
        assert_eq!(cond_mq.validate(), FfiResult::UNSUPPORTED);
    }
}

#[test]
fn test_schedule_label_id_variants_and_default() {
    assert_eq!(ScheduleLabelId::default(), ScheduleLabelId::UPDATE);
    assert_eq!(ScheduleLabelId::UPDATE.0, 0);
    assert_eq!(ScheduleLabelId::STARTUP.0, 1);
    assert_eq!(ScheduleLabelId::PRE_UPDATE.0, 2);
    assert_eq!(ScheduleLabelId::POST_UPDATE.0, 3);
    assert_eq!(ScheduleLabelId::FIXED_UPDATE.0, 4);
    assert_eq!(ScheduleLabelId::LAST.0, 5);

    for raw in 0..=5 {
        assert!(ScheduleLabelId(raw).is_valid());
    }
    assert!(!ScheduleLabelId(6).is_valid());
    assert!(!ScheduleLabelId(u32::MAX).is_valid());
}

#[test]
fn test_scheduled_system_descriptor_layout_and_zero_extension() {
    unsafe extern "C" fn dummy_sys(
        _iter: *mut RawChunkIter,
        _cmds: *mut RawCommandQueue,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(core::mem::size_of::<SystemDescriptor>(), 48);
        assert_eq!(core::mem::size_of::<ScheduledSystemDescriptor>(), 88);
        assert_eq!(core::mem::align_of::<ScheduledSystemDescriptor>(), 8);
        assert_eq!(core::mem::offset_of!(ScheduledSystemDescriptor, base), 0);
        assert_eq!(
            core::mem::offset_of!(ScheduledSystemDescriptor, schedule),
            48
        );
        assert_eq!(core::mem::offset_of!(ScheduledSystemDescriptor, flags), 52);
        assert_eq!(core::mem::offset_of!(ScheduledSystemDescriptor, after), 56);
        assert_eq!(
            core::mem::offset_of!(ScheduledSystemDescriptor, after_count),
            64
        );
        assert_eq!(core::mem::offset_of!(ScheduledSystemDescriptor, before), 72);
        assert_eq!(
            core::mem::offset_of!(ScheduledSystemDescriptor, before_count),
            80
        );
    }

    // 1. Older v0.1 SystemDescriptor (48 bytes) zero-extended to ScheduledSystemDescriptor (88 bytes)
    let v1_desc = SystemDescriptor::new(
        c"legacy_sys".as_ptr(),
        core::ptr::null(),
        0,
        dummy_sys,
        core::ptr::without_provenance_mut(0xBEEF),
    );
    assert_eq!(v1_desc.validate(), FfiResult::OK);

    // SAFETY: `v1_desc` is a valid initialized `SystemDescriptor` on the stack.
    let promoted = unsafe {
        read_descriptor::<ScheduledSystemDescriptor>((&raw const v1_desc).cast::<c_void>())
    }
    .expect("v0.1 SystemDescriptor must zero-extend cleanly");

    assert_eq!(
        promoted.base.struct_size,
        core::mem::size_of::<SystemDescriptor>() as u32
    );
    assert_eq!(promoted.schedule, ScheduleLabelId::UPDATE);
    assert_eq!(promoted.flags, 0);
    assert!(promoted.after.is_null());
    assert_eq!(promoted.after_count, 0);
    assert!(promoted.before.is_null());
    assert_eq!(promoted.before_count, 0);
    assert_eq!(promoted.base.user_data.addr(), 0xBEEF);
    assert_eq!(promoted.validate(), FfiResult::OK);

    // 2. Full ScheduledSystemDescriptor with Startup schedule and ordering arrays
    let after_names = [c"init_world".as_ptr()];
    let v2_desc = v1_desc
        .with_schedule(ScheduleLabelId::STARTUP)
        .with_after(after_names.as_ptr(), after_names.len());
    assert_eq!(v2_desc.validate(), FfiResult::OK);

    // Passing `as_base_ptr()` through `RegisterSystemFn`-compatible pointer round-trips all v2 fields
    // SAFETY: `v2_desc` is valid for all `size_of::<ScheduledSystemDescriptor>()` bytes.
    let roundtrip =
        unsafe { read_descriptor::<ScheduledSystemDescriptor>(v2_desc.as_base_ptr().cast()) }
            .expect("v2 descriptor must read cleanly");
    assert_eq!(roundtrip.schedule, ScheduleLabelId::STARTUP);
    assert_eq!(roundtrip.after_count, 1);
    assert_eq!(roundtrip.after, after_names.as_ptr());

    // 3. Forward-compatible future descriptor (larger than ScheduledSystemDescriptor)
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct FutureSystemDescriptor {
        v2: ScheduledSystemDescriptor,
        future_extra: [u64; 4],
    }
    let mut future_desc = FutureSystemDescriptor {
        v2: v2_desc,
        future_extra: [0xDEAD_BEEF; 4],
    };
    future_desc.v2.base.struct_size = core::mem::size_of::<FutureSystemDescriptor>() as u32;

    // SAFETY: `future_desc` is readable for `size_of::<FutureSystemDescriptor>()` bytes.
    let truncated_future = unsafe {
        read_descriptor::<ScheduledSystemDescriptor>((&raw const future_desc).cast::<c_void>())
    }
    .expect("future descriptor with larger struct_size must be accepted");
    assert_eq!(truncated_future.schedule, ScheduleLabelId::STARTUP);
    assert_eq!(truncated_future.after_count, 1);

    // 4. Truncated descriptor (< MIN_V1_SIZE) rejected
    let mut too_small = v1_desc;
    too_small.struct_size = 16;
    // SAFETY: `too_small` is readable for at least 4 bytes (`struct_size`).
    let err = unsafe {
        read_descriptor::<ScheduledSystemDescriptor>((&raw const too_small).cast::<c_void>())
    }
    .unwrap_err();
    assert_eq!(err, FfiResult::INVALID_VERSION);

    // 5. Foreign C struct with zeroed/NULL callback pointer rejected before assume_init()
    let mut raw_c_zeroed = [0u8; core::mem::size_of::<SystemDescriptor>()];
    let sz_bytes = (core::mem::size_of::<SystemDescriptor>() as u32).to_ne_bytes();
    raw_c_zeroed[..4].copy_from_slice(&sz_bytes);
    // SAFETY: `raw_c_zeroed` has `size_of::<SystemDescriptor>()` readable bytes with null callback.
    let null_cb_err =
        unsafe { read_descriptor::<ScheduledSystemDescriptor>(raw_c_zeroed.as_ptr().cast()) }
            .unwrap_err();
    assert_eq!(null_cb_err, FfiResult::NULL_POINTER);

    // 6. Null descriptor pointer rejected
    // SAFETY: Passing a null pointer exercises the explicit null-pointer guard.
    let null_desc_err =
        unsafe { read_descriptor::<ScheduledSystemDescriptor>(core::ptr::null()) }.unwrap_err();
    assert_eq!(null_desc_err, FfiResult::NULL_POINTER);
}

#[test]
fn test_host_api_raw_header_inspection_on_legacy_buffer() {
    #[repr(C)]
    struct Legacy64Buffer {
        struct_size: u32,
        abi_version: u32,
        ptrs: [usize; 7],
    }
    let legacy = Legacy64Buffer {
        struct_size: core::mem::size_of::<Legacy64Buffer>() as u32,
        abi_version: ABI_VERSION_PACKED,
        ptrs: [1; 7],
    };
    let api_ptr = (&raw const legacy).cast::<HostApi>();
    // SAFETY: `legacy` is aligned and readable for 64 bytes (`>= 8` bytes for header fields).
    unsafe {
        assert_eq!(
            HostApi::raw_struct_size(api_ptr),
            Some(core::mem::size_of::<Legacy64Buffer>() as u32)
        );
        assert_eq!(HostApi::raw_abi_version(api_ptr), Some(ABI_VERSION_PACKED));
        assert!(!HostApi::has_register_component_raw(api_ptr));
        assert_eq!(HostApi::raw_struct_size(core::ptr::null()), None);
    }
}

#[test]
fn test_abi_pod_primitives_and_arrays() {
    fn assert_pod<T: AbiPod>() {}
    assert_pod::<u8>();
    assert_pod::<u16>();
    assert_pod::<u32>();
    assert_pod::<u64>();
    assert_pod::<u128>();
    assert_pod::<usize>();
    assert_pod::<i8>();
    assert_pod::<i16>();
    assert_pod::<i32>();
    assert_pod::<i64>();
    assert_pod::<i128>();
    assert_pod::<isize>();
    assert_pod::<f32>();
    assert_pod::<f64>();
    assert_pod::<FfiEntity>();
    assert_pod::<[f32; 4]>();
    assert_pod::<[[u32; 4]; 4]>();
}

#[test]
fn test_extended_access_modes_and_conditional_descriptors() {
    let all_modes = [
        ComponentAccessMode::READ,
        ComponentAccessMode::WRITE,
        ComponentAccessMode::FILTER_ONLY,
        ComponentAccessMode::WITHOUT,
        ComponentAccessMode::OPTIONAL_READ,
        ComponentAccessMode::OPTIONAL_WRITE,
        ComponentAccessMode::CHANGED,
        ComponentAccessMode::ADDED,
        ComponentAccessMode::RESOURCE_READ,
        ComponentAccessMode::RESOURCE_WRITE,
    ];
    for (idx, mode) in all_modes.iter().copied().enumerate() {
        assert_eq!(mode.0, idx as u32);
        assert!(mode.is_valid());
    }
    assert!(!ComponentAccessMode(10).is_valid());
    assert!(!ComponentAccessMode(u32::MAX).is_valid());

    assert!(ComponentAccessMode::OPTIONAL_READ.is_optional());
    assert!(ComponentAccessMode::OPTIONAL_WRITE.is_optional());
    assert!(!ComponentAccessMode::READ.is_optional());

    assert!(ComponentAccessMode::WRITE.is_mutable());
    assert!(ComponentAccessMode::OPTIONAL_WRITE.is_mutable());
    assert!(ComponentAccessMode::RESOURCE_WRITE.is_mutable());
    assert!(!ComponentAccessMode::OPTIONAL_READ.is_mutable());
    assert!(!ComponentAccessMode::CHANGED.is_mutable());
    assert!(!ComponentAccessMode::ADDED.is_mutable());
    assert!(!ComponentAccessMode::RESOURCE_READ.is_mutable());

    assert!(ComponentAccessMode::CHANGED.is_tick_filter());
    assert!(ComponentAccessMode::ADDED.is_tick_filter());
    assert!(!ComponentAccessMode::FILTER_ONLY.is_tick_filter());

    assert!(ComponentAccessMode::RESOURCE_READ.is_resource());
    assert!(ComponentAccessMode::RESOURCE_WRITE.is_resource());
    assert!(!ComponentAccessMode::READ.is_resource());

    assert_eq!(
        ComponentAccessDesc::optional_read(c"A".as_ptr()).mode,
        ComponentAccessMode::OPTIONAL_READ
    );
    assert_eq!(
        ComponentAccessDesc::optional_write(c"A".as_ptr()).mode,
        ComponentAccessMode::OPTIONAL_WRITE
    );
    assert_eq!(
        ComponentAccessDesc::changed(c"A".as_ptr()).mode,
        ComponentAccessMode::CHANGED
    );
    assert_eq!(
        ComponentAccessDesc::added(c"A".as_ptr()).mode,
        ComponentAccessMode::ADDED
    );
    assert_eq!(
        ComponentAccessDesc::resource_read(c"R".as_ptr()).mode,
        ComponentAccessMode::RESOURCE_READ
    );
    assert_eq!(
        ComponentAccessDesc::resource_write(c"R".as_ptr()).mode,
        ComponentAccessMode::RESOURCE_WRITE
    );

    unsafe extern "C" fn dummy_sys(
        _iter: *mut RawChunkIter,
        _cmds: *mut RawCommandQueue,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    unsafe extern "C" fn dummy_cond(_ud: *mut c_void) -> FfiBool {
        FFI_BOOL_TRUE
    }

    if core::mem::size_of::<usize>() == 8 {
        assert_eq!(core::mem::size_of::<SystemDescriptor>(), 48);
        assert_eq!(core::mem::size_of::<ScheduledSystemDescriptor>(), 88);
        assert_eq!(core::mem::size_of::<ConditionalSystemDescriptor>(), 104);
        assert_eq!(
            core::mem::size_of::<ConditionalMultiQuerySystemDescriptor>(),
            104
        );
        assert_eq!(core::mem::size_of::<HostApi>(), 72);
    }

    // v1 (48B) -> v3 (104B) zero-extension
    let v1 = SystemDescriptor::new(
        c"sys".as_ptr(),
        core::ptr::null(),
        0,
        dummy_sys,
        core::ptr::null_mut(),
    );
    // SAFETY: `v1` is valid and readable for `v1.struct_size` bytes.
    let from_v1 =
        unsafe { read_descriptor::<ConditionalSystemDescriptor>((&raw const v1).cast::<c_void>()) }
            .unwrap();
    assert_eq!(from_v1.sched.schedule, ScheduleLabelId::UPDATE);
    assert!(from_v1.run_if.is_none());
    assert!(from_v1.run_if_user_data.is_null());

    // v2 (88B) -> v3 (104B) zero-extension
    let v2 = v1.with_schedule(ScheduleLabelId::FIXED_UPDATE);
    // SAFETY: `v2` is valid and readable for `v2.base.struct_size` bytes.
    let from_v2 =
        unsafe { read_descriptor::<ConditionalSystemDescriptor>((&raw const v2).cast::<c_void>()) }
            .unwrap();
    assert_eq!(from_v2.sched.schedule, ScheduleLabelId::FIXED_UPDATE);
    assert!(from_v2.run_if.is_none());
    assert!(from_v2.run_if_user_data.is_null());

    // v3 (104B) round-trip
    let tag = core::ptr::without_provenance_mut(0x1234);
    let v3 = v2.with_run_if(dummy_cond, tag);
    // SAFETY: `v3` is valid and readable for `v3.sched.base.struct_size` bytes.
    let from_v3 =
        unsafe { read_descriptor::<ConditionalSystemDescriptor>((&raw const v3).cast::<c_void>()) }
            .unwrap();
    assert_eq!(from_v3.sched.schedule, ScheduleLabelId::FIXED_UPDATE);
    assert!(from_v3.run_if.is_some());
    assert_eq!(from_v3.run_if_user_data, tag);
    assert_eq!(from_v3.validate(), FfiResult::OK);
}
