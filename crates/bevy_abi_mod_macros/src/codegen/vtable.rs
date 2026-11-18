use crate::codegen::CodegenState;
use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

pub(crate) fn emit_vtable_and_impls(state: &CodegenState, vtable_ident: &Ident) -> TokenStream {
    let sys_crate = &state.sys_crate;
    let struct_ident = &state.struct_ident;
    let layout_assertions = &state.layout_assertions;
    let extern_fns = &state.extern_fns;
    let vtable_fields = &state.vtable_fields;
    let vtable_inits = &state.vtable_inits;
    let symbol_entries = &state.symbol_entries;
    let nested_pod_assertions = &state.nested_pod_assertions;
    let component_flags_expr = &state.component_flags_expr;

    let summary_tick = state.summary_tick;
    let req_names: Vec<TokenStream> = state
        .required_components
        .iter()
        .map(|path| quote! { <#path as #sys_crate::ModComponent>::CANONICAL_NAME })
        .collect();
    let req_uuids: Vec<TokenStream> = state
        .required_components
        .iter()
        .map(|path| quote! { <#path as #sys_crate::ModComponent>::UUID })
        .collect();
    let req_assertions: Vec<TokenStream> = state
        .required_components
        .iter()
        .map(|path| {
            quote::quote_spanned! { syn::spanned::Spanned::span(path) =>
                const _: fn() = || {
                    fn assert_required_mod_component<T: #sys_crate::ModComponent>() {}
                    assert_required_mod_component::<#path>();
                };
            }
        })
        .collect();

    let witness_helpers = if state.opaque {
        quote! {}
    } else {
        crate::codegen::witness::emit_witness_helpers(sys_crate)
    };

    let is_raw_insertable = state.can_impl_raw_insertable;
    let is_abi_pod = state.can_impl_raw_insertable && state.can_impl_abi_pod;
    let (c_decl_str, zig_decl_str) =
        crate::codegen::c_decl::generate_decls(state, vtable_ident, is_abi_pod);

    let canonical_name_expr = state.canonical_name.as_str_expr();
    let canonical_cstr_bytes_expr = state.canonical_name.as_nul_bytes_expr();

    let uuid_expr = match state.custom_uuid.as_deref() {
        Some(hex_str) => {
            quote! { #sys_crate::ComponentUuid::parse_hex(#hex_str) }
        }
        None if state.decl_enum.is_some() => {
            let enum_info = state.decl_enum.as_ref().expect("checked Some");
            let tag_str = enum_info.int_repr.rust_ty_str();
            let mut schema_expr = quote! {
                #sys_crate::ComponentUuid::hash_field_schema(0u64, "__enum_tag", #tag_str, 0usize)
            };
            for var in &enum_info.variants {
                let v_name = &var.name;
                let v_tag = format!("variant:{}", var.discriminant);
                schema_expr = quote! {
                    #sys_crate::ComponentUuid::hash_field_schema(#schema_expr, #v_name, #v_tag, 0usize)
                };
                for (f_idx, (f_name, f_tag)) in var.schema_fields.iter().enumerate() {
                    let f_key = format!("{v_name}::{f_name}");
                    schema_expr = quote! {
                        #sys_crate::ComponentUuid::hash_field_schema(#schema_expr, #f_key, #f_tag, #f_idx)
                    };
                }
            }
            quote! {
                #sys_crate::ComponentUuid::from_parts(
                    Self::CANONICAL_NAME,
                    ::core::mem::size_of::<#struct_ident>(),
                    ::core::mem::align_of::<#struct_ident>(),
                    #schema_expr,
                )
            }
        }
        None if is_abi_pod => {
            let mut schema_expr = quote! { 0u64 };
            for (field_name, field_tag, member_toks) in &state.schema_fields {
                schema_expr = quote! {
                    #sys_crate::ComponentUuid::hash_field_schema(
                        #schema_expr,
                        #field_name,
                        #field_tag,
                        ::core::mem::offset_of!(#struct_ident, #member_toks),
                    )
                };
            }
            quote! {
                #sys_crate::ComponentUuid::from_parts(
                    Self::CANONICAL_NAME,
                    ::core::mem::size_of::<#struct_ident>(),
                    ::core::mem::align_of::<#struct_ident>(),
                    #schema_expr,
                )
            }
        }
        None => {
            quote! {
                #sys_crate::ComponentUuid::from_canonical_name(Self::CANONICAL_NAME)
            }
        }
    };

    let conditional_raw_insertable_impl = if is_raw_insertable {
        quote! {
            ::core::assert!(!::core::mem::needs_drop::<#struct_ident>());

            // SAFETY: `#struct_ident` is a non-packed `'static` struct with zero drop glue, no skipped fields,
            // no `String`/`Vec`/`bool` fields, and every field verified `RawInsertable` / `AbiPod` above.
            #[allow(clippy::undocumented_unsafe_blocks)]
            unsafe impl #sys_crate::RawInsertable for #struct_ident {}
        }
    } else {
        quote! {}
    };

    let conditional_abi_pod_impl = if is_abi_pod {
        quote! {
            fn assert_pod_self_copy<T: ::core::marker::Copy + 'static>() {}
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
        /// Generated C-ABI `0.2` VTable for `#struct_ident`, prefixed at byte offset `0` by
        /// `VTableHeader` (`struct_size`, `abi_version`, `uuid`, `size`, `align`) followed by
        /// `Option<unsafe extern "C" fn(...)>` field accessor slots.
        #[allow(unreachable_pub)]
        #[repr(C)]
        #[derive(Debug, Copy, Clone)]
        pub struct #vtable_ident {
            /// Wire-level VTable metadata header (`struct_size`, `abi_version`, `uuid`, `size`, `align`).
            pub header: #sys_crate::VTableHeader,
            #(#vtable_fields)*
        }

        // SAFETY: `#vtable_ident` is `#[repr(C)]`, begins with `VTableHeader` (`struct_size: u32`
        // at byte offset `0`), and every trailing field is `Option<unsafe extern "C" fn(...)>`,
        // which is valid when all-bits-zero under Rust's null-pointer niche guarantee.
        #[allow(clippy::undocumented_unsafe_blocks)]
        unsafe impl #sys_crate::ExtensibleStruct for #vtable_ident {
            const MIN_V0_2_SIZE: usize = ::core::mem::size_of::<#vtable_ident>();
        }

        #[allow(non_snake_case)]
        const _: () = {
            #[allow(dead_code)]
            #[diagnostic::on_unimplemented(
                message = "field type `{Self}` is not Rust's primitive `bool`; use `#[mod_field(nested)]`, `#[mod_field(primitive)]`, or `#[mod_field(skip)]` to override classification"
            )]
            trait __BamPrimBool: Sized {
                fn cast_const_ptr(ptr: *const Self) -> *const bool;
                fn cast_mut_ptr(ptr: *mut Self) -> *mut bool;
            }

            impl __BamPrimBool for bool {
                #[inline(always)]
                fn cast_const_ptr(ptr: *const Self) -> *const bool {
                    ptr
                }
                #[inline(always)]
                fn cast_mut_ptr(ptr: *mut Self) -> *mut bool {
                    ptr
                }
            }

            #[allow(dead_code)]
            #[diagnostic::on_unimplemented(
                message = "field type `{Self}` is not `std::string::String` (syntactic `String` classification requires `std::string::String`; use `#[mod_field(nested)]`, `#[mod_field(primitive)]`, or `#[mod_field(skip)]` if intentional)"
            )]
            trait __BamStdString: Sized {
                fn cast_const_ptr(ptr: *const Self) -> *const ::std::string::String;
                fn cast_mut_ptr(ptr: *mut Self) -> *mut ::std::string::String;
            }

            impl __BamStdString for ::std::string::String {
                #[inline(always)]
                fn cast_const_ptr(ptr: *const Self) -> *const ::std::string::String {
                    ptr
                }
                #[inline(always)]
                fn cast_mut_ptr(ptr: *mut Self) -> *mut ::std::string::String {
                    ptr
                }
            }

            #[allow(dead_code)]
            #[diagnostic::on_unimplemented(
                message = "field type `{Self}` is not `std::vec::Vec<{Elem}>` (syntactic `Vec<T>` classification requires `std::vec::Vec<T>`; use `#[mod_field(nested)]` or `#[mod_field(skip)]` if intentional)"
            )]
            trait __BamStdVec<Elem>: Sized {
                fn cast_const_ptr(ptr: *const Self) -> *const ::std::vec::Vec<Elem>;
                fn cast_mut_ptr(ptr: *mut Self) -> *mut ::std::vec::Vec<Elem>;
            }

            impl<Elem> __BamStdVec<Elem> for ::std::vec::Vec<Elem> {
                #[inline(always)]
                fn cast_const_ptr(ptr: *const Self) -> *const ::std::vec::Vec<Elem> {
                    ptr
                }
                #[inline(always)]
                fn cast_mut_ptr(ptr: *mut Self) -> *mut ::std::vec::Vec<Elem> {
                    ptr
                }
            }

            #[allow(dead_code)]
            #[diagnostic::on_unimplemented(
                message = "field type `{Self}` is not `bevy_abi_mod_sys::FfiOption<{Inner}>` (syntactic `FfiOption<T>` classification requires `bevy_abi_mod_sys::FfiOption<T>`; use `#[mod_field(nested)]` or `#[mod_field(skip)]` if intentional)"
            )]
            trait __BamFfiOption<Inner>: Sized {
                fn cast_const_ptr(ptr: *const Self) -> *const #sys_crate::FfiOption<Inner>;
                fn cast_mut_ptr(ptr: *mut Self) -> *mut #sys_crate::FfiOption<Inner>;
            }

            impl<Inner> __BamFfiOption<Inner> for #sys_crate::FfiOption<Inner> {
                #[inline(always)]
                fn cast_const_ptr(ptr: *const Self) -> *const #sys_crate::FfiOption<Inner> {
                    ptr
                }
                #[inline(always)]
                fn cast_mut_ptr(ptr: *mut Self) -> *mut #sys_crate::FfiOption<Inner> {
                    ptr
                }
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_read_bool<T: __BamPrimBool>(
                field_ptr: *const T,
            ) -> #sys_crate::FfiBool {
                let ptr = <T as __BamPrimBool>::cast_const_ptr(field_ptr);
                let flag: bool = unsafe { ::core::ptr::read(ptr) };
                #sys_crate::FfiBool::from_bool(flag)
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_write_bool<T: __BamPrimBool>(
                field_ptr: *mut T,
                val: #sys_crate::FfiBool,
            ) -> #sys_crate::FfiResult {
                let ptr = <T as __BamPrimBool>::cast_mut_ptr(field_ptr);
                let normalized: bool = val.get();
                unsafe { ::core::ptr::write(ptr, normalized) };
                #sys_crate::FfiResult::OK
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_read_string<T: __BamStdString>(
                field_ptr: *const T,
            ) -> #sys_crate::StrView {
                let str_ptr = <T as __BamStdString>::cast_const_ptr(field_ptr);
                let str_ref: &::std::string::String = unsafe { &*str_ptr };
                #sys_crate::StrView::from_str(str_ref.as_str())
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_write_string<T: __BamStdString>(
                field_ptr: *mut T,
                view: #sys_crate::StrView,
            ) -> #sys_crate::FfiResult {
                let status = view.validate();
                if status.is_err() {
                    return status;
                }
                let str_ptr = <T as __BamStdString>::cast_mut_ptr(field_ptr);
                if view.len == 0 {
                    unsafe { (*str_ptr).clear() };
                    return #sys_crate::FfiResult::OK;
                }

                // Inspect the existing heap buffer capacity interval before creating any `&mut String`
                // so self-aliasing slices (`get` view passed back into `set`) never violate aliasing.
                let (buf_start, buf_end) = unsafe {
                    let existing: &::std::string::String = &*str_ptr;
                    let start = existing.as_ptr() as usize;
                    (start, start.saturating_add(existing.capacity()))
                };
                let src_start = view.ptr as usize;
                let src_end = src_start.saturating_add(view.len);
                let overlaps = src_start < buf_end && buf_start < src_end;

                let bytes: &[u8] = unsafe { view.as_slice() };
                let valid_str: &str = match ::core::str::from_utf8(bytes) {
                    ::core::result::Result::Ok(s) => s,
                    ::core::result::Result::Err(_) => return #sys_crate::FfiResult::INVALID_UTF8,
                };

                if overlaps {
                    let temp = ::std::string::String::from(valid_str);
                    unsafe {
                        let field_mut = &mut *str_ptr;
                        field_mut.clear();
                        field_mut.push_str(temp.as_str());
                    }
                } else {
                    unsafe {
                        let field_mut = &mut *str_ptr;
                        field_mut.clear();
                        field_mut.push_str(valid_str);
                    }
                }
                #sys_crate::FfiResult::OK
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_read_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *const T,
            ) -> #sys_crate::FfiSlice<Elem> {
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_const_ptr(field_ptr);
                let vec_ref: &::std::vec::Vec<Elem> = unsafe { &*vec_ptr };
                #sys_crate::FfiSlice::from_slice(vec_ref.as_slice())
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_write_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *mut T,
                slice: #sys_crate::FfiSlice<Elem>,
            ) -> #sys_crate::FfiResult {
                let status = slice.validate();
                if status.is_err() {
                    return status;
                }
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_mut_ptr(field_ptr);
                if slice.len == 0 {
                    unsafe { (*vec_ptr).clear() };
                    return #sys_crate::FfiResult::OK;
                }

                let elem_size = ::core::mem::size_of::<Elem>();
                let (buf_start, buf_end) = unsafe {
                    let existing: &::std::vec::Vec<Elem> = &*vec_ptr;
                    let start = existing.as_ptr() as usize;
                    let cap_bytes = existing.capacity().saturating_mul(elem_size);
                    (start, start.saturating_add(cap_bytes))
                };
                let src_start = slice.ptr as usize;
                let src_end = src_start.saturating_add(slice.len.saturating_mul(elem_size));
                let overlaps = src_start < buf_end && buf_start < src_end;

                let elems: &[Elem] = unsafe { slice.as_slice() };
                if overlaps {
                    let temp: ::std::vec::Vec<Elem> = elems.to_vec();
                    unsafe {
                        let field_mut = &mut *vec_ptr;
                        field_mut.clear();
                        field_mut.extend_from_slice(temp.as_slice());
                    }
                } else {
                    unsafe {
                        let field_mut = &mut *vec_ptr;
                        field_mut.clear();
                        field_mut.extend_from_slice(elems);
                    }
                }
                #sys_crate::FfiResult::OK
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_push_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *mut T,
                val: Elem,
            ) -> #sys_crate::FfiResult {
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_mut_ptr(field_ptr);
                let vec_mut = unsafe { &mut *vec_ptr };
                if vec_mut.try_reserve(1).is_err() {
                    return #sys_crate::FfiResult::CAPACITY_EXCEEDED;
                }
                vec_mut.push(val);
                #sys_crate::FfiResult::OK
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_len_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *const T,
            ) -> usize {
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_const_ptr(field_ptr);
                unsafe { (*vec_ptr).len() }
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_clear_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *mut T,
            ) -> #sys_crate::FfiResult {
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_mut_ptr(field_ptr);
                unsafe { (*vec_ptr).clear() };
                #sys_crate::FfiResult::OK
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_pop_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *mut T,
                out_elem: *mut Elem,
            ) -> #sys_crate::FfiBool {
                if out_elem.is_null() || !out_elem.is_aligned() {
                    return #sys_crate::FfiBool::FALSE;
                }
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_mut_ptr(field_ptr);
                match unsafe { (*vec_ptr).pop() } {
                    ::core::option::Option::Some(elem) => {
                        unsafe { ::core::ptr::write(out_elem, elem) };
                        #sys_crate::FfiBool::TRUE
                    }
                    ::core::option::Option::None => #sys_crate::FfiBool::FALSE,
                }
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_remove_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *mut T,
                index: usize,
                out_elem: *mut Elem,
            ) -> #sys_crate::FfiResult {
                if out_elem.is_null() {
                    return #sys_crate::FfiResult::NULL_POINTER;
                }
                if !out_elem.is_aligned() {
                    return #sys_crate::FfiResult::INVALID_ARGUMENT;
                }
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_mut_ptr(field_ptr);
                let vec_mut = unsafe { &mut *vec_ptr };
                if index >= vec_mut.len() {
                    return #sys_crate::FfiResult::OUT_OF_RANGE;
                }
                let elem = vec_mut.remove(index);
                unsafe { ::core::ptr::write(out_elem, elem) };
                #sys_crate::FfiResult::OK
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_swap_remove_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *mut T,
                index: usize,
                out_elem: *mut Elem,
            ) -> #sys_crate::FfiResult {
                if out_elem.is_null() {
                    return #sys_crate::FfiResult::NULL_POINTER;
                }
                if !out_elem.is_aligned() {
                    return #sys_crate::FfiResult::INVALID_ARGUMENT;
                }
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_mut_ptr(field_ptr);
                let vec_mut = unsafe { &mut *vec_ptr };
                if index >= vec_mut.len() {
                    return #sys_crate::FfiResult::OUT_OF_RANGE;
                }
                let elem = vec_mut.swap_remove(index);
                unsafe { ::core::ptr::write(out_elem, elem) };
                #sys_crate::FfiResult::OK
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_truncate_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *mut T,
                new_len: usize,
            ) -> #sys_crate::FfiResult {
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_mut_ptr(field_ptr);
                unsafe { (*vec_ptr).truncate(new_len) };
                #sys_crate::FfiResult::OK
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_reserve_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *mut T,
                additional: usize,
            ) -> #sys_crate::FfiResult {
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_mut_ptr(field_ptr);
                match unsafe { (*vec_ptr).try_reserve(additional) } {
                    ::core::result::Result::Ok(()) => #sys_crate::FfiResult::OK,
                    ::core::result::Result::Err(_) => #sys_crate::FfiResult::CAPACITY_EXCEEDED,
                }
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_as_mut_slice_vec<Elem: #sys_crate::AbiPod, T: __BamStdVec<Elem>>(
                field_ptr: *mut T,
                out_len: *mut usize,
            ) -> *mut Elem {
                if out_len.is_null() || !out_len.is_aligned() {
                    return ::core::ptr::null_mut();
                }
                let vec_ptr = <T as __BamStdVec<Elem>>::cast_mut_ptr(field_ptr);
                let vec_mut = unsafe { &mut *vec_ptr };
                unsafe { ::core::ptr::write(out_len, vec_mut.len()) };
                vec_mut.as_mut_ptr()
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_read_option<Inner: #sys_crate::AbiPod, T: __BamFfiOption<Inner>>(
                field_ptr: *const T,
            ) -> #sys_crate::FfiOption<Inner> {
                let opt_ptr = <T as __BamFfiOption<Inner>>::cast_const_ptr(field_ptr);
                unsafe { ::core::ptr::read(opt_ptr) }
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_write_option<Inner: #sys_crate::AbiPod, T: __BamFfiOption<Inner>>(
                field_ptr: *mut T,
                val: #sys_crate::FfiOption<Inner>,
            ) -> #sys_crate::FfiResult {
                let opt_ptr = <T as __BamFfiOption<Inner>>::cast_mut_ptr(field_ptr);
                let normalized = if val.is_some.get() {
                    #sys_crate::FfiOption::some(val.value)
                } else {
                    #sys_crate::FfiOption::none()
                };
                unsafe { ::core::ptr::write(opt_ptr, normalized) };
                #sys_crate::FfiResult::OK
            }

            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            #[inline]
            unsafe fn __bam_clear_option<Inner: #sys_crate::AbiPod, T: __BamFfiOption<Inner>>(
                field_ptr: *mut T,
            ) -> #sys_crate::FfiResult {
                let opt_ptr = <T as __BamFfiOption<Inner>>::cast_mut_ptr(field_ptr);
                unsafe { ::core::ptr::write(opt_ptr, #sys_crate::FfiOption::<Inner>::none()) };
                #sys_crate::FfiResult::OK
            }

            #witness_helpers
            #(#layout_assertions)*
            #(#req_assertions)*
            #(#extern_fns)*

            #[allow(unreachable_pub)]
            impl #struct_ident {
                /// Canonical namespaced component name (`"<namespace>::<Name>"`).
                pub const MOD_COMPONENT_NAME: &'static str =
                    <#struct_ident as #sys_crate::ModComponent>::CANONICAL_NAME;

                /// `true` when `#[mod_component(summary_tick)]` is enabled.
                pub const SUMMARY_TICK: bool = #summary_tick;

                /// Canonical names of components declared via `#[mod_component(require(...))]`.
                pub const REQUIRED_COMPONENT_NAMES: &'static [&'static str] = &[#(#req_names),*];

                /// Deterministic UUIDs of components declared via `#[mod_component(require(...))]`.
                pub const REQUIRED_COMPONENT_UUIDS: &'static [#sys_crate::ComponentUuid] = &[#(#req_uuids),*];

                /// Compile-time C-ABI VTable constant for `#struct_ident`.
                pub const VTABLE: #vtable_ident = #vtable_ident {
                    header: #sys_crate::VTableHeader::new::<#vtable_ident, #struct_ident>(
                        <#struct_ident as #sys_crate::ModComponent>::UUID,
                    ),
                    #(#vtable_inits)*
                };

                /// Returns a static reference to this component's C-ABI VTable.
                #[inline]
                pub fn vtable() -> &'static #vtable_ident {
                    <#struct_ident as #sys_crate::ModComponent>::vtable()
                }
            }

            #conditional_raw_insertable_impl
            #conditional_abi_pod_impl

            static VTABLE_INSTANCE: #vtable_ident = #struct_ident::VTABLE;

            static SYMBOLS: &[#sys_crate::SymbolEntry] = &[
                #(#symbol_entries),*
            ];

            impl #sys_crate::ModComponent for #struct_ident {
                const CANONICAL_NAME: &'static str = #canonical_name_expr;
                const COMPONENT_CSTR: &'static ::core::ffi::CStr =
                    match ::core::ffi::CStr::from_bytes_with_nul(#canonical_cstr_bytes_expr) {
                        ::core::result::Result::Ok(s) => s,
                        ::core::result::Result::Err(_) => {
                            panic!("component canonical name contains interior NUL byte")
                        }
                    };
                const UUID: #sys_crate::ComponentUuid = #uuid_expr;
                const IS_ABI_POD: bool = #is_abi_pod;
                const IS_RAW_INSERTABLE: bool = #is_raw_insertable;
                const COMPONENT_FLAGS: u32 = #component_flags_expr;
                const C_DECL: &'static str = #c_decl_str;
                const ZIG_DECL: &'static str = #zig_decl_str;

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
