#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

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

mod common;

use abi_contract::{
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
use common::{NameTag, Position, Velocity};
use core::ffi::{c_char, c_void};
use core::sync::atomic::AtomicBool;
use host_runtime::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, host_get_proc_address,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
    host_iter_entities, host_iter_mark_changed, host_iter_next_chunk, host_iter_reset,
    host_register_component, host_register_multi_query_system, host_register_scheduled_system,
    host_register_system, host_register_system_in_schedule, AbiModPlugin, CommandQueueBridge,
    HostChunkIter, ModAppExt, ModInitContext, ModRegistry, QueryColumnDesc, MAX_QUERY_COMPONENTS,
    MAX_SYSTEM_QUERIES,
};
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
#[derive(Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq)]
pub struct FuzzBoolComp {
    pub flag: bool,
    pub code: u32,
}

#[repr(C)]
#[derive(Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq)]
#[component(storage = "SparseSet")]
pub struct FuzzSparseVal {
    pub v: u64,
}

#[repr(C)]
#[derive(Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq)]
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
    let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

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
            let (_buf, raw_ptr) = make_sized_descriptor_buffer(&valid_base, sz, misalign);
            let min_v1 = SystemDescriptor::MIN_V1_SIZE as u32;

            // SAFETY: `raw_ptr` has exact readable prefix length for `read_descriptor`.
            let r_legacy =
                unsafe { host_register_system(ctx_ptr, raw_ptr.cast::<SystemDescriptor>()) };
            // SAFETY: Same valid buffer passed to scheduled entry point.
            let r_sched = unsafe {
                host_register_scheduled_system(ctx_ptr, raw_ptr.cast::<ScheduledSystemDescriptor>())
            };

            if sz < min_v1 {
                assert_eq!(
                    r_legacy,
                    FfiResult::INTERNAL_ERROR,
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
            host_register_system(ctx_ptr, zero_cb_bytes.as_ptr().cast::<SystemDescriptor>()),
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

    // Randomized fuzz loop over malformed combinations
    let mut rng = Rng::new(0xC0FF_EE01_2026_1111);
    let valid_comp_names = [c"Position", c"Velocity", c"NameTag", c"FuzzSparseVal"];

    for step in 0..FUZZ_ITERS {
        init_ctx.registered_systems.clear();
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
                assert_eq!(res, FfiResult::INVALID_COMPONENT, "step {step}");
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
                // Invalid `ComponentAccessMode(4..=u32::MAX)` inside query array
                let q_len = 1 + rng.range(MAX_QUERY_COMPONENTS);
                let bad_idx = rng.range(q_len);
                let bad_mode = 4u32.saturating_add(rng.next_u32()).max(4);
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
                assert_eq!(res, FfiResult::INVALID_COMPONENT, "step {step}");
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
                let res_in_sched =
                    unsafe { host_register_system_in_schedule(ctx_ptr, &legacy_desc, bad_sched) };
                assert_eq!(res_in_sched, FfiResult::INTERNAL_ERROR, "step {step}");
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
                let (_buf, raw_ptr) = make_sized_descriptor_buffer(&valid_base, sz, rng.range(4));
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
    let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

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
        let (_buf, raw_ptr) = make_sized_descriptor_buffer(&valid_base, sz, 0);
        let min_v1 = MultiQuerySystemDescriptor::MIN_V1_SIZE as u32;
        // SAFETY: `_buf` is readable for `read_descriptor`.
        let res = unsafe {
            host_register_multi_query_system(ctx_ptr, raw_ptr.cast::<MultiQuerySystemDescriptor>())
        };
        if sz < min_v1 {
            assert_eq!(res, FfiResult::INTERNAL_ERROR, "sz={sz}");
        } else {
            assert_eq!(res, FfiResult::OK, "sz={sz}");
        }
    }

    // Null & zero-callback checks
    // SAFETY: Testing explicit null guards and zeroed callback slot.
    unsafe {
        assert_eq!(
            host_register_multi_query_system(core::ptr::null_mut(), core::ptr::null()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_register_multi_query_system(ctx_ptr, core::ptr::null()),
            FfiResult::NULL_POINTER
        );

        let mut zero_cb = vec![0u8; core::mem::size_of::<ScheduledMultiQuerySystemDescriptor>()];
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

    let mut rng = Rng::new(0xBADC_0FFE_E002_2222);
    for step in 0..FUZZ_ITERS {
        init_ctx.registered_systems.clear();
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
                let res = unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                assert_eq!(res, FfiResult::INTERNAL_ERROR, "step {step}");
            }
            1 => {
                // Valid `query_spec_count` (`1..=8`) with null `query_specs`
                let mut desc = valid_base;
                desc.base.query_specs = core::ptr::null();
                desc.base.query_spec_count = 1 + rng.range(MAX_SYSTEM_QUERIES);
                // SAFETY: Null `query_specs` check precedes slice read.
                let res = unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
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
                let res = unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                assert_eq!(res, FfiResult::INVALID_COMPONENT, "step {step}");
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
                let res = unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                assert_eq!(res, FfiResult::NULL_POINTER, "step {step}");
            }
            4 => {
                // Invalid `ComponentAccessMode(4..=u32::MAX)` in one of the specs
                let bad_mode = (4 + (rng.next_u32() % (u32::MAX - 4))).max(4);
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
                let res = unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                assert_eq!(res, FfiResult::INVALID_COMPONENT, "step {step}");
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
                let res = unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                assert_eq!(res, FfiResult::INVALID_UTF8, "step {step}");
            }
            6 => {
                // Invalid `ScheduleLabelId` (`6..=u32::MAX`)
                let mut desc = valid_base;
                desc.schedule = ScheduleLabelId(6 + (rng.next_u32() % 50_000));
                // SAFETY: Valid descriptor with invalid schedule ID.
                let res = unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
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
                let res = unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
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
                let res = unsafe { host_register_multi_query_system(ctx_ptr, desc.as_base_ptr()) };
                assert_eq!(res, FfiResult::OK, "step {step}");
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

        // Active context has null `world` -> INTERNAL_ERROR
        let _guard = dummy_ctx.make_active();
        assert_eq!(
            host_register_component(dummy_ptr, &template_desc),
            FfiResult::INTERNAL_ERROR
        );
        // Passing a forged/different non-null `ctx` while `dummy_ctx` is active -> INTERNAL_ERROR
        let mut other_ctx = ModInitContext::new(Arc::clone(&registry));
        assert_eq!(
            host_register_component(
                (&mut other_ctx as *mut ModInitContext).cast::<c_void>(),
                &template_desc
            ),
            FfiResult::INTERNAL_ERROR
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
                // Non-power-of-two or zero alignment -> INTERNAL_ERROR
                let align = non_pow2_aligns[rng.range(non_pow2_aligns.len())];
                let size = rng.range(128);
                let desc = ModComponentDescriptor::new(name_cstr.as_ptr(), uuid, size, align);
                // SAFETY: Valid descriptor with invalid alignment.
                assert_eq!(
                    unsafe { host_register_component(ctx_ptr, &desc) },
                    FfiResult::INTERNAL_ERROR,
                    "step {step}: align={align} size={size}"
                );
            }
            3 => {
                // Valid power-of-two `align >= 2`, but `size % align != 0` -> INTERNAL_ERROR
                let shift = 1 + rng.range(10); // align in 2..=1024
                let align = 1usize << shift;
                let rem = 1 + rng.range(align - 1);
                let size = rng.range(16) * align + rem;
                let desc = ModComponentDescriptor::new(name_cstr.as_ptr(), uuid, size, align);
                // SAFETY: Valid descriptor with unpadded size (`size % align != 0`).
                assert_eq!(
                    unsafe { host_register_component(ctx_ptr, &desc) },
                    FfiResult::INTERNAL_ERROR,
                    "step {step}: size={size} align={align}"
                );
            }
            4 => {
                // `usize::MAX` or `> isize::MAX` overflow sizes -> INTERNAL_ERROR
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
                    FfiResult::INTERNAL_ERROR,
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
                        assert_eq!(res, FfiResult::INTERNAL_ERROR, "step {step}");
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
                host_register_component((&mut ctx as *mut ModInitContext).cast::<c_void>(), &desc)
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
                        || Entity::try_from_bits(bad_entity.to_bits()) == Some(Entity::PLACEHOLDER)
                    {
                        assert_eq!(res, FfiResult::INTERNAL_ERROR, "step {step}");
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
                    assert_eq!(res, FfiResult::INTERNAL_ERROR, "step {step}");
                }
                8 => {
                    // Invalid entity (`NULL` or `PLACEHOLDER`) with otherwise valid component & size -> INTERNAL_ERROR
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
                    assert_eq!(res, FfiResult::INTERNAL_ERROR, "step {step}");
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
                            let sym = unsafe { host_get_proc_address(c"Velocity_vtable".as_ptr()) };
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
                            assert!(unsafe {
                                host_get_proc_address(bad_sym.as_ptr().cast::<c_char>())
                            }
                            .is_null());
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
