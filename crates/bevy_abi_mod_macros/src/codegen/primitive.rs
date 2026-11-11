use crate::codegen::CodegenState;
use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::{Field, Type};

pub(crate) fn emit_primitive_field(
    state: &mut CodegenState,
    field: &Field,
    field_name: &str,
    member_tokens: &TokenStream,
    prim_ty: &Type,
    field_span: Span,
) -> syn::Result<()> {
    let ids = state.register_pair(field, field_name, "get", "set", "accessor")?;
    let get_export = state.export_prefix(&ids.fn_first_str);
    let set_export = state.export_prefix(&ids.fn_second_str);

    let sys_crate = &state.sys_crate;
    let struct_ident = &state.struct_ident;
    let fn_get_ident = &ids.fn_first_ident;
    let fn_set_ident = &ids.fn_second_ident;
    let vt_get_ident = &ids.vt_first_ident;
    let vt_set_ident = &ids.vt_second_ident;

    state.layout_assertions.push(quote_spanned! { field_span =>
        const _: fn() = || {
            fn assert_primitive_ffi<T: #sys_crate::AbiPod + ::core::default::Default>() {}
            assert_primitive_ffi::<#prim_ty>();
        };
    });

    state.extern_fns.push(quote! {
        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #get_export
        unsafe extern "C" fn #fn_get_ident(
            ptr: *const ::core::ffi::c_void,
        ) -> #prim_ty {
            if ptr.is_null() {
                return <#prim_ty as ::core::default::Default>::default();
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to getter"
            );
            // SAFETY: Caller guarantees `ptr` points to a valid, aligned `#struct_ident`
            // and `ptr` was verified non-null above.
            unsafe {
                ::core::ptr::read(::core::ptr::addr_of!((*typed_ptr).#member_tokens))
            }
        }

        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #set_export
        unsafe extern "C" fn #fn_set_ident(
            ptr: *mut ::core::ffi::c_void,
            val: #prim_ty,
        ) {
            if ptr.is_null() {
                return;
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to setter"
            );
            // SAFETY: Caller guarantees `ptr` points to a valid, aligned, mutable `#struct_ident`
            // and `ptr` was verified non-null above.
            unsafe {
                ::core::ptr::write(
                    ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                    val,
                );
            }
        }
    });

    state.vtable_fields.push(quote! {
        pub #vt_get_ident: unsafe extern "C" fn(*const ::core::ffi::c_void) -> #prim_ty,
        pub #vt_set_ident: unsafe extern "C" fn(*mut ::core::ffi::c_void, #prim_ty),
    });

    state.vtable_inits.push(quote! {
        #vt_get_ident: #fn_get_ident,
        #vt_set_ident: #fn_set_ident,
    });

    state.push_symbol_pair(&ids);
    Ok(())
}
