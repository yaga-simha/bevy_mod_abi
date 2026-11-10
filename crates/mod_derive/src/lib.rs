#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `mod_derive`
//!
//! Procedural macro crate providing `#[derive(ModExposable)]` for Bevy host components.
//! Generates `#[no_mangle] pub unsafe extern "C-unwind"` field accessors, a `#[repr(C)]`
//! `<Struct>VTable` containing exact `size` and `align` byte strides and accessor function
//! pointers, and implements `abi_contract::ExposableComponent`.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, Data, DeriveInput, Fields, GenericArgument, Index, Ident, LitByteStr,
    PathArguments, Type,
};

/// Classification of a struct field for C-ABI accessor code generation.
enum FieldKind {
    /// Copy primitive (`f32`, `f64`, `i8`..`i64`, `u8`..`u64`, `usize`, `isize`, `bool`).
    Primitive(Type),
    /// Heap-allocated `String` owned by the host allocator.
    HostString,
    /// Heap-allocated `Vec<T>` owned by the host allocator (`T: Copy`).
    HostVec(Type),
    /// Nested compound struct or fixed array exposed via interior pointer accessors.
    Nested(Type),
}

fn classify_type(ty: &Type) -> FieldKind {
    if let Type::Path(type_path) = ty {
        if type_path.qself.is_none() {
            if let Some(last_seg) = type_path.path.segments.last() {
                let ident_str = last_seg.ident.to_string();
                match (&last_seg.arguments, ident_str.as_str()) {
                    (
                        PathArguments::None,
                        "f32" | "f64" | "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16"
                        | "u32" | "u64" | "usize" | "bool",
                    ) => {
                        return FieldKind::Primitive(ty.clone());
                    }
                    (PathArguments::None, "String") => {
                        return FieldKind::HostString;
                    }
                    (PathArguments::AngleBracketed(args), "Vec") => {
                        if args.args.len() == 1 {
                            if let Some(GenericArgument::Type(elem_ty)) = args.args.first() {
                                return FieldKind::HostVec(elem_ty.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    FieldKind::Nested(ty.clone())
}

/// Derive macro for `ModExposable` components.
///
/// For any `#[repr(Rust)]` struct, generates:
/// 1. `#[no_mangle] pub unsafe extern "C-unwind"` getters/setters/interior-pointer functions.
/// 2. `#[repr(C)] pub struct <Struct>VTable` with `size`, `align`, and all accessor pointers.
/// 3. `impl abi_contract::ExposableComponent for <Struct>`.
#[proc_macro_derive(ModExposable, attributes(mod_component, mod_field))]
pub fn derive_mod_exposable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let struct_ident = &input.ident;
    let struct_name_str = struct_ident.to_string();
    let vtable_ident = format_ident!("{}VTable", struct_ident);
    let vtable_fn_ident = format_ident!("{}_vtable", struct_ident);

    let mut cstr_bytes = struct_name_str.as_bytes().to_vec();
    cstr_bytes.push(0);
    let cstr_lit = LitByteStr::new(&cstr_bytes, Span::call_site());

    let fields = match &input.data {
        Data::Struct(data_struct) => &data_struct.fields,
        _ => {
            return syn::Error::new_spanned(
                &input.ident,
                "`#[derive(ModExposable)]` can only be applied to structs",
            )
            .to_compile_error()
            .into();
        }
    };

    let mut extern_fns = Vec::new();
    let mut vtable_fields = Vec::new();
    let mut vtable_inits = Vec::new();
    let mut symbol_entries = Vec::new();

    // Register `<Struct>_vtable` symbol returning the static VTable struct pointer
    symbol_entries.push(quote! {
        ::abi_contract::SymbolEntry {
            name: #struct_name_str,
            ptr: &#struct_ident::VTABLE as *const #vtable_ident as *const ::core::ffi::c_void,
        },
        ::abi_contract::SymbolEntry {
            name: concat!(#struct_name_str, "_vtable"),
            ptr: &#struct_ident::VTABLE as *const #vtable_ident as *const ::core::ffi::c_void,
        },
        ::abi_contract::SymbolEntry {
            name: concat!(#struct_name_str, "_get_vtable"),
            ptr: #vtable_fn_ident as *const ::core::ffi::c_void,
        }
    });

    let field_iter: Vec<(String, proc_macro2::TokenStream, &Type)> = match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                let ident = f.ident.as_ref().expect("named field must have ident");
                (ident.to_string(), quote! { #ident }, &f.ty)
            })
            .collect(),
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(idx, f)| {
                let index = Index::from(idx);
                (idx.to_string(), quote! { #index }, &f.ty)
            })
            .collect(),
        Fields::Unit => Vec::new(),
    };

    for (field_name, member_tokens, field_ty) in field_iter {
        let kind = classify_type(field_ty);
        match kind {
            FieldKind::Primitive(prim_ty) => {
                let fn_get_ident = Ident::new(
                    &format!("{}_get_{}", struct_name_str, field_name),
                    Span::call_site(),
                );
                let fn_set_ident = Ident::new(
                    &format!("{}_set_{}", struct_name_str, field_name),
                    Span::call_site(),
                );
                let vt_get_ident =
                    Ident::new(&format!("get_{}", field_name), Span::call_site());
                let vt_set_ident =
                    Ident::new(&format!("set_{}", field_name), Span::call_site());

                let fn_get_name_str = fn_get_ident.to_string();
                let fn_set_name_str = fn_set_ident.to_string();

                extern_fns.push(quote! {
                    /// Read primitive field `#field_name` from `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an initialized `#struct_ident`.
                    #[no_mangle]
                    pub unsafe extern "C-unwind" fn #fn_get_ident(
                        ptr: *const ::core::ffi::c_void,
                    ) -> #prim_ty {
                        if ptr.is_null() {
                            return <#prim_ty as ::core::default::Default>::default();
                        }
                        let typed_ptr = ptr as *const #struct_ident;
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
                    #[no_mangle]
                    pub unsafe extern "C-unwind" fn #fn_set_ident(
                        ptr: *mut ::core::ffi::c_void,
                        val: #prim_ty,
                    ) {
                        if ptr.is_null() {
                            return;
                        }
                        let typed_ptr = ptr as *mut #struct_ident;
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
                    pub #vt_get_ident: unsafe extern "C-unwind" fn(*const ::core::ffi::c_void) -> #prim_ty,
                    pub #vt_set_ident: unsafe extern "C-unwind" fn(*mut ::core::ffi::c_void, #prim_ty),
                });

                vtable_inits.push(quote! {
                    #vt_get_ident: #fn_get_ident,
                    #vt_set_ident: #fn_set_ident,
                });

                symbol_entries.push(quote! {
                    ::abi_contract::SymbolEntry {
                        name: #fn_get_name_str,
                        ptr: #fn_get_ident as *const ::core::ffi::c_void,
                    },
                    ::abi_contract::SymbolEntry {
                        name: #fn_set_name_str,
                        ptr: #fn_set_ident as *const ::core::ffi::c_void,
                    }
                });
            }
            FieldKind::HostString => {
                let fn_get_ident = Ident::new(
                    &format!("{}_get_{}", struct_name_str, field_name),
                    Span::call_site(),
                );
                let fn_set_ident = Ident::new(
                    &format!("{}_set_{}", struct_name_str, field_name),
                    Span::call_site(),
                );
                let vt_get_ident =
                    Ident::new(&format!("get_{}", field_name), Span::call_site());
                let vt_set_ident =
                    Ident::new(&format!("set_{}", field_name), Span::call_site());

                let fn_get_name_str = fn_get_ident.to_string();
                let fn_set_name_str = fn_set_ident.to_string();

                extern_fns.push(quote! {
                    /// Borrow UTF-8 bytes of `String` field `#field_name` on `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an initialized `#struct_ident`.
                    #[no_mangle]
                    pub unsafe extern "C-unwind" fn #fn_get_ident(
                        ptr: *const ::core::ffi::c_void,
                    ) -> ::abi_contract::FfiSlice<u8> {
                        if ptr.is_null() {
                            return ::abi_contract::FfiSlice::empty();
                        }
                        let typed_ptr = ptr as *const #struct_ident;
                        // SAFETY: Caller guarantees `ptr` points to a valid `#struct_ident` instance
                        // whose `String` field remains alive for the borrow duration.
                        let field_ref: &::std::string::String =
                            unsafe { &(*typed_ptr).#member_tokens };
                        ::abi_contract::FfiSlice::from_str(field_ref.as_str())
                    }

                    /// Mutate `String` field `#field_name` on `#struct_ident` using the host's heap allocator.
                    ///
                    /// Validates non-null pointers and UTF-8 encoding before mutating.
                    ///
                    /// # Safety
                    /// - `ptr` must be non-null, properly aligned, and point to a mutable `#struct_ident`.
                    /// - If `len > 0`, `utf8_ptr` must be non-null and point to `len` valid bytes.
                    #[no_mangle]
                    pub unsafe extern "C-unwind" fn #fn_set_ident(
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
                        let bytes: &[u8] = if len == 0 {
                            &[]
                        } else {
                            // SAFETY: `utf8_ptr` is verified non-null and caller guarantees `len` valid bytes.
                            unsafe { ::core::slice::from_raw_parts(utf8_ptr, len) }
                        };
                        if let ::core::result::Result::Ok(valid_str) = ::core::str::from_utf8(bytes) {
                            let typed_ptr = ptr as *mut #struct_ident;
                            // SAFETY: `typed_ptr` is verified non-null and points to a host-owned `#struct_ident`.
                            // All string buffer reallocation occurs strictly on the host heap.
                            let field_mut: &mut ::std::string::String =
                                unsafe { &mut (*typed_ptr).#member_tokens };
                            field_mut.clear();
                            field_mut.push_str(valid_str);
                        }
                    }
                });

                vtable_fields.push(quote! {
                    pub #vt_get_ident: unsafe extern "C-unwind" fn(*const ::core::ffi::c_void) -> ::abi_contract::FfiSlice<u8>,
                    pub #vt_set_ident: unsafe extern "C-unwind" fn(*mut ::core::ffi::c_void, *const u8, usize),
                });

                vtable_inits.push(quote! {
                    #vt_get_ident: #fn_get_ident,
                    #vt_set_ident: #fn_set_ident,
                });

                symbol_entries.push(quote! {
                    ::abi_contract::SymbolEntry {
                        name: #fn_get_name_str,
                        ptr: #fn_get_ident as *const ::core::ffi::c_void,
                    },
                    ::abi_contract::SymbolEntry {
                        name: #fn_set_name_str,
                        ptr: #fn_set_ident as *const ::core::ffi::c_void,
                    }
                });
            }
            FieldKind::HostVec(elem_ty) => {
                let fn_get_ident = Ident::new(
                    &format!("{}_get_{}", struct_name_str, field_name),
                    Span::call_site(),
                );
                let fn_set_ident = Ident::new(
                    &format!("{}_set_{}", struct_name_str, field_name),
                    Span::call_site(),
                );
                let vt_get_ident =
                    Ident::new(&format!("get_{}", field_name), Span::call_site());
                let vt_set_ident =
                    Ident::new(&format!("set_{}", field_name), Span::call_site());

                let fn_get_name_str = fn_get_ident.to_string();
                let fn_set_name_str = fn_set_ident.to_string();

                extern_fns.push(quote! {
                    /// Borrow elements of `Vec` field `#field_name` on `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an initialized `#struct_ident`.
                    #[no_mangle]
                    pub unsafe extern "C-unwind" fn #fn_get_ident(
                        ptr: *const ::core::ffi::c_void,
                    ) -> ::abi_contract::FfiSlice<#elem_ty> {
                        if ptr.is_null() {
                            return ::abi_contract::FfiSlice::empty();
                        }
                        let typed_ptr = ptr as *const #struct_ident;
                        // SAFETY: Caller guarantees `ptr` points to a valid `#struct_ident` instance.
                        let field_ref: &::std::vec::Vec<#elem_ty> =
                            unsafe { &(*typed_ptr).#member_tokens };
                        ::abi_contract::FfiSlice::from_slice(field_ref.as_slice())
                    }

                    /// Mutate `Vec` field `#field_name` on `#struct_ident` using the host's heap allocator.
                    ///
                    /// # Safety
                    /// - `ptr` must be non-null, properly aligned, and point to a mutable `#struct_ident`.
                    /// - If `len > 0`, `data_ptr` must be non-null, aligned, and point to `len` valid elements.
                    #[no_mangle]
                    pub unsafe extern "C-unwind" fn #fn_set_ident(
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
                        let slice: &[#elem_ty] = if len == 0 {
                            &[]
                        } else {
                            // SAFETY: `data_ptr` is verified non-null and caller guarantees `len` valid elements.
                            unsafe { ::core::slice::from_raw_parts(data_ptr, len) }
                        };
                        let typed_ptr = ptr as *mut #struct_ident;
                        // SAFETY: `typed_ptr` is verified non-null and points to a host-owned `#struct_ident`.
                        let field_mut: &mut ::std::vec::Vec<#elem_ty> =
                            unsafe { &mut (*typed_ptr).#member_tokens };
                        field_mut.clear();
                        field_mut.extend_from_slice(slice);
                    }
                });

                vtable_fields.push(quote! {
                    pub #vt_get_ident: unsafe extern "C-unwind" fn(*const ::core::ffi::c_void) -> ::abi_contract::FfiSlice<#elem_ty>,
                    pub #vt_set_ident: unsafe extern "C-unwind" fn(*mut ::core::ffi::c_void, *const #elem_ty, usize),
                });

                vtable_inits.push(quote! {
                    #vt_get_ident: #fn_get_ident,
                    #vt_set_ident: #fn_set_ident,
                });

                symbol_entries.push(quote! {
                    ::abi_contract::SymbolEntry {
                        name: #fn_get_name_str,
                        ptr: #fn_get_ident as *const ::core::ffi::c_void,
                    },
                    ::abi_contract::SymbolEntry {
                        name: #fn_set_name_str,
                        ptr: #fn_set_ident as *const ::core::ffi::c_void,
                    }
                });
            }
            FieldKind::Nested(_nested_ty) => {
                let fn_ptr_ident = Ident::new(
                    &format!("{}_ptr_{}", struct_name_str, field_name),
                    Span::call_site(),
                );
                let fn_ptr_mut_ident = Ident::new(
                    &format!("{}_ptr_mut_{}", struct_name_str, field_name),
                    Span::call_site(),
                );
                let vt_ptr_ident =
                    Ident::new(&format!("ptr_{}", field_name), Span::call_site());
                let vt_ptr_mut_ident =
                    Ident::new(&format!("ptr_mut_{}", field_name), Span::call_site());

                let fn_ptr_name_str = fn_ptr_ident.to_string();
                let fn_ptr_mut_name_str = fn_ptr_mut_ident.to_string();

                extern_fns.push(quote! {
                    /// Obtain a read-only interior pointer to nested field `#field_name` on `#struct_ident`.
                    ///
                    /// # Safety
                    /// `ptr` must be non-null, properly aligned, and point to an initialized `#struct_ident`.
                    #[no_mangle]
                    pub unsafe extern "C-unwind" fn #fn_ptr_ident(
                        ptr: *const ::core::ffi::c_void,
                    ) -> *const ::core::ffi::c_void {
                        if ptr.is_null() {
                            return ::core::ptr::null();
                        }
                        let typed_ptr = ptr as *const #struct_ident;
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
                    #[no_mangle]
                    pub unsafe extern "C-unwind" fn #fn_ptr_mut_ident(
                        ptr: *mut ::core::ffi::c_void,
                    ) -> *mut ::core::ffi::c_void {
                        if ptr.is_null() {
                            return ::core::ptr::null_mut();
                        }
                        let typed_ptr = ptr as *mut #struct_ident;
                        // SAFETY: `typed_ptr` is verified non-null and points to a valid mutable `#struct_ident`;
                        // `addr_of_mut!` computes the mutable interior pointer without aliasing violations.
                        unsafe {
                            ::core::ptr::addr_of_mut!((*typed_ptr).#member_tokens)
                                as *mut ::core::ffi::c_void
                        }
                    }
                });

                vtable_fields.push(quote! {
                    pub #vt_ptr_ident: unsafe extern "C-unwind" fn(*const ::core::ffi::c_void) -> *const ::core::ffi::c_void,
                    pub #vt_ptr_mut_ident: unsafe extern "C-unwind" fn(*mut ::core::ffi::c_void) -> *mut ::core::ffi::c_void,
                });

                vtable_inits.push(quote! {
                    #vt_ptr_ident: #fn_ptr_ident,
                    #vt_ptr_mut_ident: #fn_ptr_mut_ident,
                });

                symbol_entries.push(quote! {
                    ::abi_contract::SymbolEntry {
                        name: #fn_ptr_name_str,
                        ptr: #fn_ptr_ident as *const ::core::ffi::c_void,
                    },
                    ::abi_contract::SymbolEntry {
                        name: #fn_ptr_mut_name_str,
                        ptr: #fn_ptr_mut_ident as *const ::core::ffi::c_void,
                    }
                });
            }
        }
    }

    let expanded = quote! {
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

        /// Return a pointer to the static `#vtable_ident` for `#struct_ident`.
        #[no_mangle]
        pub unsafe extern "C-unwind" fn #vtable_fn_ident() -> *const #vtable_ident {
            &#struct_ident::VTABLE as *const #vtable_ident
        }

        impl #struct_ident {
            pub const MOD_COMPONENT_NAME: &'static str = #struct_name_str;

            pub const VTABLE: #vtable_ident = #vtable_ident {
                size: ::core::mem::size_of::<#struct_ident>(),
                align: ::core::mem::align_of::<#struct_ident>(),
                #(#vtable_inits)*
            };

            #[inline]
            pub const fn vtable() -> &'static #vtable_ident {
                &Self::VTABLE
            }
        }

        impl ::abi_contract::ExposableComponent for #struct_ident {
            const COMPONENT_NAME: &'static str = #struct_name_str;
            const COMPONENT_CSTR: &'static [u8] = #cstr_lit;
            const UUID: ::abi_contract::ComponentUuid =
                ::abi_contract::ComponentUuid::from_name(#struct_name_str);

            type VTable = #vtable_ident;

            const VTABLE: Self::VTable = #struct_ident::VTABLE;

            #[inline]
            fn vtable_ptr() -> *const Self::VTable {
                &#struct_ident::VTABLE as *const Self::VTable
            }

            #[inline]
            fn symbols() -> &'static [::abi_contract::SymbolEntry] {
                static SYMBOLS: &[::abi_contract::SymbolEntry] = &[
                    #(#symbol_entries),*
                ];
                SYMBOLS
            }
        }
    };

    TokenStream::from(expanded)
}
