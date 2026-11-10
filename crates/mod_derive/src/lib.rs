#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `mod_derive`: Procedural Macro for `#[derive(ModExposable)]`
//!
//! This crate provides the `#[derive(ModExposable)]` derive macro for host Bevy components
//! and compound types. It generates C-ABI field accessors, a `#[repr(C)]` `<Struct>VTable`
//! with static byte strides and function pointers, compile-time layout assertions, and an
//! implementation of [`abi_contract::ExposableComponent`].
//!
//! ## Macro Compilation Pipeline
//!
//! ```text
//! Rust Struct (#[derive(ModExposable)])
//!         │
//!         ▼
//!    AST Parsing & Validation
//!    ├── Reject generic structs
//!    ├── Reject enums
//!    ├── Reject unsupported heap-owning types without #[mod_field(skip)]
//!    └── Reject colliding symbol or accessor names
//!         │
//!         ▼
//!    Field Classification (classify_type)
//!    ├── Primitive: f32, f64, i8..i64, u8..u64, usize, isize, bool
//!    ├── HostString: String (Host-allocated)
//!    ├── HostVec: Vec<T: FfiLayout + Copy> (Host-allocated)
//!    └── Nested: Compound struct / array (Interior pointer accessors)
//!         │
//!         ▼
//!    Code Generation (expand_mod_exposable)
//!    ├── Extern "C" field accessors (<Prefix>_get_<field>, <Prefix>_set_<field>)
//!    │   ├── Self-aliasing guards for String and Vec setters
//!    │   └── Zero-cost null-pointer checks
//!    ├── VTable Struct Definition (<Struct>VTable { size, align, ... })
//!    ├── Static VTable Instance & Symbol Registration Table
//!    ├── Compile-Time Layout Assertions (assert_primitive_ffi, assert_vec_elem_ffi, assert_nested_ffi)
//!    └── Trait Implementation (impl ExposableComponent for <Struct>)
//! ```
//!
//! ## Attribute Reference
//!
//! ### Struct-Level Attributes (`#[mod_component(...)]`)
//! - `#[mod_component(name = "...")]`: Specifies a custom component name (e.g. `"game::Position"`),
//!   disambiguating identically named structs across modules.
//! - `#[mod_component(prefix = "...")]`: Custom symbol prefix. Replaces non-alphanumeric characters with `_`.
//! - `#[mod_component(export_symbols)]`: Emits `#[no_mangle]` on accessors for direct `.cdylib` export.
//!
//! ### Field-Level Attributes (`#[mod_field(...)]`)
//! - `#[mod_field(skip)]`: Skips field accessor generation (mandatory for host-private fields, closures,
//!   raw pointers, or Rust collections like `Option<T>`, `Box<T>`, `HashMap<K, V>`).
//! - `#[mod_field(primitive)]`: Forces classification as primitive value type.
//! - `#[mod_field(string)]`: Forces classification as host-owned `String`.
//! - `#[mod_field(vec)]`: Forces classification as host-owned `Vec<T>`.
//! - `#[mod_field(nested)]`: Forces classification as nested compound struct exposed via interior pointers
//!   (`ptr_<field>` and `ptr_mut_<field>`).
//!
//! ## The Self-Aliasing Problem & Defensive Resolution
//!
//! When guest code invokes a setter for a host-allocated collection (`String` or `Vec<T>`), it may pass
//! a pointer to memory that resides inside the component's own existing heap buffer:
//!
//! ```text
//! Component Buffer: [ "Hello World" ]
//!                     ▲       ▲
//!                     │       └── Guest passes subslice [6..11]: "World"
//!                     └────────── Existing buffer start
//! ```
//!
//! If the host setter naively called `field.clear(); field.push_str(incoming);`, clearing the buffer
//! would deallocate or truncate the memory before `push_str` reads from it, causing a use-after-free
//! or data corruption!
//!
//! To prevent this, the generated setters implement a **4-step self-aliasing detection algorithm**:
//! 1. **Buffer Address Extraction**: Read existing buffer range `[buf_start .. buf_end)`.
//! 2. **Input Address Calculation**: Calculate incoming pointer interval `[src_start .. src_end)`.
//! 3. **Interval Intersection Check**: `let overlaps = src_start < buf_end && buf_start < src_end;`.
//! 4. **Disjoint vs. Overlapping Branching**:
//!    - If `overlaps`: Allocate a fresh `String` / `Vec<T>` clone *before* mutating `*field_ptr`.
//!    - If non-overlapping: Safely update in-place with zero extra allocations.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote, quote_spanned};
use std::collections::HashSet;
use syn::{
    parse_macro_input, spanned::Spanned, Data, DeriveInput, Fields, GenericArgument, Ident, Index,
    LitByteStr, LitStr, PathArguments, Type,
};

/// Classification of a struct field for C-ABI accessor code generation.
enum FieldKind {
    /// Skip generating C-ABI accessors for this field (`#[mod_field(skip)]`).
    Skip,
    /// Copy primitive (`f32`, `f64`, `i8`..`i64`, `u8`..`u64`, `usize`, `isize`, `bool`).
    Primitive(Type),
    /// Heap-allocated `String` owned by the host allocator.
    HostString,
    /// Heap-allocated `Vec<T>` owned by the host allocator (`T: FfiLayout + Copy`).
    HostVec(Type),
    /// Nested compound struct or fixed array (`T: FfiLayout`) exposed via interior pointer accessors.
    Nested(Type),
}

/// Parsed container-level `#[mod_component(...)]` attributes.
struct ComponentAttrs {
    /// Custom component name override.
    custom_name: Option<String>,
    /// Custom symbol prefix override.
    custom_prefix: Option<String>,
    /// Whether to emit `#[no_mangle]` on accessors.
    export_symbols: bool,
}

/// Parses container-level `#[mod_component(...)]` attributes from a derive input.
fn parse_component_attrs(input: &DeriveInput) -> syn::Result<ComponentAttrs> {
    let mut attrs = ComponentAttrs {
        custom_name: None,
        custom_prefix: None,
        export_symbols: false,
    };

    for attr in &input.attrs {
        if !attr.path().is_ident("mod_component") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                let value = meta.value()?;
                let s: LitStr = value.parse()?;
                attrs.custom_name = Some(s.value());
                Ok(())
            } else if meta.path.is_ident("prefix") {
                let value = meta.value()?;
                let s: LitStr = value.parse()?;
                attrs.custom_prefix = Some(s.value());
                Ok(())
            } else if meta.path.is_ident("export_symbols") {
                attrs.export_symbols = true;
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[mod_component(...)]` attribute; expected `name = \"...\"`, `prefix = \"...\"`, or `export_symbols`",
                ))
            }
        })?;
    }

    Ok(attrs)
}

/// Field-level override specified via `#[mod_field(...)]`.
enum FieldAttrOverride {
    None,
    Skip,
    Primitive,
    HostString,
    HostVec,
    Nested,
}

/// Parses field-level `#[mod_field(...)]` attributes.
fn parse_field_attr(attrs: &[syn::Attribute]) -> syn::Result<FieldAttrOverride> {
    let mut result = FieldAttrOverride::None;
    for attr in attrs {
        if !attr.path().is_ident("mod_field") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                result = FieldAttrOverride::Skip;
                Ok(())
            } else if meta.path.is_ident("primitive") {
                result = FieldAttrOverride::Primitive;
                Ok(())
            } else if meta.path.is_ident("string") {
                result = FieldAttrOverride::HostString;
                Ok(())
            } else if meta.path.is_ident("vec") {
                result = FieldAttrOverride::HostVec;
                Ok(())
            } else if meta.path.is_ident("nested") {
                result = FieldAttrOverride::Nested;
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[mod_field(...)]` attribute; expected `skip`, `primitive`, `string`, `vec`, or `nested`",
                ))
            }
        })?;
    }
    Ok(result)
}

/// Classifies a field type into primitive, string, vector, or nested representation.
fn classify_type(ty: &Type, attr_override: FieldAttrOverride) -> syn::Result<FieldKind> {
    match attr_override {
        FieldAttrOverride::Skip => return Ok(FieldKind::Skip),
        FieldAttrOverride::Primitive => return Ok(FieldKind::Primitive(ty.clone())),
        FieldAttrOverride::HostString => return Ok(FieldKind::HostString),
        FieldAttrOverride::Nested => return Ok(FieldKind::Nested(ty.clone())),
        FieldAttrOverride::HostVec => {
            if let Type::Path(type_path) = ty {
                if let Some(last_seg) = type_path.path.segments.last() {
                    if let PathArguments::AngleBracketed(args) = &last_seg.arguments {
                        if let Some(GenericArgument::Type(elem_ty)) = args.args.first() {
                            return Ok(FieldKind::HostVec(elem_ty.clone()));
                        }
                    }
                }
            }
            return Err(syn::Error::new_spanned(
                ty,
                "`#[mod_field(vec)]` requires a `Vec<T>` field type",
            ));
        }
        FieldAttrOverride::None => {}
    }

    match ty {
        Type::Reference(_) | Type::Ptr(_) | Type::Slice(_) => {
            return Err(syn::Error::new_spanned(
                ty,
                "references, raw pointers, and unsized slices cannot be exposed across the C-ABI; mark with `#[mod_field(skip)]` if host-private",
            ));
        }
        Type::Path(type_path) if type_path.qself.is_none() => {
            if let Some(last_seg) = type_path.path.segments.last() {
                let ident_str = last_seg.ident.to_string();
                match (&last_seg.arguments, ident_str.as_str()) {
                    (
                        PathArguments::None,
                        "f32" | "f64" | "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16"
                        | "u32" | "u64" | "usize" | "bool",
                    ) => {
                        return Ok(FieldKind::Primitive(ty.clone()));
                    }
                    (PathArguments::None, "String") => {
                        return Ok(FieldKind::HostString);
                    }
                    (PathArguments::AngleBracketed(args), "Vec") => {
                        if args.args.len() == 1 {
                            if let Some(GenericArgument::Type(elem_ty)) = args.args.first() {
                                return Ok(FieldKind::HostVec(elem_ty.clone()));
                            }
                        }
                        return Err(syn::Error::new_spanned(
                            ty,
                            "unsupported `Vec` type arguments for `ModExposable`",
                        ));
                    }
                    (
                        _,
                        "Box" | "Rc" | "Arc" | "Option" | "Result" | "HashMap" | "BTreeMap"
                        | "HashSet" | "BTreeSet" | "Cow" | "VecDeque",
                    ) => {
                        return Err(syn::Error::new_spanned(
                            ty,
                            format!(
                                "`{ident_str}` owns heap/Rust-layout state and cannot be exposed as a raw nested pointer; use `#[mod_field(skip)]` or a C-ABI safe field type"
                            ),
                        ));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    Ok(FieldKind::Nested(ty.clone()))
}

/// Sanitizes a string into a valid C identifier prefix.
fn sanitize_symbol_prefix(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

/// Derive macro for `ModExposable` components.
#[proc_macro_derive(ModExposable, attributes(mod_component, mod_field))]
pub fn derive_mod_exposable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_mod_exposable(&input) {
        Ok(ts) => ts.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// Core expansion function for `#[derive(ModExposable)]`.
fn expand_mod_exposable(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`#[derive(ModExposable)]` does not support generic structs",
        ));
    }

    let comp_attrs = parse_component_attrs(input)?;
    let struct_ident = &input.ident;
    let struct_ident_str = struct_ident.to_string();
    let component_name_str = comp_attrs
        .custom_name
        .clone()
        .unwrap_or_else(|| struct_ident_str.clone());
    let fn_prefix = comp_attrs.custom_prefix.unwrap_or_else(|| {
        if comp_attrs.custom_name.is_some() {
            sanitize_symbol_prefix(&component_name_str)
        } else {
            struct_ident_str.clone()
        }
    });

    let no_mangle_attr = if comp_attrs.export_symbols {
        quote! { #[no_mangle] }
    } else {
        quote! {}
    };

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

    let mut extern_fns = Vec::new();
    let mut vtable_fields = Vec::new();
    let mut vtable_inits = Vec::new();
    let mut symbol_entries = Vec::new();
    let mut layout_assertions = Vec::new();
    let mut used_vtable_fields = HashSet::<String>::new();
    let mut used_symbol_names = HashSet::<String>::new();

    // Register `<Prefix>_vtable` and `<ComponentName>` symbols pointing to the single `static VTABLE_INSTANCE`.
    let vtable_sym_name = format!("{}_vtable", fn_prefix);
    used_symbol_names.insert(component_name_str.clone());
    used_symbol_names.insert(vtable_sym_name.clone());

    symbol_entries.push(quote! {
        ::abi_contract::SymbolEntry {
            name: #component_name_str,
            ptr: &VTABLE_INSTANCE as *const #vtable_ident as *const ::core::ffi::c_void,
        },
        ::abi_contract::SymbolEntry {
            name: #vtable_sym_name,
            ptr: &VTABLE_INSTANCE as *const #vtable_ident as *const ::core::ffi::c_void,
        }
    });

    if comp_attrs.custom_name.is_some() && component_name_str != struct_ident_str {
        let short_vtable_sym = format!("{}_vtable", struct_ident_str);
        if used_symbol_names.insert(short_vtable_sym.clone()) {
            symbol_entries.push(quote! {
                ::abi_contract::SymbolEntry {
                    name: #short_vtable_sym,
                    ptr: &VTABLE_INSTANCE as *const #vtable_ident as *const ::core::ffi::c_void,
                }
            });
        }
    }

    let field_iter: Vec<(String, proc_macro2::TokenStream, &syn::Field)> = match fields {
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
            FieldKind::Skip => continue,
            FieldKind::Primitive(prim_ty) => {
                let vt_get_str = format!("get_{}", field_name);
                let vt_set_str = format!("set_{}", field_name);
                let fn_get_str = format!("{}_get_{}", fn_prefix, field_name);
                let fn_set_str = format!("{}_set_{}", fn_prefix, field_name);

                if !used_vtable_fields.insert(vt_get_str.clone())
                    || !used_vtable_fields.insert(vt_set_str.clone())
                    || !used_symbol_names.insert(fn_get_str.clone())
                    || !used_symbol_names.insert(fn_set_str.clone())
                {
                    return Err(syn::Error::new_spanned(
                        field,
                        format!(
                            "field `{field_name}` produces a colliding accessor name (`{fn_get_str}` / `{fn_set_str}`)"
                        ),
                    ));
                }

                let fn_get_ident = Ident::new(&fn_get_str, Span::call_site());
                let fn_set_ident = Ident::new(&fn_set_str, Span::call_site());
                let vt_get_ident = Ident::new(&vt_get_str, Span::call_site());
                let vt_set_ident = Ident::new(&vt_set_str, Span::call_site());

                layout_assertions.push(quote_spanned! { field_span =>
                    const _: fn() = || {
                        fn assert_primitive_ffi<T: ::abi_contract::FfiLayout + ::core::marker::Copy + ::core::default::Default>() {}
                        assert_primitive_ffi::<#prim_ty>();
                    };
                });

                extern_fns.push(quote! {
                    /// Read primitive field `#field_name` from `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an initialized `#struct_ident`.
                    #no_mangle_attr
                    pub unsafe extern "C" fn #fn_get_ident(
                        ptr: *const ::core::ffi::c_void,
                    ) -> #prim_ty {
                        if ptr.is_null() {
                            return <#prim_ty as ::core::default::Default>::default();
                        }
                        let typed_ptr = ptr as *const #struct_ident;
                        ::core::debug_assert!(
                            (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                            "unaligned component pointer passed to getter"
                        );
                        // SAFETY: Caller guarantees `ptr` points to a valid, aligned `#struct_ident`
                        // and we verified `ptr` is non-null above.
                        unsafe {
                            ::core::ptr::read(::core::ptr::addr_of!((*typed_ptr).#member_tokens))
                        }
                    }

                    /// Write primitive field `#field_name` on `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an exclusively borrowed `#struct_ident`.
                    #no_mangle_attr
                    pub unsafe extern "C" fn #fn_set_ident(
                        ptr: *mut ::core::ffi::c_void,
                        val: #prim_ty,
                    ) {
                        if ptr.is_null() {
                            return;
                        }
                        let typed_ptr = ptr as *mut #struct_ident;
                        ::core::debug_assert!(
                            (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                            "unaligned component pointer passed to setter"
                        );
                        // SAFETY: Caller guarantees `ptr` points to a valid, aligned, mutable `#struct_ident`
                        // and we verified `ptr` is non-null above.
                        unsafe {
                            ::core::ptr::write(
                                ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens),
                                val,
                            );
                        }
                    }
                });

                vtable_fields.push(quote! {
                    pub #vt_get_ident: unsafe extern "C" fn(*const ::core::ffi::c_void) -> #prim_ty,
                    pub #vt_set_ident: unsafe extern "C" fn(*mut ::core::ffi::c_void, #prim_ty),
                });

                vtable_inits.push(quote! {
                    #vt_get_ident: #fn_get_ident,
                    #vt_set_ident: #fn_set_ident,
                });

                symbol_entries.push(quote! {
                    ::abi_contract::SymbolEntry {
                        name: #fn_get_str,
                        ptr: #fn_get_ident as *const ::core::ffi::c_void,
                    },
                    ::abi_contract::SymbolEntry {
                        name: #fn_set_str,
                        ptr: #fn_set_ident as *const ::core::ffi::c_void,
                    }
                });
            }
            FieldKind::HostString => {
                let vt_get_str = format!("get_{}", field_name);
                let vt_set_str = format!("set_{}", field_name);
                let fn_get_str = format!("{}_get_{}", fn_prefix, field_name);
                let fn_set_str = format!("{}_set_{}", fn_prefix, field_name);

                if !used_vtable_fields.insert(vt_get_str.clone())
                    || !used_vtable_fields.insert(vt_set_str.clone())
                    || !used_symbol_names.insert(fn_get_str.clone())
                    || !used_symbol_names.insert(fn_set_str.clone())
                {
                    return Err(syn::Error::new_spanned(
                        field,
                        format!(
                            "field `{field_name}` produces a colliding accessor name (`{fn_get_str}` / `{fn_set_str}`)"
                        ),
                    ));
                }

                let fn_get_ident = Ident::new(&fn_get_str, Span::call_site());
                let fn_set_ident = Ident::new(&fn_set_str, Span::call_site());
                let vt_get_ident = Ident::new(&vt_get_str, Span::call_site());
                let vt_set_ident = Ident::new(&vt_set_str, Span::call_site());

                extern_fns.push(quote! {
                    /// Borrow UTF-8 bytes of `String` field `#field_name` on `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an initialized `#struct_ident`.
                    /// The returned `FfiSlice<u8>` is valid only until the next mutation of this field.
                    #no_mangle_attr
                    pub unsafe extern "C" fn #fn_get_ident(
                        ptr: *const ::core::ffi::c_void,
                    ) -> ::abi_contract::FfiSlice<u8> {
                        if ptr.is_null() {
                            return ::abi_contract::FfiSlice::empty();
                        }
                        let typed_ptr = ptr as *const #struct_ident;
                        ::core::debug_assert!(
                            (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                            "unaligned component pointer passed to string getter"
                        );
                        // SAFETY: Caller guarantees `ptr` points to a valid `#struct_ident` instance
                        // whose `String` field remains alive for the borrow duration.
                        let field_ref: &::std::string::String =
                            unsafe { &*(::core::ptr::addr_of!((*typed_ptr).#member_tokens)) };
                        ::abi_contract::FfiSlice::from_str(field_ref.as_str())
                    }

                    /// Mutate `String` field `#field_name` on `#struct_ident` using the host's heap allocator.
                    ///
                    /// Validates non-null pointers, UTF-8 encoding, and self-aliasing overlap before mutating.
                    ///
                    /// # Safety
                    /// - `ptr` must be non-null, properly aligned, and point to a mutable `#struct_ident`.
                    /// - If `len > 0`, `utf8_ptr` must be non-null and point to `len` valid bytes.
                    #no_mangle_attr
                    pub unsafe extern "C" fn #fn_set_ident(
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
                        let typed_ptr = ptr as *mut #struct_ident;
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

                        // ── Self-Aliasing Detection Algorithm (Step 1-4) ──
                        // Check whether `[utf8_ptr .. utf8_ptr + len]` overlaps the existing `String` buffer.
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
                                // Overlapping: allocate fresh host-heap String BEFORE mutating the buffer
                                let replacement = ::std::string::String::from(valid_str);
                                // SAFETY: `valid_str` is no longer used after `replacement` is constructed;
                                // replacing `*field_ptr` drops the old buffer cleanly on the host allocator.
                                unsafe {
                                    *field_ptr = replacement;
                                }
                            } else {
                                // Disjoint: in-place clear and append with zero extra heap allocations
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

                vtable_fields.push(quote! {
                    pub #vt_get_ident: unsafe extern "C" fn(*const ::core::ffi::c_void) -> ::abi_contract::FfiSlice<u8>,
                    pub #vt_set_ident: unsafe extern "C" fn(*mut ::core::ffi::c_void, *const u8, usize),
                });

                vtable_inits.push(quote! {
                    #vt_get_ident: #fn_get_ident,
                    #vt_set_ident: #fn_set_ident,
                });

                symbol_entries.push(quote! {
                    ::abi_contract::SymbolEntry {
                        name: #fn_get_str,
                        ptr: #fn_get_ident as *const ::core::ffi::c_void,
                    },
                    ::abi_contract::SymbolEntry {
                        name: #fn_set_str,
                        ptr: #fn_set_ident as *const ::core::ffi::c_void,
                    }
                });
            }
            FieldKind::HostVec(elem_ty) => {
                let vt_get_str = format!("get_{}", field_name);
                let vt_set_str = format!("set_{}", field_name);
                let fn_get_str = format!("{}_get_{}", fn_prefix, field_name);
                let fn_set_str = format!("{}_set_{}", fn_prefix, field_name);

                if !used_vtable_fields.insert(vt_get_str.clone())
                    || !used_vtable_fields.insert(vt_set_str.clone())
                    || !used_symbol_names.insert(fn_get_str.clone())
                    || !used_symbol_names.insert(fn_set_str.clone())
                {
                    return Err(syn::Error::new_spanned(
                        field,
                        format!(
                            "field `{field_name}` produces a colliding accessor name (`{fn_get_str}` / `{fn_set_str}`)"
                        ),
                    ));
                }

                let fn_get_ident = Ident::new(&fn_get_str, Span::call_site());
                let fn_set_ident = Ident::new(&fn_set_str, Span::call_site());
                let vt_get_ident = Ident::new(&vt_get_str, Span::call_site());
                let vt_set_ident = Ident::new(&vt_set_str, Span::call_site());

                layout_assertions.push(quote_spanned! { field_span =>
                    const _: fn() = || {
                        fn assert_vec_elem_ffi<T: ::abi_contract::FfiLayout + ::core::marker::Copy>() {}
                        assert_vec_elem_ffi::<#elem_ty>();
                    };
                });

                extern_fns.push(quote! {
                    /// Borrow elements of `Vec` field `#field_name` on `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an initialized `#struct_ident`.
                    #no_mangle_attr
                    pub unsafe extern "C" fn #fn_get_ident(
                        ptr: *const ::core::ffi::c_void,
                    ) -> ::abi_contract::FfiSlice<#elem_ty> {
                        if ptr.is_null() {
                            return ::abi_contract::FfiSlice::empty();
                        }
                        let typed_ptr = ptr as *const #struct_ident;
                        ::core::debug_assert!(
                            (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                            "unaligned component pointer passed to vec getter"
                        );
                        // SAFETY: Caller guarantees `ptr` points to a valid `#struct_ident` instance.
                        let field_ref: &::std::vec::Vec<#elem_ty> =
                            unsafe { &*(::core::ptr::addr_of!((*typed_ptr).#member_tokens)) };
                        ::abi_contract::FfiSlice::from_slice(field_ref.as_slice())
                    }

                    /// Mutate `Vec` field `#field_name` on `#struct_ident` using the host's heap allocator.
                    ///
                    /// # Safety
                    /// - `ptr` must be non-null, properly aligned, and point to a mutable `#struct_ident`.
                    /// - If `len > 0`, `data_ptr` must be non-null, aligned, and point to `len` valid elements.
                    #no_mangle_attr
                    pub unsafe extern "C" fn #fn_set_ident(
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
                        let typed_ptr = ptr as *mut #struct_ident;
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

                        // ── Self-Aliasing Detection Algorithm for Vec<T> (Step 1-4) ──
                        // Check whether `[data_ptr .. data_ptr + len * size_of::<T>()]` overlaps existing buffer.
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
                            // Overlapping: clone elements into a fresh host-allocated Vec before mutating
                            let replacement: ::std::vec::Vec<#elem_ty> = slice.to_vec();
                            // SAFETY: `slice` is no longer accessed after `replacement` is allocated.
                            unsafe {
                                *field_ptr = replacement;
                            }
                        } else {
                            // Disjoint: clear and extend in-place with zero reallocations
                            // SAFETY: `slice` is disjoint from `*field_ptr`'s buffer.
                            unsafe {
                                let field_mut = &mut *field_ptr;
                                field_mut.clear();
                                field_mut.extend_from_slice(slice);
                            }
                        }
                    }
                });

                vtable_fields.push(quote! {
                    pub #vt_get_ident: unsafe extern "C" fn(*const ::core::ffi::c_void) -> ::abi_contract::FfiSlice<#elem_ty>,
                    pub #vt_set_ident: unsafe extern "C" fn(*mut ::core::ffi::c_void, *const #elem_ty, usize),
                });

                vtable_inits.push(quote! {
                    #vt_get_ident: #fn_get_ident,
                    #vt_set_ident: #fn_set_ident,
                });

                symbol_entries.push(quote! {
                    ::abi_contract::SymbolEntry {
                        name: #fn_get_str,
                        ptr: #fn_get_ident as *const ::core::ffi::c_void,
                    },
                    ::abi_contract::SymbolEntry {
                        name: #fn_set_str,
                        ptr: #fn_set_ident as *const ::core::ffi::c_void,
                    }
                });
            }
            FieldKind::Nested(nested_ty) => {
                let vt_ptr_str = format!("ptr_{}", field_name);
                let vt_ptr_mut_str = format!("ptr_mut_{}", field_name);
                let fn_ptr_str = format!("{}_ptr_{}", fn_prefix, field_name);
                let fn_ptr_mut_str = format!("{}_ptr_mut_{}", fn_prefix, field_name);

                if !used_vtable_fields.insert(vt_ptr_str.clone())
                    || !used_vtable_fields.insert(vt_ptr_mut_str.clone())
                    || !used_symbol_names.insert(fn_ptr_str.clone())
                    || !used_symbol_names.insert(fn_ptr_mut_str.clone())
                {
                    return Err(syn::Error::new_spanned(
                        field,
                        format!(
                            "field `{field_name}` produces a colliding interior-pointer accessor name (`{vt_ptr_str}` / `{vt_ptr_mut_str}`)"
                        ),
                    ));
                }

                let fn_ptr_ident = Ident::new(&fn_ptr_str, Span::call_site());
                let fn_ptr_mut_ident = Ident::new(&fn_ptr_mut_str, Span::call_site());
                let vt_ptr_ident = Ident::new(&vt_ptr_str, Span::call_site());
                let vt_ptr_mut_ident = Ident::new(&vt_ptr_mut_str, Span::call_site());

                layout_assertions.push(quote_spanned! { field_span =>
                    const _: fn() = || {
                        fn assert_nested_ffi<T: ::abi_contract::FfiLayout>() {}
                        assert_nested_ffi::<#nested_ty>();
                    };
                });

                extern_fns.push(quote! {
                    /// Obtain a read-only interior pointer to nested field `#field_name` on `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an initialized `#struct_ident`.
                    #no_mangle_attr
                    pub unsafe extern "C" fn #fn_ptr_ident(
                        ptr: *const ::core::ffi::c_void,
                    ) -> *const ::core::ffi::c_void {
                        if ptr.is_null() {
                            return ::core::ptr::null();
                        }
                        let typed_ptr = ptr as *const #struct_ident;
                        ::core::debug_assert!(
                            (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                            "unaligned component pointer passed to nested ptr getter"
                        );
                        // SAFETY: `typed_ptr` is verified non-null and points to a valid `#struct_ident`;
                        // `addr_of!` computes the interior field address without creating a reference.
                        unsafe {
                            ::core::ptr::addr_of!((*typed_ptr).#member_tokens)
                                as *const ::core::ffi::c_void
                        }
                    }

                    /// Obtain a mutable interior pointer to nested field `#field_name` on `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an exclusively borrowed `#struct_ident`.
                    #no_mangle_attr
                    pub unsafe extern "C" fn #fn_ptr_mut_ident(
                        ptr: *mut ::core::ffi::c_void,
                    ) -> *mut ::core::ffi::c_void {
                        if ptr.is_null() {
                            return ::core::ptr::null_mut();
                        }
                        let typed_ptr = ptr as *mut #struct_ident;
                        ::core::debug_assert!(
                            (typed_ptr as usize) % ::core::mem::align_of::<#struct_ident>() == 0,
                            "unaligned component pointer passed to nested ptr_mut getter"
                        );
                        // SAFETY: `typed_ptr` is verified non-null and points to a valid mutable `#struct_ident`;
                        // `addr_of_mut!` computes the mutable interior pointer without aliasing violations.
                        unsafe {
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens)
                                as *mut ::core::ffi::c_void
                        }
                    }
                });

                vtable_fields.push(quote! {
                    pub #vt_ptr_ident: unsafe extern "C" fn(*const ::core::ffi::c_void) -> *const ::core::ffi::c_void,
                    pub #vt_ptr_mut_ident: unsafe extern "C" fn(*mut ::core::ffi::c_void) -> *mut ::core::ffi::c_void,
                });

                vtable_inits.push(quote! {
                    #vt_ptr_ident: #fn_ptr_ident,
                    #vt_ptr_mut_ident: #fn_ptr_mut_ident,
                });

                symbol_entries.push(quote! {
                    ::abi_contract::SymbolEntry {
                        name: #fn_ptr_str,
                        ptr: #fn_ptr_ident as *const ::core::ffi::c_void,
                    },
                    ::abi_contract::SymbolEntry {
                        name: #fn_ptr_mut_str,
                        ptr: #fn_ptr_mut_ident as *const ::core::ffi::c_void,
                    }
                });
            }
        }
    }

    let expanded = quote! {
        #(#layout_assertions)*
        #(#extern_fns)*

        /// C-ABI VTable for `#struct_ident`, containing exact byte stride (`size`),
        /// alignment (`align`), and field accessor function pointers.
        #[repr(C)]
        #[derive(Debug, Copy, Clone)]
        pub struct #vtable_ident {
            pub size: usize,
            pub align: usize,
            #(#vtable_fields)*
        }

        impl #struct_ident {
            pub const MOD_COMPONENT_NAME: &'static str = #component_name_str;

            pub const VTABLE: #vtable_ident = #vtable_ident {
                size: ::core::mem::size_of::<#struct_ident>(),
                align: ::core::mem::align_of::<#struct_ident>(),
                #(#vtable_inits)*
            };

            #[inline]
            pub fn vtable() -> &'static #vtable_ident {
                <#struct_ident as ::abi_contract::ExposableComponent>::vtable()
            }
        }

        // SAFETY: `#struct_ident` derives `ModExposable` and is accessed across the C-ABI strictly
        // through its generated VTable accessors.
        unsafe impl ::abi_contract::FfiLayout for #struct_ident {}

        const _: () = {
            static VTABLE_INSTANCE: #vtable_ident = #struct_ident::VTABLE;

            static SYMBOLS: &[::abi_contract::SymbolEntry] = &[
                #(#symbol_entries),*
            ];

            impl ::abi_contract::ExposableComponent for #struct_ident {
                const COMPONENT_NAME: &'static str = #component_name_str;
                const COMPONENT_CSTR: &'static ::core::ffi::CStr =
                    match ::core::ffi::CStr::from_bytes_with_nul(#cstr_lit) {
                        ::core::result::Result::Ok(s) => s,
                        ::core::result::Result::Err(_) => panic!("invalid CStr literal"),
                    };
                const UUID: ::abi_contract::ComponentUuid =
                    ::abi_contract::ComponentUuid::from_name(#component_name_str);

                type VTable = #vtable_ident;

                #[inline]
                fn vtable() -> &'static Self::VTable {
                    &VTABLE_INSTANCE
                }

                #[inline]
                fn symbols() -> &'static [::abi_contract::SymbolEntry] {
                    SYMBOLS
                }
            }
        };
    };

    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn test_rejects_generic_structs() {
        let input: DeriveInput = parse_quote! {
            pub struct GenericComp<T> {
                pub value: T,
            }
        };
        let err = expand_mod_exposable(&input).unwrap_err();
        assert!(err.to_string().contains("does not support generic structs"));
    }

    #[test]
    fn test_rejects_enums() {
        let input: DeriveInput = parse_quote! {
            pub enum StateComp {
                Idle,
                Running,
            }
        };
        let err = expand_mod_exposable(&input).unwrap_err();
        assert!(err.to_string().contains("can only be applied to structs"));
    }

    #[test]
    fn test_rejects_nested_field_name_collision() {
        let input: DeriveInput = parse_quote! {
            pub struct CollidingNested {
                pub x: SubVec,
                pub mut_x: SubVec,
            }
        };
        let err = expand_mod_exposable(&input).unwrap_err();
        assert!(err
            .to_string()
            .contains("colliding interior-pointer accessor name"));
    }

    #[test]
    fn test_rejects_heap_owning_types_without_skip() {
        for bad_input in [
            parse_quote! { pub struct Bad1 { pub opt: Option<String> } },
            parse_quote! { pub struct Bad2 { pub boxed: Box<u32> } },
            parse_quote! { pub struct Bad3 { pub map: HashMap<u32, u32> } },
            parse_quote! { pub struct Bad4 { pub r: &'static str } },
        ] {
            let err = expand_mod_exposable(&bad_input).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("mod_field(skip)") || msg.contains("cannot be exposed"),
                "unexpected error message: {msg}"
            );
        }
    }

    #[test]
    fn test_rejects_unknown_attributes() {
        let input: DeriveInput = parse_quote! {
            #[mod_component(nonexistent_attr)]
            pub struct BadAttr {
                pub x: f32,
            }
        };
        let err = expand_mod_exposable(&input).unwrap_err();
        assert!(err.to_string().contains("unsupported `#[mod_component"));
    }
}
