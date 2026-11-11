use crate::codegen::CodegenState;
use proc_macro2::TokenStream;
use quote::quote;
use syn::Field;

pub(crate) fn emit_string_field(
    state: &mut CodegenState,
    field: &Field,
    field_name: &str,
    member_tokens: &TokenStream,
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

    state.extern_fns.push(quote! {
        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #get_export
        unsafe extern "C" fn #fn_get_ident(
            ptr: *const ::core::ffi::c_void,
        ) -> #sys_crate::FfiSlice<u8> {
            if ptr.is_null() {
                return #sys_crate::FfiSlice::empty();
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to string getter"
            );
            // SAFETY: Caller guarantees `ptr` points to a valid `#struct_ident` instance
            // whose `String` field remains alive for the borrow duration.
            let field_ref: &::std::string::String =
                unsafe { &*(::core::ptr::addr_of!((*typed_ptr).#member_tokens)) };
            #sys_crate::FfiSlice::from_str(field_ref.as_str())
        }

        #[allow(non_snake_case, clippy::undocumented_unsafe_blocks, clippy::ptr_as_ptr)]
        #set_export
        unsafe extern "C" fn #fn_set_ident(
            ptr: *mut ::core::ffi::c_void,
            utf8_ptr: *const u8,
            len: usize,
        ) {
            if ptr.is_null() {
                return;
            }
            if len > 0 && utf8_ptr.is_null() {
                return;
            }
            let typed_ptr = ptr.cast::<#struct_ident>();
            ::core::debug_assert!(
                (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                "unaligned component pointer passed to string setter"
            );
            // SAFETY: `typed_ptr` is verified non-null and points to a valid `#struct_ident`.
            let field_ptr: *mut ::std::string::String =
                unsafe { ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens) };

            if len == 0 {
                // SAFETY: `field_ptr` points to a valid host-owned `String` and `len == 0`
                // does not read from `utf8_ptr`.
                unsafe { (*field_ptr).clear() };
                return;
            }

            // Detect whether `[utf8_ptr .. utf8_ptr + len]` overlaps the existing `String` buffer.
            // SAFETY: Reading the current buffer pointer and length via shared reference before
            // taking any mutable reference to `*field_ptr`.
            let (buf_start, buf_end) = unsafe {
                let existing_bytes = (*field_ptr).as_bytes();
                let range = existing_bytes.as_ptr_range();
                (range.start as usize, range.end as usize)
            };
            let src_start = utf8_ptr as usize;
            let src_end = src_start.saturating_add(len);
            let overlaps = src_start < buf_end && buf_start < src_end;

            // SAFETY: `utf8_ptr` is verified non-null and caller guarantees `len` initialized bytes.
            // No `&mut String` exists while `bytes` is live.
            let bytes: &[u8] = unsafe { ::core::slice::from_raw_parts(utf8_ptr, len) };
            if let ::core::result::Result::Ok(valid_str) = ::core::str::from_utf8(bytes) {
                if overlaps {
                    let replacement = ::std::string::String::from(valid_str);
                    // SAFETY: `valid_str` is no longer used after `replacement` is constructed;
                    // replacing `*field_ptr` drops the old buffer cleanly on the host allocator.
                    unsafe {
                        *field_ptr = replacement;
                    }
                } else {
                    // SAFETY: `valid_str` does not overlap `*field_ptr`'s buffer, so taking
                    // `&mut *field_ptr` and copying bytes in-place is disjoint and aliasing-safe.
                    unsafe {
                        let field_mut = &mut *field_ptr;
                        field_mut.clear();
                        field_mut.push_str(valid_str);
                    }
                }
            }
        }
    });

    state.vtable_fields.push(quote! {
        pub #vt_get_ident: unsafe extern "C" fn(*const ::core::ffi::c_void) -> #sys_crate::FfiSlice<u8>,
        pub #vt_set_ident: unsafe extern "C" fn(*mut ::core::ffi::c_void, *const u8, usize),
    });

    state.vtable_inits.push(quote! {
        #vt_get_ident: #fn_get_ident,
        #vt_set_ident: #fn_set_ident,
    });

    state.push_symbol_pair(&ids);
    Ok(())
}
