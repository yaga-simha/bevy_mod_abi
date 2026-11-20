use std::ffi::CString;

use crate::attrs::{
    parse_guest_component_attrs, parse_repr_flags, resolve_guest_crate_path, resolve_sys_crate_path,
};
use crate::classify::canonical_field_type_tag;
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Index, LitCStr};

/// Core expansion entry point for `#[derive(GuestComponent)]`.
pub(crate) fn expand_guest_component(input: &DeriveInput) -> syn::Result<TokenStream> {
    expand_guest_component_inner(input, None, None)
}

#[cfg(test)]
pub(crate) fn expand_guest_component_with_crates(
    input: &DeriveInput,
    sys_crate: &TokenStream,
    guest_crate: &TokenStream,
) -> syn::Result<TokenStream> {
    expand_guest_component_inner(input, Some(sys_crate.clone()), Some(guest_crate.clone()))
}

fn expand_guest_component_inner(
    input: &DeriveInput,
    sys_crate_override: Option<TokenStream>,
    guest_crate_override: Option<TokenStream>,
) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(GuestComponent)]` does not support generic structs",
        ));
    }

    let fields = match &input.data {
        Data::Struct(data_struct) => &data_struct.fields,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`#[derive(GuestComponent)]` can only be applied to structs (`GuestComponent` requires `AbiPod`)",
            ));
        }
    };

    let attrs = parse_guest_component_attrs(input)?;
    let repr_flags = parse_repr_flags(&input.attrs)?;
    if repr_flags.has_packed {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(GuestComponent)]` cannot be applied to `#[repr(packed)]` structs because `GuestComponent` requires `AbiPod`",
        ));
    }
    if !repr_flags.has_c && !repr_flags.has_transparent {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(GuestComponent)]` requires `#[repr(C)]` or `#[repr(transparent)]` to guarantee deterministic cross-language memory layout",
        ));
    }

    let sys_crate =
        sys_crate_override.unwrap_or_else(|| resolve_sys_crate_path(attrs.crate_override.as_ref()));
    let guest_crate = guest_crate_override
        .unwrap_or_else(|| resolve_guest_crate_path(attrs.guest_crate_override.as_ref()));

    let struct_ident = &input.ident;
    let base_name = attrs
        .custom_name
        .unwrap_or_else(|| struct_ident.to_string());
    let full_name = match attrs.custom_namespace {
        Some(ns) => format!("{ns}::{base_name}"),
        None => base_name,
    };
    let name_cstr = CString::new(full_name.as_bytes()).map_err(|_| {
        syn::Error::new_spanned(
            struct_ident,
            "component name must not contain interior NUL bytes",
        )
    })?;
    let name_lit = LitCStr::new(&name_cstr, struct_ident.span());

    let schema_expr = if attrs.no_schema_hash {
        quote! { 0u64 }
    } else {
        let mut expr = quote! { 0u64 };
        match fields {
            Fields::Named(named) => {
                for field in &named.named {
                    let Some(ident) = field.ident.as_ref() else {
                        continue;
                    };
                    let field_name = ident.to_string();
                    let field_tag = canonical_field_type_tag(&field.ty);
                    expr = quote! {
                        #sys_crate::ComponentUuid::hash_field_schema(
                            #expr,
                            #field_name,
                            #field_tag,
                            ::core::mem::offset_of!(#struct_ident, #ident),
                        )
                    };
                }
            }
            Fields::Unnamed(unnamed) => {
                for (idx, field) in unnamed.unnamed.iter().enumerate() {
                    let field_name = idx.to_string();
                    let field_tag = canonical_field_type_tag(&field.ty);
                    let index = Index::from(idx);
                    expr = quote! {
                        #sys_crate::ComponentUuid::hash_field_schema(
                            #expr,
                            #field_name,
                            #field_tag,
                            ::core::mem::offset_of!(#struct_ident, #index),
                        )
                    };
                }
            }
            Fields::Unit => {}
        }
        expr
    };

    let mut flag_terms = Vec::new();
    if attrs.sparse_set {
        flag_terms.push(quote! { #sys_crate::BAM_COMPONENT_FLAG_SPARSE_SET });
    }
    if attrs.immutable {
        flag_terms.push(quote! { #sys_crate::BAM_COMPONENT_FLAG_IMMUTABLE });
    }
    if attrs.summary_tick {
        flag_terms.push(quote! { #sys_crate::BAM_COMPONENT_FLAG_SUMMARY_TICK });
    }
    let flags_expr = if flag_terms.is_empty() {
        quote! { 0u32 }
    } else {
        quote! { #(#flag_terms)|* }
    };

    let uuid_item = match attrs.custom_uuid.as_deref() {
        Some(hex_str) => quote! {
            const UUID: #sys_crate::ComponentUuid = #sys_crate::ComponentUuid::parse_hex(#hex_str);
        },
        None => quote! {},
    };

    Ok(quote! {
        const _: () = {
            impl #guest_crate::GuestComponent for #struct_ident {
                const NAME: &'static ::core::ffi::CStr = #name_lit;
                const FIELD_SCHEMA_HASH: u64 = #schema_expr;
                const FLAGS: u32 = #flags_expr;
                #uuid_item
            }
        };
    })
}
