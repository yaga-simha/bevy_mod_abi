use crate::attrs::ReprFlags;
use crate::codegen::CodegenState;
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Ident, LitByteStr};

pub(crate) fn emit_vtable_and_impls(
    state: &CodegenState,
    vtable_ident: &Ident,
    component_name_str: &str,
    cstr_lit: &LitByteStr,
    repr_flags: ReprFlags,
    no_pod: bool,
) -> TokenStream {
    let sys_crate = &state.sys_crate;
    let struct_ident = &state.struct_ident;
    let layout_assertions = &state.layout_assertions;
    let extern_fns = &state.extern_fns;
    let vtable_fields = &state.vtable_fields;
    let vtable_inits = &state.vtable_inits;
    let symbol_entries = &state.symbol_entries;
    let nested_pod_assertions = &state.nested_pod_assertions;

    let is_raw_insertable = state.can_impl_ffi_layout;
    let is_pod = state.can_impl_ffi_layout && repr_flags.is_deterministic_unpacked() && !no_pod;

    let conditional_ffi_layout_impl = if is_raw_insertable {
        quote! {
            ::core::assert!(!::core::mem::needs_drop::<#struct_ident>());

            // SAFETY: `#struct_ident` is a non-packed `'static` struct with zero drop glue, no skipped fields,
            // no `String`/`Vec`/`bool` fields, and every field verified `FfiLayout` / `AbiPod` above.
            #[allow(clippy::undocumented_unsafe_blocks)]
            unsafe impl #sys_crate::FfiLayout for #struct_ident {}
        }
    } else {
        quote! {}
    };

    let conditional_abi_pod_impl = if is_pod {
        quote! {
            fn assert_pod_self_copy<T: ::core::marker::Copy + 'static>() {}
            fn assert_nested_pod<T: #sys_crate::AbiPod>() {}
            fn check_pod_invariants() {
                assert_pod_self_copy::<#struct_ident>();
                #(#nested_pod_assertions)*
            }

            // SAFETY: `#struct_ident` is `#[repr(C)]` or `#[repr(transparent)]` (non-packed), `Copy + 'static`,
            // has zero drop glue, no skipped fields, and every field is verified `AbiPod` above.
            #[allow(clippy::undocumented_unsafe_blocks)]
            unsafe impl #sys_crate::AbiPod for #struct_ident {}
        }
    } else {
        quote! {}
    };

    quote! {
        /// C-ABI VTable for `#struct_ident`, containing exact byte stride (`size`),
        /// alignment (`align`), and field accessor function pointers.
        #[allow(unreachable_pub)]
        #[repr(C)]
        #[derive(Debug, Copy, Clone)]
        pub struct #vtable_ident {
            pub size: usize,
            pub align: usize,
            #(#vtable_fields)*
        }

        #[allow(non_snake_case)]
        const _: () = {
            #(#layout_assertions)*
            #(#extern_fns)*

            #[allow(unreachable_pub)]
            impl #struct_ident {
                /// Canonical component name registered across the C-ABI boundary.
                pub const MOD_COMPONENT_NAME: &'static str = #component_name_str;

                /// Compile-time C-ABI VTable constant for `#struct_ident`.
                pub const VTABLE: #vtable_ident = #vtable_ident {
                    size: ::core::mem::size_of::<#struct_ident>(),
                    align: ::core::mem::align_of::<#struct_ident>(),
                    #(#vtable_inits)*
                };

                /// Returns a static reference to this component's C-ABI VTable.
                #[inline]
                pub fn vtable() -> &'static #vtable_ident {
                    <#struct_ident as #sys_crate::ExposableComponent>::vtable()
                }
            }

            #conditional_ffi_layout_impl
            #conditional_abi_pod_impl

            static VTABLE_INSTANCE: #vtable_ident = #struct_ident::VTABLE;

            static SYMBOLS: &[#sys_crate::SymbolEntry] = &[
                #(#symbol_entries),*
            ];

            impl #sys_crate::ExposableComponent for #struct_ident {
                const COMPONENT_NAME: &'static str = #component_name_str;
                const COMPONENT_CSTR: &'static ::core::ffi::CStr =
                    match ::core::ffi::CStr::from_bytes_with_nul(#cstr_lit) {
                        ::core::result::Result::Ok(s) => s,
                        ::core::result::Result::Err(_) => panic!("invalid CStr literal"),
                    };
                const UUID: #sys_crate::ComponentUuid =
                    #sys_crate::ComponentUuid::from_name(#component_name_str);
                const IS_POD: bool = #is_pod;
                const IS_RAW_INSERTABLE: bool = #is_raw_insertable;

                type VTable = #vtable_ident;

                #[inline]
                fn vtable() -> &'static Self::VTable {
                    &VTABLE_INSTANCE
                }

                #[inline]
                fn symbols() -> &'static [#sys_crate::SymbolEntry] {
                    SYMBOLS
                }
            }
        };
    }
}
