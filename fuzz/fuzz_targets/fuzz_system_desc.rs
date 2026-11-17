#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

//! Fuzz Target 2: `BamSystemDesc` & `BamQueryDesc` validation + `host_register_system`
//! host parser across arbitrary query counts, term counts, access modes, schedules, flags,
//! resource/component kind mismatches, self-conflicts, and poisoned `LoadContext` states.

use bevy_abi_mod_host::{ModComponent, ModRegistry, ModWorldExt};
use bevy_abi_mod_host_sys::{
    LoadContext, host_load_host_ctx, host_register_component, host_register_system,
};
use bevy_abi_mod_sys::{
    AccessMode, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET, BamComponent,
    BamQueryDesc, BamSystemCtx, BamSystemDesc, BamTermDesc, ComponentDesc, ComponentUuid, FfiBool,
    FfiResult, ScheduleId,
};
use bevy_ecs::resource::Resource;
use bevy_ecs::world::World;
use core::ffi::{c_char, c_void};
use core::sync::atomic::AtomicBool;
use libfuzzer_sys::fuzz_target;
use std::hint::black_box;
use std::sync::Arc;

#[repr(C)]
#[derive(Resource, Clone, Copy, Default, ModComponent)]
#[mod_component(name = "fuzz::Res")]
struct FuzzRes {
    ticks: u64,
}

unsafe extern "C" fn noop_sys_cb(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_run_if(_ud: *mut c_void) -> FfiBool {
    FfiBool::TRUE
}

fn cstr_from_slice(bytes: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = bytes
        .iter()
        .copied()
        .take(48)
        .map(|b| if b == 0 { b'_' } else { b })
        .collect();
    out.push(0);
    out
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 12 {
        return;
    }

    let mut world = World::new();
    world.init_resource::<FuzzRes>();
    let _ = world.register_mod_resource::<FuzzRes>();
    let registry = world.resource::<ModRegistry>().clone();

    let mut ctx = LoadContext::with_manifest_name(
        registry,
        Arc::new(AtomicBool::new(true)),
        &mut world as *mut World,
        None,
        Some(Arc::from("fuzz_mod")),
    );
    let ctx_ptr = ctx.as_ffi_ptr();

    // Register 4 diverse dynamic components (dense mutable, sparse mutable, dense immutable, ZST)
    let specs: &[(&[u8], usize, usize, u32)] = &[
        (b"DenseComp\0", 12, 4, 0),
        (b"SparseComp\0", 8, 4, BAM_COMPONENT_FLAG_SPARSE_SET),
        (b"ImmutComp\0", 4, 4, BAM_COMPONENT_FLAG_IMMUTABLE),
        (b"MarkerZst\0", 0, 1, 0),
    ];
    for (idx, &(cname, size, align, flags)) in specs.iter().enumerate() {
        let mut desc = ComponentDesc::new(
            cname.as_ptr().cast::<c_char>(),
            ComponentUuid::from_parts("fuzz_mod::c", size, align, idx as u64),
            size,
            align,
        );
        desc.flags = flags;
        let mut out_h = BamComponent::INVALID;
        // SAFETY: Valid active `ctx_ptr`, NUL-terminated `cname`, writable `out_h`.
        let _ = unsafe { host_register_component(ctx_ptr, &desc, &mut out_h) };
    }

    let flags = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let schedule_raw = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let reserved0 = if data[8] & 0x80 != 0 {
        data[8] as u32
    } else {
        0
    };
    let use_null_cb = (data[9] & 1) != 0;
    let use_run_if = (data[9] & 2) != 0;
    let poison_early = (data[9] & 4) != 0;
    let q_count = ((data[10] as usize) % 10).min((data.len() - 12) / 3 + 1);

    let name_buf = cstr_from_slice(&data[11..data.len().min(27)]);

    let mut cursor = 12usize;
    let mut term_storage: Vec<Vec<BamTermDesc>> = Vec::with_capacity(q_count);
    let mut query_descs: Vec<BamQueryDesc> = Vec::with_capacity(q_count);

    for _ in 0..q_count {
        let t_count = if cursor < data.len() {
            let n = (data[cursor] as usize) % 36;
            cursor += 1;
            n
        } else {
            0
        };
        let q_flags = if cursor < data.len() && (data[cursor] & 0x40 != 0) {
            let f = data[cursor] as u32;
            cursor += 1;
            f
        } else {
            0
        };

        let mut terms = Vec::with_capacity(t_count);
        for _ in 0..t_count {
            let comp_raw = if cursor < data.len() {
                let c = (data[cursor] % 8) as u32;
                cursor += 1;
                c
            } else {
                1
            };
            let mode_raw = if cursor < data.len() {
                let m = (data[cursor] % 13) as u32;
                cursor += 1;
                m
            } else {
                0
            };
            let t_flags = if cursor < data.len() && (data[cursor] & 0x80 != 0) {
                let tf = data[cursor] as u32;
                cursor += 1;
                tf
            } else {
                0
            };
            terms.push(BamTermDesc {
                component: BamComponent::from_raw(comp_raw),
                mode: AccessMode(mode_raw),
                flags: t_flags,
            });
        }
        term_storage.push(terms);
        let last = term_storage.last().unwrap();
        let mut qd = BamQueryDesc::new(
            if last.is_empty() {
                core::ptr::null()
            } else {
                last.as_ptr()
            },
            last.len(),
        );
        qd.flags = q_flags;
        query_descs.push(qd);
    }

    let sys_desc = BamSystemDesc {
        struct_size: core::mem::size_of::<BamSystemDesc>() as u32,
        flags,
        name: name_buf.as_ptr().cast::<c_char>(),
        queries: if query_descs.is_empty() {
            core::ptr::null()
        } else {
            query_descs.as_ptr()
        },
        query_count: query_descs.len(),
        callback: if use_null_cb { None } else { Some(noop_sys_cb) },
        user_data: core::ptr::null_mut(),
        schedule: ScheduleId(schedule_raw),
        reserved0,
        after: core::ptr::null(),
        after_count: 0,
        before: core::ptr::null(),
        before_count: 0,
        run_if: if use_run_if { Some(noop_run_if) } else { None },
        run_if_user_data: core::ptr::null_mut(),
        in_sets: core::ptr::null(),
        in_set_count: 0,
    };

    if poison_early {
        ctx.finish_init();
    }

    // SAFETY: `ctx_ptr` points to our stack `LoadContext` (active or intentionally finished),
    // and `sys_desc` references valid stack-backed C-strings, `query_descs`, and `term_storage`.
    let status = unsafe { host_register_system(ctx_ptr, &sys_desc) };
    let _ = black_box(status);

    // Also exercise `host_load_host_ctx` under both active and finished states.
    // SAFETY: `ctx_ptr` is non-null and aligned.
    let hctx = unsafe { host_load_host_ctx(ctx_ptr.cast_const()) };
    let _ = black_box(hctx);
});
