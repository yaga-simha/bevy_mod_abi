mod bool;
pub(crate) mod c_decl;
mod nested;
mod primitive;
mod string;
mod vec;
mod vtable;

use crate::attrs::{
    parse_component_attrs, parse_field_attr, parse_repr_flags, resolve_sys_crate_path,
    sanitize_symbol_segment,
};
use crate::classify::{FieldKind, canonical_field_type_tag, classify_type};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::{Data, DeriveInput, Field, Fields, Ident, Index, spanned::Spanned};

/// Representation of a component's canonical namespaced identifier (`"<namespace>::<Name>"`).
pub(crate) enum CanonicalNameSpec {
    /// Fully known at macro-expansion time (`#[mod_component(name = "ns::Name")]` or `namespace = "..."`).
    Literal(String),
    /// Prefixed with `::core::module_path!()` at compile time.
    ModulePathPrefixed { short_name: String },
}

impl CanonicalNameSpec {
    /// Emits a `const`-compatible `&'static str` expression for `ModComponent::CANONICAL_NAME`.
    pub(crate) fn as_str_expr(&self) -> TokenStream {
        match self {
            Self::Literal(s) => quote! { #s },
            Self::ModulePathPrefixed { short_name } => {
                let suffix = format!("::{short_name}");
                quote! { ::core::concat!(::core::module_path!(), #suffix) }
            }
        }
    }

    /// Emits a `const`-compatible `&'static [u8]` NUL-terminated byte-slice expression for
    /// `ModComponent::COMPONENT_CSTR`.
    pub(crate) fn as_nul_bytes_expr(&self) -> TokenStream {
        match self {
            Self::Literal(s) => {
                let with_nul = format!("{s}\0");
                quote! { #with_nul.as_bytes() }
            }
            Self::ModulePathPrefixed { short_name } => {
                let suffix = format!("::{short_name}\0");
                quote! { ::core::concat!(::core::module_path!(), #suffix).as_bytes() }
            }
        }
    }

    /// Emits a `const`-compatible `&'static str` expression for `"bam.v0.<canonical_name>.<slot>"`.
    pub(crate) fn symbol_name_expr(&self, slot_suffix: &str) -> TokenStream {
        match self {
            Self::Literal(s) => {
                let sym = format!("bam.v0.{s}.{slot_suffix}");
                quote! { #sym }
            }
            Self::ModulePathPrefixed { short_name } => {
                let suffix = format!("::{short_name}.{slot_suffix}");
                quote! { ::core::concat!("bam.v0.", ::core::module_path!(), #suffix) }
            }
        }
    }
}

/// Receiver pointer mutability and standardized null/alignment validation for a generated C-ABI slot.
pub(crate) enum ReceiverKind {
    /// `*const c_void` getter / interior-pointer slot: returns `on_invalid` when `ptr.is_null()`
    /// or `!typed_ptr.is_aligned()`, otherwise binds `typed_ptr: *const #struct_ident`.
    Const { on_invalid: TokenStream },
    /// `*mut c_void` setter / mutable interior-pointer slot:
    /// 1. Returns `on_null` when `ptr.is_null()`.
    /// 2. Runs optional `pre_align_check` (e.g. non-empty slice null check -> `NULL_POINTER`).
    /// 3. Returns `on_unaligned` when `!typed_ptr.is_aligned()`.
    /// 4. Executes `valid_body` with `typed_ptr: *mut #struct_ident`.
    Mut {
        on_null: TokenStream,
        pre_align_check: Option<TokenStream>,
        on_unaligned: TokenStream,
    },
}

/// Specification for a single generated `extern "C"` function and VTable slot.
pub(crate) struct SlotSpec {
    /// Prefix joined with `_{field_name}` (`"get"`, `"set"`, `"push"`, `"ptr"`, `"ptr_mut"`).
    pub(crate) prefix: &'static str,
    /// Receiver pointer validation contract.
    pub(crate) receiver: ReceiverKind,
    /// Trailing parameter declarations for the `extern "C" fn` (including leading comma if non-empty).
    pub(crate) extra_params_fn: TokenStream,
    /// Trailing parameter types for the VTable `Option<unsafe extern "C" fn(...)>` type.
    pub(crate) extra_params_vt: TokenStream,
    /// Return type of the `extern "C" fn`.
    pub(crate) ret_ty: TokenStream,
    /// Body evaluated once `typed_ptr` is verified non-null and aligned for `#struct_ident`.
    pub(crate) valid_body: TokenStream,
}

/// Unified code-generation specification for one struct field.
pub(crate) struct AccessorSpec {
    /// Human-readable category name for duplicate-slot diagnostics.
    pub(crate) collision_label: &'static str,
    /// Whether this field permits `RawInsertable` on the enclosing struct.
    pub(crate) allows_raw_insertable: bool,
    /// Whether this field permits cross-language `AbiPod` on the enclosing struct.
    pub(crate) allows_abi_pod: bool,
    /// Compile-time type/trait assertions emitted inside `const _: () = { ... };`.
    pub(crate) assertions: Vec<TokenStream>,
    /// Optional `AbiPod` assertion emitted only when the enclosing struct implements `AbiPod`.
    pub(crate) pod_assertion: Option<TokenStream>,
    /// Ordered VTable slots emitted for this field.
    pub(crate) slots: Vec<SlotSpec>,
}

pub(crate) struct CodegenState {
    pub(crate) sys_crate: TokenStream,
    pub(crate) struct_ident: Ident,
    pub(crate) canonical_name: CanonicalNameSpec,
    pub(crate) export_fn_prefix: String,
    pub(crate) export_symbols: bool,
    pub(crate) opaque: bool,
    pub(crate) can_impl_raw_insertable: bool,
    pub(crate) can_impl_abi_pod: bool,
    pub(crate) component_flags_expr: TokenStream,
    pub(crate) custom_uuid: Option<String>,
    pub(crate) schema_fields: Vec<(String, String, TokenStream)>,
    pub(crate) decl_fields: Vec<c_decl::DeclFieldInfo>,
    pub(crate) extern_fns: Vec<TokenStream>,
    pub(crate) vtable_fields: Vec<TokenStream>,
    pub(crate) vtable_inits: Vec<TokenStream>,
    pub(crate) symbol_entries: Vec<TokenStream>,
    pub(crate) layout_assertions: Vec<TokenStream>,
    pub(crate) nested_pod_assertions: Vec<TokenStream>,
    pub(crate) used_vtable_fields: HashSet<String>,
    pub(crate) used_export_names: HashSet<String>,
}

impl CodegenState {
    /// Single unified driver emitting `extern "C"` functions, null/alignment guards, VTable field
    /// declarations, static VTable initializers, and `"bam.v0.*"` symbol table entries from an
    /// [`AccessorSpec`].
    pub(crate) fn emit_accessor(
        &mut self,
        field: &Field,
        field_name: &str,
        spec: AccessorSpec,
    ) -> syn::Result<()> {
        self.can_impl_raw_insertable &= spec.allows_raw_insertable;
        self.can_impl_abi_pod &= spec.allows_abi_pod;
        self.layout_assertions.extend(spec.assertions);
        if let Some(pod_assert) = spec.pod_assertion {
            self.nested_pod_assertions.push(pod_assert);
        }

        let sys_crate = self.sys_crate.clone();
        let struct_ident = self.struct_ident.clone();

        for slot in spec.slots {
            let vt_slot_str = format!("{}_{field_name}", slot.prefix);
            let export_sym_str = format!("{}_{vt_slot_str}", self.export_fn_prefix);

            if !self.used_vtable_fields.insert(vt_slot_str.clone())
                || !self.used_export_names.insert(export_sym_str.clone())
            {
                return Err(syn::Error::new_spanned(
                    field,
                    format!(
                        "field `{field_name}` produces a colliding {} name (`{vt_slot_str}` / `{export_sym_str}`)",
                        spec.collision_label
                    ),
                ));
            }

            let vt_slot_ident = Ident::new(&vt_slot_str, Span::call_site());
            let fn_ident = format_ident!("__bam_{}_{}", struct_ident, vt_slot_str);

            let export_attr = if self.export_symbols {
                quote! {
                    #[allow(unreachable_pub)]
                    #[unsafe(export_name = #export_sym_str)]
                    pub
                }
            } else {
                quote! {}
            };

            let extra_params_fn = &slot.extra_params_fn;
            let extra_params_vt = &slot.extra_params_vt;
            let ret_ty = &slot.ret_ty;
            let valid_body = &slot.valid_body;

            let (recv_param_fn, recv_param_vt, fn_preamble) = match &slot.receiver {
                ReceiverKind::Const { on_invalid } => (
                    quote! { ptr: *const ::core::ffi::c_void },
                    quote! { *const ::core::ffi::c_void },
                    quote! {
                        if ptr.is_null() {
                            return #on_invalid;
                        }
                        let typed_ptr = ptr.cast::<#struct_ident>();
                        if !typed_ptr.is_aligned() {
                            return #on_invalid;
                        }
                    },
                ),
                ReceiverKind::Mut {
                    on_null,
                    pre_align_check,
                    on_unaligned,
                } => {
                    let pre_check = pre_align_check.as_ref().cloned().unwrap_or_default();
                    (
                        quote! { ptr: *mut ::core::ffi::c_void },
                        quote! { *mut ::core::ffi::c_void },
                        quote! {
                            if ptr.is_null() {
                                return #on_null;
                            }
                            #pre_check
                            let typed_ptr = ptr.cast::<#struct_ident>();
                            if !typed_ptr.is_aligned() {
                                return #on_unaligned;
                            }
                        },
                    )
                }
            };

            self.extern_fns.push(quote! {
                #[allow(
                    non_snake_case,
                    clippy::undocumented_unsafe_blocks,
                    clippy::ptr_as_ptr
                )]
                #export_attr
                unsafe extern "C" fn #fn_ident(
                    #recv_param_fn
                    #extra_params_fn
                ) -> #ret_ty {
                    #fn_preamble
                    #valid_body
                }
            });

            let vt_slot_doc =
                format!("Generated C-ABI `{vt_slot_str}` accessor slot for `{struct_ident}`.");
            self.vtable_fields.push(quote! {
                #[doc = #vt_slot_doc]
                pub #vt_slot_ident: ::core::option::Option<
                    unsafe extern "C" fn(#recv_param_vt #extra_params_vt) -> #ret_ty
                >,
            });

            self.vtable_inits.push(quote! {
                #vt_slot_ident: ::core::option::Option::Some(#fn_ident),
            });

            let sym_name_expr = self.canonical_name.symbol_name_expr(&vt_slot_str);
            self.symbol_entries.push(quote! {
                #sys_crate::SymbolEntry {
                    name: #sym_name_expr,
                    ptr: #fn_ident as *const ::core::ffi::c_void,
                }
            });
        }

        Ok(())
    }
}

/// Core expansion entry point for `#[derive(ModComponent)]`.
pub(crate) fn expand_mod_component(input: &DeriveInput) -> syn::Result<TokenStream> {
    let current_crate = std::env::var("CARGO_CRATE_NAME").ok();
    expand_mod_component_with_crate(input, current_crate.as_deref())
}

pub(crate) fn expand_mod_component_with_crate(
    input: &DeriveInput,
    current_crate_name: Option<&str>,
) -> syn::Result<TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(ModComponent)]` does not support generic structs",
        ));
    }

    let comp_attrs = parse_component_attrs(input)?;
    let repr_flags = parse_repr_flags(&input.attrs)?;
    if repr_flags.has_packed {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(ModComponent)]` cannot be applied to `#[repr(packed)]` structs because aligned field pointers and references are undefined behavior",
        ));
    }

    let fields = match &input.data {
        Data::Struct(data_struct) => &data_struct.fields,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "`#[derive(ModComponent)]` can only be applied to structs",
            ));
        }
    };

    let sys_crate = resolve_sys_crate_path(comp_attrs.crate_override.as_ref());
    let struct_ident = input.ident.clone();
    let struct_ident_str = struct_ident.to_string();
    let vtable_ident = format_ident!("{}VTable", struct_ident);

    let (canonical_name, default_export_stem) = match (
        comp_attrs.custom_namespace.as_deref(),
        comp_attrs.custom_name.as_deref(),
    ) {
        (Some(ns), Some(short)) => (
            CanonicalNameSpec::Literal(format!("{ns}::{short}")),
            format!(
                "bam_{}_{}",
                sanitize_symbol_segment(ns),
                sanitize_symbol_segment(short)
            ),
        ),
        (Some(ns), None) => (
            CanonicalNameSpec::Literal(format!("{ns}::{struct_ident_str}")),
            format!(
                "bam_{}_{}",
                sanitize_symbol_segment(ns),
                sanitize_symbol_segment(&struct_ident_str)
            ),
        ),
        (None, Some(name)) if name.contains("::") => (
            CanonicalNameSpec::Literal(name.to_owned()),
            format!("bam_{}", sanitize_symbol_segment(name)),
        ),
        (None, Some(short)) => {
            let crate_seg = sanitize_symbol_segment(current_crate_name.unwrap_or("host"));
            (
                CanonicalNameSpec::ModulePathPrefixed {
                    short_name: short.to_owned(),
                },
                format!("bam_{crate_seg}_{}", sanitize_symbol_segment(short)),
            )
        }
        (None, None) => {
            let crate_seg = sanitize_symbol_segment(current_crate_name.unwrap_or("host"));
            (
                CanonicalNameSpec::ModulePathPrefixed {
                    short_name: struct_ident_str.clone(),
                },
                format!(
                    "bam_{crate_seg}_{}",
                    sanitize_symbol_segment(&struct_ident_str)
                ),
            )
        }
    };

    let export_fn_prefix = match comp_attrs.custom_prefix.as_deref() {
        Some(custom) => {
            let san = sanitize_symbol_segment(custom);
            if san.starts_with("bam_") {
                san
            } else {
                format!("bam_{san}")
            }
        }
        None => default_export_stem,
    };

    let component_flags_expr = match (comp_attrs.sparse_set, comp_attrs.immutable) {
        (false, false) => quote! { 0u32 },
        (true, false) => quote! { #sys_crate::BAM_COMPONENT_FLAG_SPARSE_SET },
        (false, true) => quote! { #sys_crate::BAM_COMPONENT_FLAG_IMMUTABLE },
        (true, true) => {
            quote! { #sys_crate::BAM_COMPONENT_FLAG_SPARSE_SET | #sys_crate::BAM_COMPONENT_FLAG_IMMUTABLE }
        }
    };

    let mut state = CodegenState {
        sys_crate: sys_crate.clone(),
        struct_ident,
        canonical_name,
        export_fn_prefix,
        export_symbols: comp_attrs.export_symbols,
        opaque: comp_attrs.opaque,
        can_impl_raw_insertable: !comp_attrs.opaque,
        can_impl_abi_pod: !comp_attrs.opaque
            && repr_flags.is_deterministic_unpacked()
            && !comp_attrs.no_pod,
        component_flags_expr,
        custom_uuid: comp_attrs.custom_uuid,
        schema_fields: Vec::new(),
        decl_fields: Vec::new(),
        extern_fns: Vec::new(),
        vtable_fields: Vec::new(),
        vtable_inits: Vec::new(),
        symbol_entries: Vec::new(),
        layout_assertions: Vec::new(),
        nested_pod_assertions: Vec::new(),
        used_vtable_fields: HashSet::new(),
        used_export_names: HashSet::new(),
    };

    // Reserve `header` on `<Struct>VTable` so a field cannot overwrite it (`get_`/`set_`/`ptr_`
    // prefixes already prevent collisions, recorded explicitly for defense in depth).
    state.used_vtable_fields.insert("header".to_owned());

    let vtable_sym_expr = state.canonical_name.symbol_name_expr("vtable");
    state.symbol_entries.push(quote! {
        #sys_crate::SymbolEntry {
            name: #vtable_sym_expr,
            ptr: (&raw const VTABLE_INSTANCE).cast::<::core::ffi::c_void>(),
        }
    });

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

        if !matches!(kind, FieldKind::Skip) {
            state.schema_fields.push((
                field_name.clone(),
                canonical_field_type_tag(&field.ty),
                member_tokens.clone(),
            ));
            if let Some(decl_info) =
                c_decl::DeclFieldInfo::from_classified(&field_name, &kind, &field.ty)
            {
                state.decl_fields.push(decl_info);
            }
        }

        let spec = match kind {
            FieldKind::Skip => {
                state.can_impl_raw_insertable = false;
                state.can_impl_abi_pod = false;
                continue;
            }
            FieldKind::Bool(bool_ty) => {
                bool::spec(&sys_crate, &member_tokens, &bool_ty, field_span)
            }
            FieldKind::Primitive { ty, is_abi_pod } => {
                primitive::spec(&sys_crate, &member_tokens, &ty, is_abi_pod, field_span)
            }
            FieldKind::HostString(str_ty) => {
                string::spec(&sys_crate, &member_tokens, &str_ty, field_span)
            }
            FieldKind::HostVec { field_ty, elem_ty } => {
                vec::spec(&sys_crate, &member_tokens, &field_ty, &elem_ty, field_span)
            }
            FieldKind::Nested {
                ty,
                is_abi_pod_candidate,
            } => nested::spec(
                &sys_crate,
                state.opaque,
                &member_tokens,
                &ty,
                is_abi_pod_candidate,
                field_span,
            ),
        };

        state.emit_accessor(field, &field_name, spec)?;
    }

    Ok(vtable::emit_vtable_and_impls(&state, &vtable_ident))
}
