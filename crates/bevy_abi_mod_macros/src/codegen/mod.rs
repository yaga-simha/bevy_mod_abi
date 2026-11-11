mod bool;
mod nested;
mod primitive;
mod string;
mod vec;
mod vtable;

use crate::attrs::{
    parse_component_attrs, parse_field_attr, parse_repr_flags, resolve_sys_crate_path,
    sanitize_symbol_prefix,
};
use crate::classify::{FieldKind, classify_type};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::{Data, DeriveInput, Field, Fields, Ident, Index, LitByteStr, spanned::Spanned};

pub(crate) struct AccessorIdents {
    pub(crate) fn_first_str: String,
    pub(crate) fn_second_str: String,
    pub(crate) fn_first_ident: Ident,
    pub(crate) fn_second_ident: Ident,
    pub(crate) vt_first_ident: Ident,
    pub(crate) vt_second_ident: Ident,
}

pub(crate) struct CodegenState {
    pub(crate) sys_crate: TokenStream,
    pub(crate) struct_ident: Ident,
    pub(crate) fn_prefix: String,
    pub(crate) export_symbols: bool,
    pub(crate) opaque: bool,
    pub(crate) can_impl_ffi_layout: bool,
    pub(crate) extern_fns: Vec<TokenStream>,
    pub(crate) vtable_fields: Vec<TokenStream>,
    pub(crate) vtable_inits: Vec<TokenStream>,
    pub(crate) symbol_entries: Vec<TokenStream>,
    pub(crate) layout_assertions: Vec<TokenStream>,
    pub(crate) nested_pod_assertions: Vec<TokenStream>,
    pub(crate) used_vtable_fields: HashSet<String>,
    pub(crate) used_symbol_names: HashSet<String>,
}

impl CodegenState {
    pub(crate) fn register_pair(
        &mut self,
        field: &Field,
        field_name: &str,
        vt_first_prefix: &str,
        vt_second_prefix: &str,
        collision_kind: &str,
    ) -> syn::Result<AccessorIdents> {
        let vt_first_str = format!("{vt_first_prefix}_{field_name}");
        let vt_second_str = format!("{vt_second_prefix}_{field_name}");
        let fn_first_str = format!("{}_{vt_first_str}", self.fn_prefix);
        let fn_second_str = format!("{}_{vt_second_str}", self.fn_prefix);

        if !self.used_vtable_fields.insert(vt_first_str.clone())
            || !self.used_vtable_fields.insert(vt_second_str.clone())
            || !self.used_symbol_names.insert(fn_first_str.clone())
            || !self.used_symbol_names.insert(fn_second_str.clone())
        {
            return Err(syn::Error::new_spanned(
                field,
                format!(
                    "field `{field_name}` produces a colliding {collision_kind} name (`{fn_first_str}` / `{fn_second_str}`)"
                ),
            ));
        }

        Ok(AccessorIdents {
            fn_first_ident: Ident::new(&fn_first_str, Span::call_site()),
            fn_second_ident: Ident::new(&fn_second_str, Span::call_site()),
            vt_first_ident: Ident::new(&vt_first_str, Span::call_site()),
            vt_second_ident: Ident::new(&vt_second_str, Span::call_site()),
            fn_first_str,
            fn_second_str,
        })
    }

    pub(crate) fn export_prefix(&self, symbol_name: &str) -> TokenStream {
        if self.export_symbols {
            quote! {
                #[allow(unreachable_pub)]
                #[unsafe(export_name = #symbol_name)]
                pub
            }
        } else {
            quote! {}
        }
    }

    pub(crate) fn push_symbol_pair(&mut self, ids: &AccessorIdents) {
        let sys_crate = &self.sys_crate;
        let fn_first_str = &ids.fn_first_str;
        let fn_second_str = &ids.fn_second_str;
        let fn_first_ident = &ids.fn_first_ident;
        let fn_second_ident = &ids.fn_second_ident;
        self.symbol_entries.push(quote! {
            #sys_crate::SymbolEntry {
                name: #fn_first_str,
                ptr: #fn_first_ident as *const ::core::ffi::c_void,
            },
            #sys_crate::SymbolEntry {
                name: #fn_second_str,
                ptr: #fn_second_ident as *const ::core::ffi::c_void,
            }
        });
    }
}

/// Core expansion function for `#[derive(ModExposable)]`.
pub(crate) fn expand_mod_exposable(input: &DeriveInput) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(ModExposable)]` does not support generic structs",
        ));
    }

    let comp_attrs = parse_component_attrs(input)?;
    let repr_flags = parse_repr_flags(&input.attrs)?;
    if repr_flags.has_packed {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(ModExposable)]` cannot be applied to `#[repr(packed)]` structs because aligned field pointers and references are undefined behavior",
        ));
    }

    let sys_crate = resolve_sys_crate_path(comp_attrs.crate_override.as_ref());
    let struct_ident = input.ident.clone();
    let struct_ident_str = struct_ident.to_string();
    let component_name_str = comp_attrs
        .custom_name
        .clone()
        .unwrap_or_else(|| struct_ident_str.clone());
    let fn_prefix = comp_attrs.custom_prefix.clone().unwrap_or_else(|| {
        if comp_attrs.custom_name.is_some() {
            sanitize_symbol_prefix(&component_name_str)
        } else {
            struct_ident_str.clone()
        }
    });

    let vtable_ident = format_ident!("{}VTable", struct_ident);

    let mut cstr_bytes = component_name_str.as_bytes().to_vec();
    if cstr_bytes.contains(&0) {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "component name must not contain interior NUL bytes",
        ));
    }
    cstr_bytes.push(0);
    let cstr_lit = LitByteStr::new(&cstr_bytes, Span::call_site());

    let fields = match &input.data {
        Data::Struct(data_struct) => &data_struct.fields,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`#[derive(ModExposable)]` can only be applied to structs",
            ));
        }
    };

    let mut state = CodegenState {
        sys_crate: sys_crate.clone(),
        struct_ident,
        fn_prefix: fn_prefix.clone(),
        export_symbols: comp_attrs.export_symbols,
        opaque: comp_attrs.opaque,
        can_impl_ffi_layout: !comp_attrs.opaque,
        extern_fns: Vec::new(),
        vtable_fields: Vec::new(),
        vtable_inits: Vec::new(),
        symbol_entries: Vec::new(),
        layout_assertions: Vec::new(),
        nested_pod_assertions: Vec::new(),
        used_vtable_fields: HashSet::new(),
        used_symbol_names: HashSet::new(),
    };

    // Register `<Prefix>_vtable` and `<ComponentName>` symbols pointing to `VTABLE_INSTANCE`.
    let vtable_sym_name = format!("{fn_prefix}_vtable");
    state.used_symbol_names.insert(component_name_str.clone());
    state.used_symbol_names.insert(vtable_sym_name.clone());

    state.symbol_entries.push(quote! {
        #sys_crate::SymbolEntry {
            name: #component_name_str,
            ptr: &VTABLE_INSTANCE as *const #vtable_ident as *const ::core::ffi::c_void,
        },
        #sys_crate::SymbolEntry {
            name: #vtable_sym_name,
            ptr: &VTABLE_INSTANCE as *const #vtable_ident as *const ::core::ffi::c_void,
        }
    });

    if comp_attrs.custom_name.is_some() && component_name_str != struct_ident_str {
        let short_vtable_sym = format!("{struct_ident_str}_vtable");
        if state.used_symbol_names.insert(short_vtable_sym.clone()) {
            state.symbol_entries.push(quote! {
                #sys_crate::SymbolEntry {
                    name: #short_vtable_sym,
                    ptr: &VTABLE_INSTANCE as *const #vtable_ident as *const ::core::ffi::c_void,
                }
            });
        }
    }

    let field_iter: Vec<(String, TokenStream, &Field)> = match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                let ident = f.ident.as_ref().expect("named field must have ident");
                (ident.to_string(), quote! { #ident }, f)
            })
            .collect(),
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(idx, f)| {
                let index = Index::from(idx);
                (idx.to_string(), quote! { #index }, f)
            })
            .collect(),
        Fields::Unit => Vec::new(),
    };

    for (field_name, member_tokens, field) in field_iter {
        let attr_override = parse_field_attr(&field.attrs)?;
        let kind = classify_type(&field.ty, attr_override)?;
        let field_span = field.ty.span();

        match kind {
            FieldKind::Skip => {
                state.can_impl_ffi_layout = false;
            }
            FieldKind::Bool => {
                bool::emit_bool_field(&mut state, field, &field_name, &member_tokens)?;
            }
            FieldKind::Primitive(prim_ty) => {
                primitive::emit_primitive_field(
                    &mut state,
                    field,
                    &field_name,
                    &member_tokens,
                    &prim_ty,
                    field_span,
                )?;
            }
            FieldKind::HostString => {
                string::emit_string_field(&mut state, field, &field_name, &member_tokens)?;
            }
            FieldKind::HostVec(elem_ty) => {
                vec::emit_vec_field(
                    &mut state,
                    field,
                    &field_name,
                    &member_tokens,
                    &elem_ty,
                    field_span,
                )?;
            }
            FieldKind::Nested(nested_ty) => {
                nested::emit_nested_field(
                    &mut state,
                    field,
                    &field_name,
                    &member_tokens,
                    &nested_ty,
                    field_span,
                )?;
            }
        }
    }

    Ok(vtable::emit_vtable_and_impls(
        &state,
        &vtable_ident,
        &component_name_str,
        &cstr_lit,
        repr_flags,
        comp_attrs.no_pod,
    ))
}
