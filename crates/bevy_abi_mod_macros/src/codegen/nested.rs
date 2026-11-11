use crate::codegen::CodegenState;
use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::{Field, Type};

pub(crate) fn emit_nested_field(
    state: &mut CodegenState,
    field: &Field,
    field_name: &str,
    member_tokens: &TokenStream,
    nested_ty: &Type,
    field_span: Span,
) -> syn::Result<()> {
    let ids = state.register_pair(
        field,
        field_name,
        "ptr",
        "ptr_mut",
        "interior-pointer accessor",
    )?;
    let ptr_export = state.export_prefix(&ids.fn_first_str);
    let ptr_mut_export = state.export_prefix(&ids.fn_second_str);

    let sys_crate = &state.sys_crate;
    let struct_ident = &state.struct_ident;
    let fn_ptr_ident = &ids.fn_first_ident;
    let fn_ptr_mut_ident = &ids.fn_second_ident;
    let vt_ptr_ident = &ids.vt_first_ident;
    let vt_ptr_mut_ident = &ids.vt_second_ident;

    if state.opaque {
        state.layout_assertions.push(quote_spanned! { field_span =>
            const _: fn() = || {
                fn assert_nested_exposable<T: #sys_crate::ExposableComponent>() {}
                assert_nested_exposable::<#nested_ty>();
            };
        });
    } else {
        state.layout_assertions.push(quote_spanned! { field_span =>
            const _: fn() = || {
                fn assert_nested_ffi<T: #sys_crate::FfiLayout>() {}
                assert_nested_ffi::<#nested_ty>();
            };
        });
    }

    state
        .nested_pod_assertions
        .push(quote_spanned! { field_span =>
            assert_nested_pod::<#nested_ty>();
        });

    state.extern_fns.push(quote! {
        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #ptr_export
        unsafe extern "C" fn #fn_ptr_ident(
            ptr: *const ::core::ffi::c_void,
        ) -> *const ::core::ffi::c_void {
            if ptr.is_null() {
                return ::core::ptr::null();
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to nested ptr getter"
            );
            // SAFETY: `typed_ptr` is verified non-null and points to a valid `#struct_ident`;
            // `addr_of!` computes the interior field address without creating a reference.
            unsafe {
                ::core::ptr::addr_of!((*typed_ptr).#member_tokens).cast::<::core::ffi::c_void>()
            }
        }

        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #ptr_mut_export
        unsafe extern "C" fn #fn_ptr_mut_ident(
            ptr: *mut ::core::ffi::c_void,
        ) -> *mut ::core::ffi::c_void {
            if ptr.is_null() {
                return ::core::ptr::null_mut();
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to nested ptr_mut getter"
            );
            // SAFETY: `typed_ptr` is verified non-null and points to a valid mutable `#struct_ident`;
            // `addr_of_mut!` computes the mutable interior pointer without aliasing violations.
            unsafe {
                ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens).cast::<::core::ffi::c_void>()
            }
        }
    });

    state.vtable_fields.push(quote! {
        pub #vt_ptr_ident: unsafe extern "C" fn(*const ::core::ffi::c_void) -> *const ::core::ffi::c_void,
        pub #vt_ptr_mut_ident: unsafe extern "C" fn(*mut ::core::ffi::c_void) -> *mut ::core::ffi::c_void,
    });

    state.vtable_inits.push(quote! {
        #vt_ptr_ident: #fn_ptr_ident,
        #vt_ptr_mut_ident: #fn_ptr_mut_ident,
    });

    state.push_symbol_pair(&ids);
    Ok(())
}
