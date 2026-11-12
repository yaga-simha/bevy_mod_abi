use crate::attrs::{parse_abi_pod_crate_override, parse_repr_flags, resolve_sys_crate_path};
use proc_macro2::TokenStream;
use quote::{quote, quote_spanned};
use syn::{Data, DeriveInput, spanned::Spanned};

/// Core expansion function for `#[derive(AbiPod)]`.
pub(crate) fn expand_abi_pod(input: &DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(AbiPod)]` does not support generic structs",
        ));
    }

    let fields = match &input.data {
        Data::Struct(data_struct) => &data_struct.fields,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`#[derive(AbiPod)]` can only be applied to structs",
            ));
        }
    };

    let crate_override = parse_abi_pod_crate_override(input)?;
    let sys_crate = resolve_sys_crate_path(crate_override.as_ref());

    let repr_flags = parse_repr_flags(&input.attrs)?;
    if repr_flags.has_packed {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(AbiPod)]` cannot be applied to `#[repr(packed)]` structs because unaligned field references are undefined behavior",
        ));
    }
    if !repr_flags.has_c && !repr_flags.has_transparent {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(AbiPod)]` requires `#[repr(C)]` or `#[repr(transparent)]` to guarantee deterministic cross-language memory layout",
        ));
    }

    let struct_ident = &input.ident;
    let mut field_assertions = Vec::new();
    for field in fields.iter() {
        let field_ty = &field.ty;
        let span = field_ty.span();
        field_assertions.push(quote_spanned! { span =>
            #sys_crate::assert_abi_pod::<#field_ty>();
        });
    }

    Ok(quote! {
        const _: () = {
            fn assert_self_copy<T: ::core::marker::Copy + 'static>() {}
            fn check_all() {
                assert_self_copy::<#struct_ident>();
                #(#field_assertions)*
            }
            ::core::assert!(!::core::mem::needs_drop::<#struct_ident>());

            // SAFETY: Verified `#[repr(C)]` / `#[repr(transparent)]` non-packed struct, `Copy + 'static`,
            // zero drop glue, and every field verified `AbiPod` via `assert_abi_pod` above.
            #[allow(clippy::undocumented_unsafe_blocks)]
            unsafe impl #sys_crate::RawInsertable for #struct_ident {}

            // SAFETY: All cross-language `AbiPod` layout and bit-validity invariants are verified above
            // (`u128`, `i128`, `usize`, `isize`, `bool`, `char`, heap types, and enums are rejected).
            #[allow(clippy::undocumented_unsafe_blocks)]
            unsafe impl #sys_crate::AbiPod for #struct_ident {}
        };
    })
}
