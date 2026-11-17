//! Cross-language C99 shared-library VTable mutation interop test (`M-04`, `M-05`, `C-04`).

use bevy_abi_mod_host::{AbiModPlugin, LoadOptions, ModAppExt, ModComponent, ModRegistry};
use bevy_abi_mod_host_sys::{LoadContext, host_get_proc_address, host_load_host_ctx};
use bevy_abi_mod_sys::{BamGetProcAddressFn, BamHostCtx, FfiResult};
use bevy_app::App;
use core::ffi::c_void;
use libloading::{Library, Symbol};

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "c_interop::Vec3")]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, PartialEq, ModComponent)]
#[mod_component(name = "c_interop::CInteropComponent")]
pub struct CInteropComponent {
    pub health: i32,
    pub pos: Vec3,
    pub label: String,
}

type CModTestVTableMutationFn = unsafe extern "C" fn(
    get_proc_address: BamGetProcAddressFn,
    host_ctx: *const BamHostCtx,
    column_base_ptr: *mut c_void,
    count: usize,
) -> FfiResult;

#[test]
fn test_c99_shared_library_mutates_rust_components_via_vtable() {
    let orig_dylib = crate::support::fixture_dylib("asymmetric_c_mod");
    let isolated_dylib = crate::support::TempDylibCopy::new(&orig_dylib, "c_vtable_interop");

    // 1. Register `Vec3` and `CInteropComponent` VTables on the host and verify C99 mod load handshake.
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_type::<Vec3>();
    app.register_mod_type::<CInteropComponent>();

    let mod_id = app
        .load_mod_with(
            isolated_dylib.path(),
            LoadOptions::new().with_shadow_copy(true),
        )
        .expect("ModAppExt::load_mod_with failed to load C99 mod");

    // 2. Invoke `bevy_c_mod_test_vtable_mutation` exported by the C99 shared library using
    //    the genuine context-carrying `host_get_proc_address` + `BamHostCtx`.
    let mut column: Vec<CInteropComponent> = (0..16)
        .map(|i| CInteropComponent {
            health: i * 10,
            pos: Vec3 {
                x: i as f32,
                y: (i * 2) as f32,
                z: (i * 3) as f32,
            },
            label: format!("rust_entity_{i}"),
        })
        .collect();

    let mut load_ctx = LoadContext::new(app.world().resource::<ModRegistry>().clone());
    // SAFETY: Loading the isolated C99 shared library copy and invoking its exported VTable test entry point.
    unsafe {
        let host_ctx = host_load_host_ctx(load_ctx.as_ffi_ptr());
        assert!(!host_ctx.is_null());

        let lib = Library::new(isolated_dylib.path()).expect("failed to open C99 shared library");
        let test_fn: Symbol<CModTestVTableMutationFn> = lib
            .get(b"bevy_c_mod_test_vtable_mutation\0")
            .expect("missing bevy_c_mod_test_vtable_mutation symbol");

        let res = test_fn(
            host_get_proc_address,
            host_ctx,
            column.as_mut_ptr().cast::<c_void>(),
            column.len(),
        );
        assert_eq!(res, FfiResult::OK);
    }
    load_ctx.finish_init();

    for (i, item) in column.iter().enumerate() {
        assert_eq!(item.health, (i as i32) * 10 + 1000);
        assert_eq!(item.pos.x, (i as f32) + 10.5);
        assert_eq!(item.pos.y, (i as f32) * 2.0 + 20.5);
        assert_eq!(item.pos.z, (i as f32) * 3.0 + 30.5);
        assert_eq!(item.label, "mutated_from_c99");
    }

    app.unload_mod(mod_id).expect("unload C99 mod");
}
