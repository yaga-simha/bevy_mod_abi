use crate::codegen::CodegenState;
use proc_macro2::TokenStream;
use quote::quote;
use syn::Field;

pub(crate) fn emit_bool_field(
    state: &mut CodegenState,
    field: &Field,
    field_name: &str,
    member_tokens: &TokenStream,
) -> syn::Result<()> {
    // `bool` is not bit-valid for arbitrary `u8` values (`2..=255` is UB in Rust),
    // so structs with `bool` fields disable raw-byte insertion and `AbiPod` column casting,
    // and pass `u8` across `extern "C"`.
    state.can_impl_ffi_layout = false;

    let ids = state.register_pair(field, field_name, "get", "set", "accessor")?;
    let get_export = state.export_prefix(&ids.fn_first_str);
    let set_export = state.export_prefix(&ids.fn_second_str);

    let struct_ident = &state.struct_ident;
    let fn_get_ident = &ids.fn_first_ident;
    let fn_set_ident = &ids.fn_second_ident;
    let vt_get_ident = &ids.vt_first_ident;
    let vt_set_ident = &ids.vt_second_ident;

    state.extern_fns.push(quote! {
        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #get_export
        unsafe extern "C" fn #fn_get_ident(
            ptr: *const ::core::ffi::c_void,
        ) -> u8 {
            if ptr.is_null() {
                return 0;
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to bool getter"
            );
            // SAFETY: Caller guarantees `ptr` points to a valid, aligned `#struct_ident`
            // and `ptr` was verified non-null above.
            let flag: bool = unsafe {
                ::core::ptr::read(::core::ptr::addr_of!((*typed_ptr).#member_tokens))
            };
            u8::from(flag)
        }

        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #set_export
        unsafe extern "C" fn #fn_set_ident(
            ptr: *mut ::core::ffi::c_void,
            val: u8,
        ) {
            if ptr.is_null() {
                return;
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to bool setter"
            );
            let normalized: bool = val != 0;
            // SAFETY: Caller guarantees `ptr` points to a valid, aligned, mutable `#struct_ident`
            // and `normalized` is canonical `false` or `true`.
            unsafe {
                ::core::ptr::write(
                    ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                    normalized,
                );
            }
        }
    });

    state.vtable_fields.push(quote! {
        pub #vt_get_ident: unsafe extern "C" fn(*const ::core::ffi::c_void) -> u8,
        pub #vt_set_ident: unsafe extern "C" fn(*mut ::core::ffi::c_void, u8),
    });

    state.vtable_inits.push(quote! {
        #vt_get_ident: #fn_get_ident,
        #vt_set_ident: #fn_set_ident,
    });

    state.push_symbol_pair(&ids);
    Ok(())
}
