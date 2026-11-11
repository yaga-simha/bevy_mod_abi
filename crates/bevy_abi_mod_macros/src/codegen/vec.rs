use crate::codegen::CodegenState;
use proc_macro2::{Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::{Field, Type};

pub(crate) fn emit_vec_field(
    state: &mut CodegenState,
    field: &Field,
    field_name: &str,
    member_tokens: &TokenStream,
    elem_ty: &Type,
    field_span: Span,
) -> syn::Result<()> {
    state.can_impl_ffi_layout = false;

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
            fn assert_vec_elem_ffi<T: #sys_crate::AbiPod>() {}
            assert_vec_elem_ffi::<#elem_ty>();
        };
    });

    state.extern_fns.push(quote! {
        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #get_export
        unsafe extern "C" fn #fn_get_ident(
            ptr: *const ::core::ffi::c_void,
        ) -> #sys_crate::FfiSlice<#elem_ty> {
            if ptr.is_null() {
                return #sys_crate::FfiSlice::empty();
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to vec getter"
            );
            // SAFETY: Caller guarantees `ptr` points to a valid `#struct_ident` instance.
            let field_ref: &::std::vec::Vec<#elem_ty> =
                unsafe { &*(::core::ptr::addr_of!((*typed_ptr).#member_tokens)) };
            #sys_crate::FfiSlice::from_slice(field_ref.as_slice())
        }

        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #set_export
        unsafe extern "C" fn #fn_set_ident(
            ptr: *mut ::core::ffi::c_void,
            data_ptr: *const #elem_ty,
            len: usize,
        ) {
            if ptr.is_null() {
                return;
            }
            if len > 0 && data_ptr.is_null() {
                return;
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to vec setter"
            );
            // SAFETY: `typed_ptr` is verified non-null and points to a valid `#struct_ident`.
            let field_ptr: *mut ::std::vec::Vec<#elem_ty> =
                unsafe { ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens) };

            if len == 0 {
                // SAFETY: `field_ptr` points to a valid host-owned `Vec`.
                unsafe { (*field_ptr).clear() };
                return;
            }

            // Detect whether `[data_ptr .. data_ptr + len * size_of::<T>()]` overlaps existing buffer.
            // SAFETY: Reading current slice range before taking `&mut Vec`.
            let (buf_start, buf_end) = unsafe {
                let range = (*field_ptr).as_slice().as_ptr_range();
                (range.start as usize, range.end as usize)
            };
            let src_start = data_ptr as usize;
            let src_end = src_start.saturating_add(
                len.saturating_mul(::core::mem::size_of::<#elem_ty>()),
            );
            let overlaps = src_start < buf_end && buf_start < src_end;

            // SAFETY: `data_ptr` is verified non-null and caller guarantees `len` valid elements.
            let slice: &[#elem_ty] =
                unsafe { ::core::slice::from_raw_parts(data_ptr, len) };

            if overlaps {
                let replacement: ::std::vec::Vec<#elem_ty> = slice.to_vec();
                // SAFETY: `slice` is no longer accessed after `replacement` is allocated.
                unsafe {
                    *field_ptr = replacement;
                }
            } else {
                // SAFETY: `slice` is disjoint from `*field_ptr`'s buffer.
                unsafe {
                    let field_mut = &mut *field_ptr;
                    field_mut.clear();
                    field_mut.extend_from_slice(slice);
                }
            }
        }
    });

    state.vtable_fields.push(quote! {
        pub #vt_get_ident: unsafe extern "C" fn(*const ::core::ffi::c_void) -> #sys_crate::FfiSlice<#elem_ty>,
        pub #vt_set_ident: unsafe extern "C" fn(*mut ::core::ffi::c_void, *const #elem_ty, usize),
    });

    state.vtable_inits.push(quote! {
        #vt_get_ident: #fn_get_ident,
        #vt_set_ident: #fn_set_ident,
    });

    state.push_symbol_pair(&ids);
    Ok(())
}
