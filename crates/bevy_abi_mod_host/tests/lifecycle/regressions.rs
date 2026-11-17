//! Lifecycle & safety regressions (`S-05`, `S-06`, `S-07`, `L-02`, `G-05`).

use crate::DROP_SLOT_POOL_RWLOCK;
use crate::support::{Position, TempDylibCopy, Velocity, ensure_guest_mods_built, fixture_dylib};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::drop_trampoline::available_drop_trampoline_slots;
use bevy_abi_mod_host_sys::{
    HOST_API, HandleHeader, LOAD_CTX_MAGIC, LoadContext, host_get_proc_address, host_load_host_ctx,
    host_register_component, host_register_system, host_resolve_component,
};
use bevy_abi_mod_sys::{BamHostCtx, BamLoadCtx, BamSystemCtx};
use bevy_app::App;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::AtomicBool;
use libloading::{Library, Symbol};
use std::io::{Read as _, Seek as _, Write as _};
use std::sync::Arc;

unsafe extern "C" fn noop_sys_cb(_ctx: *mut BamSystemCtx, _user_data: *mut c_void) -> FfiResult {
    FfiResult::OK
}

// ============================================================================
// S-05: Clear guest VTables & symbols on unload with zero global symbol state
// ============================================================================

#[test]
fn regression_s05_stale_vtable_after_unload() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let dylib_path = fixture_dylib("s05_vtable_mod");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let mod_id = app
        .load_mod_with(&dylib_path, LoadOptions::new().with_shadow_copy(true))
        .expect("load s05_vtable_mod succeeds");

    let sentinel_word: u64 = 0xABCD;
    // SAFETY: `sentinel_word` outlives `mod_id`, which is unloaded before `sentinel_word` drops.
    unsafe {
        app.world_mut()
            .resource_mut::<ModRegistry>()
            .register_mod_symbol(
                "s05_custom_fn",
                (&sentinel_word as *const u64).cast::<c_void>(),
                mod_id,
            );
    }

    let comp_id_before = {
        let reg = app.world().resource::<ModRegistry>();
        let comp = reg
            .snapshot()
            .get_by_name("s05_vtable_mod::S05GuestComp")
            .expect("s05_vtable_mod::S05GuestComp registered");
        assert!(!comp.vtable_ptr.is_null());
        assert!(comp.drop_fn.is_some());
        assert_eq!(comp.owner, Some(mod_id));
        assert!(
            reg.snapshot()
                .resolve_symbol("bam.v0.s05_vtable_mod::S05GuestComp.vtable")
                .is_some()
        );
        assert!(
            reg.snapshot()
                .resolve_symbol("bam.v0.s05_custom_fn")
                .is_some()
        );

        // Separate empty world registry has zero access to `app`'s symbols (`C-04` per-world isolation).
        let mut other_load = LoadContext::new(ModRegistry::new());
        // SAFETY: Valid active `other_load`.
        let other_hctx = unsafe { host_load_host_ctx(other_load.as_ffi_ptr()) };
        // SAFETY: Valid `other_hctx` and CStr.
        let isolated_lookup = unsafe {
            host_get_proc_address(
                other_hctx,
                c"bam.v0.s05_vtable_mod::S05GuestComp.vtable".as_ptr(),
            )
        };
        assert!(
            isolated_lookup.is_null(),
            "Guest VTable pointer must never leak across worlds"
        );
        other_load.finish_init();

        // Within `app`'s own `ModRegistry`, context-scoped `host_get_proc_address` resolves while loaded.
        let mut active_load = LoadContext::new(reg.clone());
        // SAFETY: Valid active `active_load`.
        let active_hctx = unsafe { host_load_host_ctx(active_load.as_ffi_ptr()) };
        // SAFETY: Valid `active_hctx` and CStr.
        let active_sym = unsafe {
            host_get_proc_address(
                active_hctx,
                c"bam.v0.s05_vtable_mod::S05GuestComp.vtable".as_ptr(),
            )
        };
        assert!(!active_sym.is_null());
        active_load.finish_init();

        comp.component_id
    };

    // Unload the mod (`dlclose`).
    app.unload_mod(mod_id)
        .expect("unload s05_vtable_mod succeeds");

    {
        let reg = app.world().resource::<ModRegistry>();
        let comp = reg
            .snapshot()
            .get_by_name("s05_vtable_mod::S05GuestComp")
            .expect("ComponentId binding preserved across unload for hot-reload");
        assert_eq!(comp.component_id, comp_id_before);
        assert!(
            comp.vtable_ptr.is_null(),
            "vtable_ptr must be nulled on unload"
        );
        assert!(comp.drop_fn.is_none(), "drop_fn must be cleared on unload");
        assert!(comp.owner.is_none(), "owner must be cleared on unload");
        assert!(
            reg.snapshot()
                .resolve_symbol("bam.v0.s05_vtable_mod::S05GuestComp.vtable")
                .is_none(),
            "guest VTable symbol must be evicted on unload"
        );
        assert!(
            reg.snapshot()
                .resolve_symbol("bam.v0.s05_custom_fn")
                .is_none(),
            "mod-owned custom symbol must be evicted on unload"
        );
    }
}

// ============================================================================
// S-06: Failed init / strict validation rolls back before dlclose
// ============================================================================

#[test]
fn regression_s06_init_failure_rolls_back() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let dylib_fail = fixture_dylib("s06_fail_init");
    let dylib_strict = fixture_dylib("s06_strict_fail");

    let baseline_slots = available_drop_trampoline_slots();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let err = app
        .load_mod_with(&dylib_fail, LoadOptions::new().with_shadow_copy(true))
        .expect_err("init returning INVALID_ARGUMENT must fail");
    assert!(matches!(
        err,
        ModLoadError::InitFailed {
            status: FfiResult::INVALID_ARGUMENT,
            ..
        }
    ));

    let reg = app.world().resource::<ModRegistry>();
    assert!(
        reg.snapshot()
            .get_by_name("s06_fail_init::S06RollbackComp")
            .is_none(),
        "LoadTransaction must roll back newly registered dynamic component before dlclose"
    );
    assert!(
        reg.snapshot()
            .resolve_symbol("bam.v0.s06_fail_init::S06RollbackComp.vtable")
            .is_none()
    );
    assert!(app.world().resource::<ModHost>().is_empty());
    assert_eq!(
        available_drop_trampoline_slots(),
        baseline_slots,
        "Rolled-back init must reclaim allocated drop trampoline slot immediately"
    );

    // Post-init strict validation failure (`UnresolvedOrderingTarget`) also rolls back cleanly.
    let strict_err = app
        .load_mod_with(
            &dylib_strict,
            LoadOptions::new().strict().with_shadow_copy(true),
        )
        .expect_err("strict load must reject unresolved ordering target");
    assert!(matches!(
        strict_err,
        ModLoadError::UnresolvedOrderingTarget { .. }
    ));

    let reg_after = app.world().resource::<ModRegistry>();
    assert!(
        reg_after
            .snapshot()
            .get_by_name("s06_strict_fail::S06StrictTempComp")
            .is_none()
    );
    assert!(
        reg_after
            .snapshot()
            .resolve_symbol("bam.v0.s06_strict_fail::S06StrictTempComp.vtable")
            .is_none()
    );
    assert!(app.world().resource::<ModHost>().is_empty());
}

// ============================================================================
// S-07 & S-08: LoadContext magic + generation + active-scope validation
// ============================================================================

#[test]
fn regression_s07_register_outside_init_rejected() {
    let mut world = World::new();
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));

    let comp_desc = ComponentDesc::new(
        c"s07::Dyn".as_ptr(),
        ComponentUuid::from_parts("s07::Dyn", 4, 4, 0),
        4,
        4,
    );
    let sys_desc = BamSystemDesc::new(
        c"s07_sys".as_ptr(),
        core::ptr::null(),
        0,
        noop_sys_cb,
        core::ptr::null_mut(),
    );

    let reg_comp_api = HOST_API.register_component.unwrap();
    let reg_sys_api = HOST_API.register_system.unwrap();

    // 1. Null `BamLoadCtx*` -> `NULL_POINTER`
    let mut out_comp = BamComponent::INVALID;
    // SAFETY: Testing null context rejection across registration entry points.
    unsafe {
        assert_eq!(
            reg_comp_api(core::ptr::null_mut(), &comp_desc, &mut out_comp),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            reg_sys_api(core::ptr::null_mut(), &sys_desc),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_register_component(core::ptr::null_mut(), &comp_desc, &mut out_comp),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_register_system(core::ptr::null_mut(), &sys_desc),
            FfiResult::NULL_POINTER
        );
    }

    // 2. Stale pointer stashed across `finish_init()` -> `INVALID_STATE`
    {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            None,
        );
        let load_ptr: *mut BamLoadCtx = ctx.as_ffi_ptr();
        // SAFETY: Valid active `ctx`.
        let host_ptr: *const BamHostCtx = unsafe { host_load_host_ctx(load_ptr) };
        assert!(!host_ptr.is_null());

        // SAFETY: Valid active `load_ptr` during init.
        let reg_sys_res = unsafe { host_register_system(load_ptr, &sys_desc) };
        assert_eq!(reg_sys_res, FfiResult::OK);

        ctx.finish_init();

        // SAFETY: `ctx` stack allocation is still live, with poisoned header (`in_init == false`).
        unsafe {
            assert!(host_load_host_ctx(load_ptr).is_null());
            assert_eq!(
                host_register_component(load_ptr, &comp_desc, &mut out_comp),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_register_system(load_ptr, &sys_desc),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_resolve_component(
                    host_ptr,
                    c"s07::Dyn".as_ptr(),
                    core::ptr::null(),
                    &mut out_comp,
                ),
                FfiResult::INVALID_STATE
            );
        }
    }

    // 3. Forged header magic / zero generation -> `INVALID_STATE`
    #[repr(C, align(8))]
    struct ForgedCtx {
        header: HandleHeader,
        pad: [u8; 256],
    }
    let mut forged = ForgedCtx {
        header: HandleHeader::with_generation(LOAD_CTX_MAGIC ^ 0xFFFF, 1),
        pad: [0u8; 256],
    };
    let forged_ptr = (&mut forged as *mut ForgedCtx).cast::<BamLoadCtx>();
    // SAFETY: `forged_ptr` is aligned and readable for `HandleHeader` check, which rejects magic mismatch.
    unsafe {
        assert_eq!(
            host_register_system(forged_ptr, &sys_desc),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_register_component(forged_ptr, &comp_desc, &mut out_comp),
            FfiResult::INVALID_STATE
        );
    }
}

// ============================================================================
// L-02: Per-World `BamInstance` isolation across simultaneous multi-World loads
// ============================================================================

#[test]
fn regression_l02_multi_world_per_instance_isolation() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app_a = App::new();
    app_a.add_plugins(AbiModPlugin);
    app_a
        .register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app_a
        .register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

    let mut app_b = App::new();
    app_b.add_plugins(AbiModPlugin);
    app_b
        .register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app_b
        .register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

    let ent_a = app_a
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 0.0,
            },
        ))
        .id();
    let ent_b = app_b
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 3.0,
                y: 4.0,
                z: 0.0,
            },
        ))
        .id();

    // Load the exact same shared library files (without shadow copy!) into TWO separate Bevy Worlds.
    let no_shadow = LoadOptions::new().with_shadow_copy(false);
    let r_a = app_a
        .load_mod_with(&rust_dylib, no_shadow.clone())
        .expect("load rust into app_a");
    let c_a = app_a
        .load_mod_with(&c_dylib, no_shadow.clone())
        .expect("load c into app_a");
    let r_b = app_b
        .load_mod_with(&rust_dylib, no_shadow.clone())
        .expect("load rust into app_b");
    let c_b = app_b
        .load_mod_with(&c_dylib, no_shadow)
        .expect("load c into app_b");

    for _ in 0..5 {
        app_a.update();
        app_b.update();
    }

    assert_eq!(
        *app_a.world().get::<Position>(ent_a).unwrap(),
        Position {
            x: 5.0,
            y: 10.0,
            z: 5.0
        }
    );
    assert_eq!(
        *app_b.world().get::<Position>(ent_b).unwrap(),
        Position {
            x: 15.0,
            y: 20.0,
            z: 5.0
        }
    );

    // Unloading BOTH mods from `app_a` runs `bevy_abi_mod_shutdown(instance_a)` ONLY for `app_a`;
    // `app_b`'s per-World `BamInstance` closures, handles, and VTables MUST continue working!
    app_a.unload_mod(r_a).expect("unload rust from app_a");
    app_a.unload_mod(c_a).expect("unload c from app_a");

    for _ in 0..5 {
        app_a.update();
        app_b.update();
    }

    // `app_a` stayed frozen while `app_b` advanced another 5 ticks cleanly!
    assert_eq!(
        *app_a.world().get::<Position>(ent_a).unwrap(),
        Position {
            x: 5.0,
            y: 10.0,
            z: 5.0
        }
    );
    assert_eq!(
        *app_b.world().get::<Position>(ent_b).unwrap(),
        Position {
            x: 30.0,
            y: 40.0,
            z: 10.0
        }
    );

    app_b.unload_mod(r_b).expect("unload rust from app_b");
    app_b.unload_mod(c_b).expect("unload c from app_b");
}

// ============================================================================
// G-05: `panic = "abort"` Rust guest fixture deterministically aborts subprocess
// ============================================================================

#[test]
#[cfg(not(miri))]
fn regression_g05_panic_abort_guest_terminates_subprocess() {
    let fixture_root = crate::support::fixture_dir();
    let _ = fixture_dylib("panic_abort_mod");

    rusty_fork::fork(
        "regressions::regression_g05_panic_abort_guest_terminates_subprocess",
        rusty_fork::rusty_fork_id!(),
        |cmd| {
            cmd.env("BAM_FIXTURE_DIR", &fixture_root);
        },
        |child, file| {
            let status = child.wait().expect("wait on G-05 forked child process");
            file.rewind().expect("rewind G-05 child captured output");
            let mut output = String::new();
            file.read_to_string(&mut output)
                .expect("read G-05 child captured output");

            assert!(
                output.contains("G05_ARMED_OK"),
                "child must load panic_abort_mod and complete 1 healthy tick before aborting; output={output}"
            );
            assert!(
                !status.success(),
                "panic = \"abort\" guest panic must terminate child process with non-zero status; status={status:?}"
            );

            #[cfg(unix)]
            {
                assert!(
                    status.unix_signal().is_some() || status.code() != Some(0),
                    "expected signal termination (SIGABRT/SIGILL) or non-zero exit on Unix, got {status:?}"
                );
            }
        },
        || {
            let abort_dylib = TempDylibCopy::new(&fixture_dylib("panic_abort_mod"), "g05_abort");
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);

            let _mod_id = app
                .load_mod_with(
                    abort_dylib.path(),
                    LoadOptions::new().with_shadow_copy(false),
                )
                .expect("panic_abort_mod loads cleanly");

            // 1. Run one healthy tick prior to arming panic.
            app.update();

            // SAFETY: Opening the same `TempDylibCopy` path to inspect healthy tick count and arm panic.
            unsafe {
                let lib = Library::new(abort_dylib.path()).expect("open panic_abort_mod");
                let ticks_fn: Symbol<unsafe extern "C" fn() -> u32> = lib
                    .get(b"panic_abort_mod_healthy_ticks\0")
                    .expect("resolve panic_abort_mod_healthy_ticks");
                assert_eq!(ticks_fn(), 1);

                let arm_fn: Symbol<unsafe extern "C" fn()> = lib
                    .get(b"panic_abort_mod_arm_panic\0")
                    .expect("resolve panic_abort_mod_arm_panic");
                arm_fn();
            }

            println!("G05_ARMED_OK");
            let _ = std::io::stdout().flush();

            // 2. Next tick triggers `panic!(...)` inside the `panic = "abort"` guest `.cdylib`,
            // terminating this child process immediately at the panic site without crossing `extern "C"`.
            app.update();

            unreachable!("process must have aborted inside panic=abort guest callback");
        },
    )
    .expect("rusty_fork G-05 child fork execution");
}
