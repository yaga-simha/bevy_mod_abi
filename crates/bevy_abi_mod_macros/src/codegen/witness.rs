use crate::codegen::CodegenState;
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{Ident, Type};

#[derive(Clone)]
pub(crate) enum WitnessFieldKind {
    Bool,
    Primitive,
    HostString,
    HostVec { elem_ty: Box<Type> },
    FfiOption { inner_ty: Box<Type> },
    Nested,
}

pub(crate) struct WitnessFieldInfo {
    pub(crate) field_name: String,
    pub(crate) type_tag: String,
    pub(crate) member_tokens: TokenStream,
    pub(crate) kind: WitnessFieldKind,
    pub(crate) raw_ty: Type,
}

pub(crate) struct WitnessEnumVariantField {
    pub(crate) field_name: String,
    pub(crate) type_tag: String,
    pub(crate) rust_ident: Option<Ident>,
    pub(crate) kind: WitnessFieldKind,
    pub(crate) raw_ty: Type,
}

pub(crate) struct WitnessEnumVariant {
    pub(crate) name: String,
    pub(crate) is_unit: bool,
    pub(crate) is_named: bool,
    pub(crate) has_skipped: bool,
    pub(crate) fields: Vec<WitnessEnumVariantField>,
}

/// Emits shared internal traits & helpers inside `const _: () = { ... };` for
/// `field_info`, `export_fields`, and `import_field` witness symbols.
#[allow(clippy::too_many_lines)]
pub(crate) fn emit_witness_helpers(sys_crate: &TokenStream) -> TokenStream {
    let widen_specs: &[(&str, &[(&str, &str)])] = &[
        ("i16", &[("i8", "i8")]),
        ("i32", &[("i8", "i8"), ("i16", "i16")]),
        ("i64", &[("i8", "i8"), ("i16", "i16"), ("i32", "i32")]),
        (
            "i128",
            &[("i8", "i8"), ("i16", "i16"), ("i32", "i32"), ("i64", "i64")],
        ),
        ("u16", &[("u8", "u8")]),
        ("u32", &[("u8", "u8"), ("u16", "u16")]),
        ("u64", &[("u8", "u8"), ("u16", "u16"), ("u32", "u32")]),
        (
            "u128",
            &[("u8", "u8"), ("u16", "u16"), ("u32", "u32"), ("u64", "u64")],
        ),
        ("f64", &[("f32", "f32")]),
    ];

    let mut widen_impls = Vec::with_capacity(widen_specs.len());
    for (target_name, sources) in widen_specs {
        let target_ident = Ident::new(target_name, Span::call_site());
        let src_checks: Vec<TokenStream> = sources
            .iter()
            .map(|(src_name, src_tag)| {
                let src_ident = Ident::new(src_name, Span::call_site());
                quote! {
                    if got_tag == #src_tag {
                        if bytes.len() != ::core::mem::size_of::<#src_ident>() {
                            return #sys_crate::FfiResult::INVALID_ARGUMENT;
                        }
                        let src_val = unsafe {
                            ::core::ptr::read_unaligned(bytes.as_ptr().cast::<#src_ident>())
                        };
                        unsafe { ::core::ptr::write(self.0, src_val as #target_ident) };
                        return #sys_crate::FfiResult::OK;
                    }
                }
            })
            .collect();

        widen_impls.push(quote! {
            #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
            impl __BamImportPrimitiveWiden for &&__BamImportWrap<#target_ident> {
                unsafe fn __bam_import_bytes(
                    &self,
                    expected_tag: &str,
                    got_tag: &str,
                    bytes: &[u8],
                ) -> #sys_crate::FfiResult {
                    if got_tag == expected_tag {
                        if bytes.len() != ::core::mem::size_of::<#target_ident>() {
                            return #sys_crate::FfiResult::INVALID_ARGUMENT;
                        }
                        let val = unsafe {
                            ::core::ptr::read_unaligned(bytes.as_ptr().cast::<#target_ident>())
                        };
                        unsafe { ::core::ptr::write(self.0, val) };
                        return #sys_crate::FfiResult::OK;
                    }
                    #(#src_checks)*
                    #sys_crate::FfiResult::NOT_FOUND
                }
            }
        });
    }

    quote! {
        #[allow(dead_code)]
        struct __BamExportWrap<T>(*const T);

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        trait __BamTryModCompExport {
            unsafe fn __bam_export_nested(
                &self,
                prefix: &str,
                _type_tag: &str,
                sink: unsafe extern "C" fn(
                    *mut ::core::ffi::c_void,
                    #sys_crate::StrView,
                    #sys_crate::StrView,
                    #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult,
                sink_ctx: *mut ::core::ffi::c_void,
            ) -> #sys_crate::FfiResult;
        }

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        impl<T: #sys_crate::ModComponent> __BamTryModCompExport for &&__BamExportWrap<T> {
            unsafe fn __bam_export_nested(
                &self,
                prefix: &str,
                _type_tag: &str,
                sink: unsafe extern "C" fn(
                    *mut ::core::ffi::c_void,
                    #sys_crate::StrView,
                    #sys_crate::StrView,
                    #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult,
                sink_ctx: *mut ::core::ffi::c_void,
            ) -> #sys_crate::FfiResult {
                let mut sym = ::std::string::String::from("bam.v0.");
                sym.push_str(<T as #sys_crate::ModComponent>::CANONICAL_NAME);
                sym.push_str(".export_fields");
                let Some(fn_ptr) = <T as #sys_crate::ModComponent>::resolve_symbol(&sym) else {
                    return #sys_crate::FfiResult::OK;
                };
                type ExportFn = unsafe extern "C" fn(
                    *const ::core::ffi::c_void,
                    ::core::option::Option<
                        unsafe extern "C" fn(
                            *mut ::core::ffi::c_void,
                            #sys_crate::StrView,
                            #sys_crate::StrView,
                            #sys_crate::FfiSlice<u8>,
                        ) -> #sys_crate::FfiResult,
                    >,
                    *mut ::core::ffi::c_void,
                ) -> #sys_crate::FfiResult;
                let export_fn: ExportFn = unsafe { ::core::mem::transmute(fn_ptr) };

                struct PrefixCtx<'a> {
                    prefix: &'a str,
                    outer_sink: unsafe extern "C" fn(
                        *mut ::core::ffi::c_void,
                        #sys_crate::StrView,
                        #sys_crate::StrView,
                        #sys_crate::FfiSlice<u8>,
                    ) -> #sys_crate::FfiResult,
                    outer_ctx: *mut ::core::ffi::c_void,
                }

                unsafe extern "C" fn prefix_trampoline(
                    ctx: *mut ::core::ffi::c_void,
                    sub_name: #sys_crate::StrView,
                    type_tag: #sys_crate::StrView,
                    payload: #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult {
                    let pctx = unsafe { &*ctx.cast::<PrefixCtx<'_>>() };
                    let Ok(sub_str) = sub_name.try_as_str() else {
                        return #sys_crate::FfiResult::INVALID_UTF8;
                    };
                    let mut full = ::std::string::String::with_capacity(
                        pctx.prefix.len() + 1 + sub_str.len(),
                    );
                    full.push_str(pctx.prefix);
                    full.push('.');
                    full.push_str(sub_str);
                    unsafe {
                        (pctx.outer_sink)(
                            pctx.outer_ctx,
                            #sys_crate::StrView::from_str(&full),
                            type_tag,
                            payload,
                        )
                    }
                }

                let mut pctx = PrefixCtx {
                    prefix,
                    outer_sink: sink,
                    outer_ctx: sink_ctx,
                };
                unsafe {
                    export_fn(
                        self.0.cast::<::core::ffi::c_void>(),
                        ::core::option::Option::Some(prefix_trampoline),
                        (&raw mut pctx).cast::<::core::ffi::c_void>(),
                    )
                }
            }
        }

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        trait __BamFallbackRawExport {
            unsafe fn __bam_export_nested(
                &self,
                prefix: &str,
                type_tag: &str,
                sink: unsafe extern "C" fn(
                    *mut ::core::ffi::c_void,
                    #sys_crate::StrView,
                    #sys_crate::StrView,
                    #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult,
                sink_ctx: *mut ::core::ffi::c_void,
            ) -> #sys_crate::FfiResult;
        }

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        impl<T: #sys_crate::RawInsertable> __BamFallbackRawExport for &__BamExportWrap<T> {
            unsafe fn __bam_export_nested(
                &self,
                prefix: &str,
                type_tag: &str,
                sink: unsafe extern "C" fn(
                    *mut ::core::ffi::c_void,
                    #sys_crate::StrView,
                    #sys_crate::StrView,
                    #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult,
                sink_ctx: *mut ::core::ffi::c_void,
            ) -> #sys_crate::FfiResult {
                let size = ::core::mem::size_of::<T>();
                let bytes: &[u8] = if size == 0 {
                    &[]
                } else {
                    unsafe { ::core::slice::from_raw_parts(self.0.cast::<u8>(), size) }
                };
                unsafe {
                    sink(
                        sink_ctx,
                        #sys_crate::StrView::from_str(prefix),
                        #sys_crate::StrView::from_str(type_tag),
                        #sys_crate::FfiSlice::from_slice(bytes),
                    )
                }
            }
        }

        #[allow(dead_code)]
        trait __BamFallbackUnsupportedExport {
            unsafe fn __bam_export_nested(
                &self,
                _prefix: &str,
                _type_tag: &str,
                _sink: unsafe extern "C" fn(
                    *mut ::core::ffi::c_void,
                    #sys_crate::StrView,
                    #sys_crate::StrView,
                    #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult,
                _sink_ctx: *mut ::core::ffi::c_void,
            ) -> #sys_crate::FfiResult;
        }

        #[allow(dead_code)]
        impl<T> __BamFallbackUnsupportedExport for __BamExportWrap<T> {
            unsafe fn __bam_export_nested(
                &self,
                _prefix: &str,
                _type_tag: &str,
                _sink: unsafe extern "C" fn(
                    *mut ::core::ffi::c_void,
                    #sys_crate::StrView,
                    #sys_crate::StrView,
                    #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult,
                _sink_ctx: *mut ::core::ffi::c_void,
            ) -> #sys_crate::FfiResult {
                #sys_crate::FfiResult::UNSUPPORTED
            }
        }

        #[allow(dead_code)]
        struct __BamImportWrap<T>(*mut T);

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        trait __BamImportPrimitiveWiden {
            unsafe fn __bam_import_bytes(
                &self,
                expected_tag: &str,
                got_tag: &str,
                bytes: &[u8],
            ) -> #sys_crate::FfiResult;
        }

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        trait __BamImportRawFallback {
            unsafe fn __bam_import_bytes(
                &self,
                expected_tag: &str,
                got_tag: &str,
                bytes: &[u8],
            ) -> #sys_crate::FfiResult;
        }

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        impl<T: #sys_crate::RawInsertable> __BamImportRawFallback for &__BamImportWrap<T> {
            unsafe fn __bam_import_bytes(
                &self,
                expected_tag: &str,
                got_tag: &str,
                bytes: &[u8],
            ) -> #sys_crate::FfiResult {
                if got_tag != expected_tag {
                    return #sys_crate::FfiResult::NOT_FOUND;
                }
                let size = ::core::mem::size_of::<T>();
                if bytes.len() != size {
                    return #sys_crate::FfiResult::INVALID_ARGUMENT;
                }
                let val = unsafe { ::core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) };
                unsafe { ::core::ptr::write(self.0, val) };
                #sys_crate::FfiResult::OK
            }
        }

        #[allow(dead_code)]
        trait __BamImportUnsupportedFallback {
            unsafe fn __bam_import_bytes(
                &self,
                _expected_tag: &str,
                _got_tag: &str,
                _bytes: &[u8],
            ) -> #sys_crate::FfiResult;
        }

        #[allow(dead_code)]
        impl<T> __BamImportUnsupportedFallback for __BamImportWrap<T> {
            unsafe fn __bam_import_bytes(
                &self,
                _expected_tag: &str,
                _got_tag: &str,
                _bytes: &[u8],
            ) -> #sys_crate::FfiResult {
                #sys_crate::FfiResult::NOT_FOUND
            }
        }

        #(#widen_impls)*

        #[allow(dead_code)]
        struct __BamNestedImportWrap<T>(*mut T);

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        trait __BamTryModCompImport {
            unsafe fn __bam_import_sub(
                &self,
                sub_name: &str,
                type_tag: #sys_crate::StrView,
                payload: #sys_crate::FfiSlice<u8>,
            ) -> #sys_crate::FfiResult;
        }

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        impl<T: #sys_crate::ModComponent> __BamTryModCompImport for &&__BamNestedImportWrap<T> {
            unsafe fn __bam_import_sub(
                &self,
                sub_name: &str,
                type_tag: #sys_crate::StrView,
                payload: #sys_crate::FfiSlice<u8>,
            ) -> #sys_crate::FfiResult {
                let mut sym = ::std::string::String::from("bam.v0.");
                sym.push_str(<T as #sys_crate::ModComponent>::CANONICAL_NAME);
                sym.push_str(".import_field");
                let Some(fn_ptr) = <T as #sys_crate::ModComponent>::resolve_symbol(&sym) else {
                    return #sys_crate::FfiResult::NOT_FOUND;
                };
                type ImportFn = unsafe extern "C" fn(
                    *mut ::core::ffi::c_void,
                    #sys_crate::StrView,
                    #sys_crate::StrView,
                    #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult;
                let import_fn: ImportFn = unsafe { ::core::mem::transmute(fn_ptr) };
                unsafe {
                    import_fn(
                        self.0.cast::<::core::ffi::c_void>(),
                        #sys_crate::StrView::from_str(sub_name),
                        type_tag,
                        payload,
                    )
                }
            }
        }

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        trait __BamFallbackNestedImport {
            unsafe fn __bam_import_sub(
                &self,
                _sub_name: &str,
                _type_tag: #sys_crate::StrView,
                _payload: #sys_crate::FfiSlice<u8>,
            ) -> #sys_crate::FfiResult;
        }

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        impl<T> __BamFallbackNestedImport for &__BamNestedImportWrap<T> {
            unsafe fn __bam_import_sub(
                &self,
                _sub_name: &str,
                _type_tag: #sys_crate::StrView,
                _payload: #sys_crate::FfiSlice<u8>,
            ) -> #sys_crate::FfiResult {
                #sys_crate::FfiResult::NOT_FOUND
            }
        }

        #[allow(dead_code)]
        struct __BamDefaultWrap<T>(::core::marker::PhantomData<T>);

        #[allow(dead_code)]
        trait __BamTryDefault<T> {
            fn __bam_try_default(&self) -> ::core::option::Option<T>;
        }

        #[allow(dead_code)]
        impl<T: ::core::default::Default> __BamTryDefault<T> for &&__BamDefaultWrap<T> {
            fn __bam_try_default(&self) -> ::core::option::Option<T> {
                ::core::option::Option::Some(<T as ::core::default::Default>::default())
            }
        }

        #[allow(dead_code)]
        trait __BamTryAbiPodZeroed<T> {
            fn __bam_try_default(&self) -> ::core::option::Option<T>;
        }

        #[allow(dead_code, clippy::undocumented_unsafe_blocks)]
        impl<T: #sys_crate::AbiPod> __BamTryAbiPodZeroed<T> for &__BamDefaultWrap<T> {
            fn __bam_try_default(&self) -> ::core::option::Option<T> {
                ::core::option::Option::Some(unsafe {
                    ::core::mem::MaybeUninit::<T>::zeroed().assume_init()
                })
            }
        }

        #[allow(dead_code)]
        trait __BamFallbackNoDefault<T> {
            fn __bam_try_default(&self) -> ::core::option::Option<T>;
        }

        #[allow(dead_code)]
        impl<T> __BamFallbackNoDefault<T> for __BamDefaultWrap<T> {
            fn __bam_try_default(&self) -> ::core::option::Option<T> {
                ::core::option::Option::None
            }
        }
    }
}

fn emit_export_field_stmt(
    sys_crate: &TokenStream,
    field_name: &str,
    type_tag: &str,
    kind: &WitnessFieldKind,
    raw_ty: &Type,
    ptr_expr: TokenStream,
) -> TokenStream {
    match kind {
        WitnessFieldKind::Bool => quote! {
            {
                let val: bool = unsafe { ::core::ptr::read(#ptr_expr) };
                let byte: u8 = u8::from(val);
                let rc = unsafe {
                    sink_fn(
                        sink_ctx,
                        #sys_crate::StrView::from_str(#field_name),
                        #sys_crate::StrView::from_str("bool"),
                        #sys_crate::FfiSlice::from_slice(::core::slice::from_ref(&byte)),
                    )
                };
                if rc.is_err() {
                    return rc;
                }
            }
        },
        WitnessFieldKind::Primitive => quote! {
            {
                let fptr: *const #raw_ty = #ptr_expr;
                let size = ::core::mem::size_of::<#raw_ty>();
                let bytes: &[u8] = if size == 0 {
                    &[]
                } else {
                    unsafe { ::core::slice::from_raw_parts(fptr.cast::<u8>(), size) }
                };
                let rc = unsafe {
                    sink_fn(
                        sink_ctx,
                        #sys_crate::StrView::from_str(#field_name),
                        #sys_crate::StrView::from_str(#type_tag),
                        #sys_crate::FfiSlice::from_slice(bytes),
                    )
                };
                if rc.is_err() {
                    return rc;
                }
            }
        },
        WitnessFieldKind::HostString => quote! {
            {
                let str_ptr = <#raw_ty as __BamStdString>::cast_const_ptr(#ptr_expr);
                let s_ref: &::std::string::String = unsafe { &*str_ptr };
                let rc = unsafe {
                    sink_fn(
                        sink_ctx,
                        #sys_crate::StrView::from_str(#field_name),
                        #sys_crate::StrView::from_str("String"),
                        #sys_crate::FfiSlice::from_slice(s_ref.as_bytes()),
                    )
                };
                if rc.is_err() {
                    return rc;
                }
            }
        },
        WitnessFieldKind::HostVec { elem_ty } => quote! {
            {
                let vec_ptr = <#raw_ty as __BamStdVec<#elem_ty>>::cast_const_ptr(#ptr_expr);
                let vec_ref: &::std::vec::Vec<#elem_ty> = unsafe { &*vec_ptr };
                let elem_size = ::core::mem::size_of::<#elem_ty>();
                let total_bytes = vec_ref.len().saturating_mul(elem_size);
                let bytes: &[u8] = if total_bytes == 0 {
                    &[]
                } else {
                    unsafe { ::core::slice::from_raw_parts(vec_ref.as_ptr().cast::<u8>(), total_bytes) }
                };
                let rc = unsafe {
                    sink_fn(
                        sink_ctx,
                        #sys_crate::StrView::from_str(#field_name),
                        #sys_crate::StrView::from_str(#type_tag),
                        #sys_crate::FfiSlice::from_slice(bytes),
                    )
                };
                if rc.is_err() {
                    return rc;
                }
            }
        },
        WitnessFieldKind::FfiOption { inner_ty } => quote! {
            {
                let opt_ptr = <#raw_ty as __BamFfiOption<#inner_ty>>::cast_const_ptr(#ptr_expr);
                let opt_ref: &#sys_crate::FfiOption<#inner_ty> = unsafe { &*opt_ptr };
                let rc = match opt_ref.as_ref() {
                    ::core::option::Option::None => {
                        let tag_none: [u8; 1] = [0u8];
                        unsafe {
                            sink_fn(
                                sink_ctx,
                                #sys_crate::StrView::from_str(#field_name),
                                #sys_crate::StrView::from_str(#type_tag),
                                #sys_crate::FfiSlice::from_slice(&tag_none),
                            )
                        }
                    }
                    ::core::option::Option::Some(inner_ref) => {
                        let inner_size = ::core::mem::size_of::<#inner_ty>();
                        let mut buf = ::std::vec::Vec::<u8>::with_capacity(1usize + inner_size);
                        buf.push(1u8);
                        if inner_size > 0 {
                            let inner_bytes: &[u8] = unsafe {
                                ::core::slice::from_raw_parts(
                                    (inner_ref as *const #inner_ty).cast::<u8>(),
                                    inner_size,
                                )
                            };
                            buf.extend_from_slice(inner_bytes);
                        }
                        unsafe {
                            sink_fn(
                                sink_ctx,
                                #sys_crate::StrView::from_str(#field_name),
                                #sys_crate::StrView::from_str(#type_tag),
                                #sys_crate::FfiSlice::from_slice(buf.as_slice()),
                            )
                        }
                    }
                };
                if rc.is_err() {
                    return rc;
                }
            }
        },
        WitnessFieldKind::Nested => quote! {
            {
                let fptr: *const #raw_ty = #ptr_expr;
                let wrap = __BamExportWrap::<#raw_ty>(fptr);
                let rc = unsafe {
                    (&&&wrap).__bam_export_nested(#field_name, #type_tag, sink_fn, sink_ctx)
                };
                if rc.is_err() {
                    return rc;
                }
            }
        },
    }
}

fn emit_import_field_body(
    sys_crate: &TokenStream,
    type_tag: &str,
    kind: &WitnessFieldKind,
    raw_ty: &Type,
    mut_ptr_expr: TokenStream,
) -> TokenStream {
    match kind {
        WitnessFieldKind::Bool => quote! {
            if tag_str != "bool" {
                return #sys_crate::FfiResult::NOT_FOUND;
            }
            if bytes.len() != 1 {
                return #sys_crate::FfiResult::INVALID_ARGUMENT;
            }
            let ptr_mut = <#raw_ty as __BamPrimBool>::cast_mut_ptr(#mut_ptr_expr);
            unsafe { ::core::ptr::write(ptr_mut, bytes[0] != 0) };
            #sys_crate::FfiResult::OK
        },
        WitnessFieldKind::Primitive | WitnessFieldKind::Nested => quote! {
            let wrap = __BamImportWrap::<#raw_ty>(#mut_ptr_expr);
            unsafe { (&&&wrap).__bam_import_bytes(#type_tag, tag_str, bytes) }
        },
        WitnessFieldKind::HostString => quote! {
            if tag_str != "String" {
                return #sys_crate::FfiResult::NOT_FOUND;
            }
            let valid_str = match ::core::str::from_utf8(bytes) {
                ::core::result::Result::Ok(s) => s,
                ::core::result::Result::Err(_) => return #sys_crate::FfiResult::INVALID_UTF8,
            };
            let str_ptr = <#raw_ty as __BamStdString>::cast_mut_ptr(#mut_ptr_expr);
            let new_str = ::std::string::String::from(valid_str);
            unsafe {
                *str_ptr = new_str;
            }
            #sys_crate::FfiResult::OK
        },
        WitnessFieldKind::HostVec { elem_ty } => quote! {
            if tag_str != #type_tag {
                return #sys_crate::FfiResult::NOT_FOUND;
            }
            let elem_size = ::core::mem::size_of::<#elem_ty>();
            let vec_ptr = <#raw_ty as __BamStdVec<#elem_ty>>::cast_mut_ptr(#mut_ptr_expr);
            if elem_size == 0 {
                if !bytes.is_empty() {
                    return #sys_crate::FfiResult::INVALID_ARGUMENT;
                }
                unsafe { (*vec_ptr).clear() };
                return #sys_crate::FfiResult::OK;
            }
            if !bytes.len().is_multiple_of(elem_size) {
                return #sys_crate::FfiResult::INVALID_ARGUMENT;
            }
            let count = bytes.len() / elem_size;
            let mut new_vec = ::std::vec::Vec::<#elem_ty>::new();
            if new_vec.try_reserve_exact(count).is_err() {
                return #sys_crate::FfiResult::CAPACITY_EXCEEDED;
            }
            let mut idx = 0usize;
            while idx < count {
                let elem_ptr = unsafe { bytes.as_ptr().add(idx * elem_size).cast::<#elem_ty>() };
                let elem = unsafe { ::core::ptr::read_unaligned(elem_ptr) };
                new_vec.push(elem);
                idx += 1;
            }
            unsafe {
                *vec_ptr = new_vec;
            }
            #sys_crate::FfiResult::OK
        },
        WitnessFieldKind::FfiOption { inner_ty } => quote! {
            if tag_str != #type_tag {
                return #sys_crate::FfiResult::NOT_FOUND;
            }
            if bytes.is_empty() {
                return #sys_crate::FfiResult::INVALID_ARGUMENT;
            }
            let opt_ptr = <#raw_ty as __BamFfiOption<#inner_ty>>::cast_mut_ptr(#mut_ptr_expr);
            if bytes[0] == 0 {
                if bytes.len() != 1 {
                    return #sys_crate::FfiResult::INVALID_ARGUMENT;
                }
                unsafe {
                    ::core::ptr::write(opt_ptr, __bam_none_option::<#inner_ty>());
                }
                return #sys_crate::FfiResult::OK;
            }
            let inner_size = ::core::mem::size_of::<#inner_ty>();
            if bytes.len() != 1usize + inner_size {
                return #sys_crate::FfiResult::INVALID_ARGUMENT;
            }
            let inner_val = unsafe {
                ::core::ptr::read_unaligned(bytes[1..].as_ptr().cast::<#inner_ty>())
            };
            unsafe {
                ::core::ptr::write(opt_ptr, __bam_some_option::<#inner_ty>(inner_val));
            }
            #sys_crate::FfiResult::OK
        },
    }
}

fn register_witness_symbol(
    state: &mut CodegenState,
    slot_str: &str,
    fn_ident: &Ident,
) -> TokenStream {
    let sys_crate = &state.sys_crate;
    let sym_expr = state.canonical_name.symbol_name_expr(slot_str);
    state.symbol_entries.push(quote! {
        #sys_crate::SymbolEntry {
            name: #sym_expr,
            ptr: #fn_ident as *const ::core::ffi::c_void,
        }
    });

    if state.export_symbols {
        let export_name = format!("{}_{slot_str}", state.export_fn_prefix);
        if state.used_export_names.insert(export_name.clone()) {
            return quote! {
                #[allow(unreachable_pub)]
                #[unsafe(export_name = #export_name)]
                pub
            };
        }
    }
    quote! {}
}

#[allow(clippy::too_many_lines)]
pub(crate) fn emit_struct_witnesses(state: &mut CodegenState) {
    if state.opaque {
        return;
    }

    let sys_crate = state.sys_crate.clone();
    let struct_ident = state.struct_ident.clone();

    let field_info_ident = format_ident!("__bam_{}_field_info", struct_ident);
    let export_fields_ident = format_ident!("__bam_{}_export_fields", struct_ident);
    let import_field_ident = format_ident!("__bam_{}_import_field", struct_ident);

    let field_info_export = register_witness_symbol(state, "field_info", &field_info_ident);
    let export_fields_export =
        register_witness_symbol(state, "export_fields", &export_fields_ident);
    let import_field_export = register_witness_symbol(state, "import_field", &import_field_ident);

    // 1. `field_info`
    let mut info_arms = Vec::with_capacity(state.witness_fields.len());
    for (idx, wf) in state.witness_fields.iter().enumerate() {
        let f_name = &wf.field_name;
        let t_tag = &wf.type_tag;
        let member = &wf.member_tokens;
        info_arms.push(quote! {
            #idx => {
                unsafe {
                    ::core::ptr::write(out_name, #sys_crate::StrView::from_str(#f_name));
                    ::core::ptr::write(out_type_tag, #sys_crate::StrView::from_str(#t_tag));
                    ::core::ptr::write(out_offset, ::core::mem::offset_of!(#struct_ident, #member));
                }
                #sys_crate::FfiBool::TRUE
            }
        });
    }

    state.extern_fns.push(quote! {
        #[allow(
            non_snake_case,
            clippy::undocumented_unsafe_blocks,
            clippy::ptr_as_ptr
        )]
        #field_info_export
        unsafe extern "C" fn #field_info_ident(
            index: usize,
            out_name: *mut #sys_crate::StrView,
            out_type_tag: *mut #sys_crate::StrView,
            out_offset: *mut usize,
        ) -> #sys_crate::FfiBool {
            if out_name.is_null() || out_type_tag.is_null() || out_offset.is_null() {
                return #sys_crate::FfiBool::FALSE;
            }
            if !out_name.is_aligned() || !out_type_tag.is_aligned() || !out_offset.is_aligned() {
                return #sys_crate::FfiBool::FALSE;
            }
            match index {
                #(#info_arms)*
                _ => #sys_crate::FfiBool::FALSE,
            }
        }
    });

    // 2. `export_fields`
    let mut export_stmts = Vec::with_capacity(state.witness_fields.len());
    for wf in &state.witness_fields {
        let member = &wf.member_tokens;
        let stmt = emit_export_field_stmt(
            &sys_crate,
            &wf.field_name,
            &wf.type_tag,
            &wf.kind,
            &wf.raw_ty,
            quote! { ::core::ptr::addr_of!((*typed_ptr).#member) },
        );
        export_stmts.push(stmt);
    }

    state.extern_fns.push(quote! {
        #[allow(
            non_snake_case,
            clippy::undocumented_unsafe_blocks,
            clippy::ptr_as_ptr,
            clippy::cast_ptr_alignment
        )]
        #export_fields_export
        unsafe extern "C" fn #export_fields_ident(
            src_ptr: *const ::core::ffi::c_void,
            sink: ::core::option::Option<
                unsafe extern "C" fn(
                    *mut ::core::ffi::c_void,
                    #sys_crate::StrView,
                    #sys_crate::StrView,
                    #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult,
            >,
            sink_ctx: *mut ::core::ffi::c_void,
        ) -> #sys_crate::FfiResult {
            if src_ptr.is_null() {
                return #sys_crate::FfiResult::NULL_POINTER;
            }
            let typed_ptr = src_ptr.cast::<#struct_ident>();
            if !typed_ptr.is_aligned() {
                return #sys_crate::FfiResult::INVALID_ARGUMENT;
            }
            let ::core::option::Option::Some(sink_fn) = sink else {
                return #sys_crate::FfiResult::NULL_POINTER;
            };
            #(#export_stmts)*
            #sys_crate::FfiResult::OK
        }
    });

    // 3. `import_field`
    let mut import_direct_arms = Vec::new();
    let mut import_nested_checks = Vec::new();
    for wf in &state.witness_fields {
        let f_name = &wf.field_name;
        let member = &wf.member_tokens;
        let body = emit_import_field_body(
            &sys_crate,
            &wf.type_tag,
            &wf.kind,
            &wf.raw_ty,
            quote! { ::core::ptr::addr_of_mut!((*typed_ptr).#member) },
        );
        import_direct_arms.push(quote! {
            #f_name => {
                #body
            }
        });
        if matches!(wf.kind, WitnessFieldKind::Nested) {
            let raw_ty = &wf.raw_ty;
            let prefix_dot = format!("{f_name}.");
            import_nested_checks.push(quote! {
                if let ::core::option::Option::Some(sub_name) = name_str.strip_prefix(#prefix_dot) {
                    let wrap = __BamNestedImportWrap::<#raw_ty>(
                        ::core::ptr::addr_of_mut!((*typed_ptr).#member),
                    );
                    return unsafe { (&&&wrap).__bam_import_sub(sub_name, type_tag, payload) };
                }
            });
        }
    }

    state.extern_fns.push(quote! {
        #[allow(
            non_snake_case,
            clippy::undocumented_unsafe_blocks,
            clippy::ptr_as_ptr,
            clippy::cast_ptr_alignment
        )]
        #import_field_export
        unsafe extern "C" fn #import_field_ident(
            dst_ptr: *mut ::core::ffi::c_void,
            field_name: #sys_crate::StrView,
            type_tag: #sys_crate::StrView,
            payload: #sys_crate::FfiSlice<u8>,
        ) -> #sys_crate::FfiResult {
            if dst_ptr.is_null() {
                return #sys_crate::FfiResult::NULL_POINTER;
            }
            let typed_ptr = dst_ptr.cast::<#struct_ident>();
            if !typed_ptr.is_aligned() {
                return #sys_crate::FfiResult::INVALID_ARGUMENT;
            }
            let s_name = field_name.validate();
            if s_name.is_err() {
                return s_name;
            }
            let s_tag = type_tag.validate();
            if s_tag.is_err() {
                return s_tag;
            }
            let s_pay = payload.validate();
            if s_pay.is_err() {
                return s_pay;
            }
            let ::core::result::Result::Ok(name_str) = field_name.try_as_str() else {
                return #sys_crate::FfiResult::INVALID_UTF8;
            };
            let ::core::result::Result::Ok(tag_str) = type_tag.try_as_str() else {
                return #sys_crate::FfiResult::INVALID_UTF8;
            };
            let bytes: &[u8] = if payload.len == 0 {
                &[]
            } else {
                unsafe { payload.as_slice() }
            };

            match name_str {
                #(#import_direct_arms)*
                _ => {
                    #(#import_nested_checks)*
                    #sys_crate::FfiResult::NOT_FOUND
                }
            }
        }
    });
}

#[allow(clippy::too_many_lines)]
pub(crate) fn emit_enum_witnesses(state: &mut CodegenState, variants: &[WitnessEnumVariant]) {
    if state.opaque {
        return;
    }

    let sys_crate = state.sys_crate.clone();
    let enum_ident = state.struct_ident.clone();

    let field_info_ident = format_ident!("__bam_{}_field_info", enum_ident);
    let export_fields_ident = format_ident!("__bam_{}_export_fields", enum_ident);
    let import_field_ident = format_ident!("__bam_{}_import_field", enum_ident);

    let field_info_export = register_witness_symbol(state, "field_info", &field_info_ident);
    let export_fields_export =
        register_witness_symbol(state, "export_fields", &export_fields_ident);
    let import_field_export = register_witness_symbol(state, "import_field", &import_field_ident);

    // 1. `field_info`: index 0 -> ("__variant", "enum_variant", 0), index 1.. -> ("<Var>::<field>", "<tag>", 0)
    let mut info_arms = Vec::new();
    info_arms.push(quote! {
        0usize => {
            unsafe {
                ::core::ptr::write(out_name, #sys_crate::StrView::from_str("__variant"));
                ::core::ptr::write(out_type_tag, #sys_crate::StrView::from_str("enum_variant"));
                ::core::ptr::write(out_offset, 0usize);
            }
            #sys_crate::FfiBool::TRUE
        }
    });
    let mut next_idx = 1usize;
    for var in variants {
        for f in &var.fields {
            let full = format!("{}::{}", var.name, f.field_name);
            let t_tag = &f.type_tag;
            info_arms.push(quote! {
                #next_idx => {
                    unsafe {
                        ::core::ptr::write(out_name, #sys_crate::StrView::from_str(#full));
                        ::core::ptr::write(out_type_tag, #sys_crate::StrView::from_str(#t_tag));
                        ::core::ptr::write(out_offset, 0usize);
                    }
                    #sys_crate::FfiBool::TRUE
                }
            });
            next_idx += 1;
        }
    }

    state.extern_fns.push(quote! {
        #[allow(
            non_snake_case,
            clippy::undocumented_unsafe_blocks,
            clippy::ptr_as_ptr
        )]
        #field_info_export
        unsafe extern "C" fn #field_info_ident(
            index: usize,
            out_name: *mut #sys_crate::StrView,
            out_type_tag: *mut #sys_crate::StrView,
            out_offset: *mut usize,
        ) -> #sys_crate::FfiBool {
            if out_name.is_null() || out_type_tag.is_null() || out_offset.is_null() {
                return #sys_crate::FfiBool::FALSE;
            }
            if !out_name.is_aligned() || !out_type_tag.is_aligned() || !out_offset.is_aligned() {
                return #sys_crate::FfiBool::FALSE;
            }
            match index {
                #(#info_arms)*
                _ => #sys_crate::FfiBool::FALSE,
            }
        }
    });

    // 2. `export_fields`
    let mut export_arms = Vec::with_capacity(variants.len());
    for var in variants {
        let v_ident = Ident::new(&var.name, Span::call_site());
        let v_name = &var.name;
        let emit_variant_tag = quote! {
            let rc = unsafe {
                sink_fn(
                    sink_ctx,
                    #sys_crate::StrView::from_str("__variant"),
                    #sys_crate::StrView::from_str("enum_variant"),
                    #sys_crate::FfiSlice::from_slice(#v_name.as_bytes()),
                )
            };
            if rc.is_err() {
                return rc;
            }
        };

        if var.is_unit {
            export_arms.push(quote! {
                #enum_ident::#v_ident => {
                    #emit_variant_tag
                }
            });
        } else if var.is_named {
            let mut bindings = Vec::new();
            let mut stmts = Vec::new();
            for (idx, f) in var.fields.iter().enumerate() {
                let r_ident = f.rust_ident.as_ref().expect("named field");
                let b_ident = format_ident!("__bf_{idx}");
                bindings.push(quote! { #r_ident: #b_ident });
                let full_name = format!("{v_name}::{}", f.field_name);
                stmts.push(emit_export_field_stmt(
                    &sys_crate,
                    &full_name,
                    &f.type_tag,
                    &f.kind,
                    &f.raw_ty,
                    quote! { &raw const *#b_ident },
                ));
            }
            let pat = if var.has_skipped {
                quote! { #enum_ident::#v_ident { #(#bindings,)* .. } }
            } else {
                quote! { #enum_ident::#v_ident { #(#bindings),* } }
            };
            export_arms.push(quote! {
                #pat => {
                    #emit_variant_tag
                    #(#stmts)*
                }
            });
        } else {
            let mut pat_elems = Vec::new();
            let mut stmts = Vec::new();
            for (idx, f) in var.fields.iter().enumerate() {
                let b_ident = format_ident!("__bf_{idx}");
                pat_elems.push(quote! { #b_ident });
                let full_name = format!("{v_name}::{}", f.field_name);
                stmts.push(emit_export_field_stmt(
                    &sys_crate,
                    &full_name,
                    &f.type_tag,
                    &f.kind,
                    &f.raw_ty,
                    quote! { &raw const *#b_ident },
                ));
            }
            let pat = if var.has_skipped {
                quote! { #enum_ident::#v_ident(#(#pat_elems,)* ..) }
            } else {
                quote! { #enum_ident::#v_ident(#(#pat_elems),*) }
            };
            export_arms.push(quote! {
                #pat => {
                    #emit_variant_tag
                    #(#stmts)*
                }
            });
        }
    }

    state.extern_fns.push(quote! {
        #[allow(
            non_snake_case,
            clippy::undocumented_unsafe_blocks,
            clippy::ptr_as_ptr,
            clippy::cast_ptr_alignment
        )]
        #export_fields_export
        unsafe extern "C" fn #export_fields_ident(
            src_ptr: *const ::core::ffi::c_void,
            sink: ::core::option::Option<
                unsafe extern "C" fn(
                    *mut ::core::ffi::c_void,
                    #sys_crate::StrView,
                    #sys_crate::StrView,
                    #sys_crate::FfiSlice<u8>,
                ) -> #sys_crate::FfiResult,
            >,
            sink_ctx: *mut ::core::ffi::c_void,
        ) -> #sys_crate::FfiResult {
            if src_ptr.is_null() {
                return #sys_crate::FfiResult::NULL_POINTER;
            }
            let typed_ptr = src_ptr.cast::<#enum_ident>();
            if !typed_ptr.is_aligned() {
                return #sys_crate::FfiResult::INVALID_ARGUMENT;
            }
            let ::core::option::Option::Some(sink_fn) = sink else {
                return #sys_crate::FfiResult::NULL_POINTER;
            };
            match unsafe { &*typed_ptr } {
                #(#export_arms)*
            }
            #sys_crate::FfiResult::OK
        }
    });

    // 3. `import_field`
    let variant_count = variants.len();
    let mut variant_switch_arms = Vec::new();
    let mut field_import_arms = Vec::new();
    let mut enum_nested_checks = Vec::new();

    for var in variants {
        let v_ident = Ident::new(&var.name, Span::call_site());
        let v_name = &var.name;

        if !var.has_skipped {
            if var.is_unit {
                variant_switch_arms.push(quote! {
                    #v_name => {
                        unsafe { *typed_ptr = #enum_ident::#v_ident };
                        #sys_crate::FfiResult::OK
                    }
                });
            } else {
                let mut defaults = Vec::new();
                let mut ctor_args = Vec::new();
                for (idx, f) in var.fields.iter().enumerate() {
                    let f_ty = &f.raw_ty;
                    let d_ident = format_ident!("__d_{idx}");
                    defaults.push(quote! {
                        let ::core::option::Option::Some(#d_ident) =
                            (&&&__BamDefaultWrap::<#f_ty>(::core::marker::PhantomData)).__bam_try_default()
                        else {
                            return #sys_crate::FfiResult::UNSUPPORTED;
                        };
                    });
                    if let Some(ref r_ident) = f.rust_ident {
                        ctor_args.push(quote! { #r_ident: #d_ident });
                    } else {
                        ctor_args.push(quote! { #d_ident });
                    }
                }
                let ctor = if var.is_named {
                    quote! { #enum_ident::#v_ident { #(#ctor_args),* } }
                } else {
                    quote! { #enum_ident::#v_ident(#(#ctor_args),*) }
                };
                variant_switch_arms.push(quote! {
                    #v_name => {
                        #(#defaults)*
                        unsafe { *typed_ptr = #ctor };
                        #sys_crate::FfiResult::OK
                    }
                });
            }
        }

        for (idx, f) in var.fields.iter().enumerate() {
            let full_name = format!("{v_name}::{}", f.field_name);
            let pat = if var.is_named {
                let r_ident = f.rust_ident.as_ref().expect("named field");
                quote! { #enum_ident::#v_ident { #r_ident: binding, .. } }
            } else {
                let mut pats = Vec::new();
                for i in 0..var.fields.len() {
                    if i == idx {
                        pats.push(quote! { binding });
                    } else {
                        pats.push(quote! { _ });
                    }
                }
                if var.has_skipped {
                    quote! { #enum_ident::#v_ident(#(#pats,)* ..) }
                } else {
                    quote! { #enum_ident::#v_ident(#(#pats),*) }
                }
            };
            let body = emit_import_field_body(
                &sys_crate,
                &f.type_tag,
                &f.kind,
                &f.raw_ty,
                quote! { &raw mut *binding },
            );
            let mismatch_arm = if variant_count > 1 {
                quote! { _ => #sys_crate::FfiResult::INVALID_STATE, }
            } else {
                quote! {}
            };
            field_import_arms.push(quote! {
                #full_name => {
                    match unsafe { &mut *typed_ptr } {
                        #pat => {
                            #body
                        }
                        #mismatch_arm
                    }
                }
            });
            if matches!(f.kind, WitnessFieldKind::Nested) {
                let raw_ty = &f.raw_ty;
                let prefix_dot = format!("{full_name}.");
                enum_nested_checks.push(quote! {
                    if let ::core::option::Option::Some(sub_name) = name_str.strip_prefix(#prefix_dot) {
                        return match unsafe { &mut *typed_ptr } {
                            #pat => {
                                let wrap = __BamNestedImportWrap::<#raw_ty>(&raw mut *binding);
                                unsafe { (&&&wrap).__bam_import_sub(sub_name, type_tag, payload) }
                            }
                            #mismatch_arm
                        };
                    }
                });
            }
        }
    }

    state.extern_fns.push(quote! {
        #[allow(
            non_snake_case,
            clippy::undocumented_unsafe_blocks,
            clippy::ptr_as_ptr,
            clippy::cast_ptr_alignment
        )]
        #import_field_export
        unsafe extern "C" fn #import_field_ident(
            dst_ptr: *mut ::core::ffi::c_void,
            field_name: #sys_crate::StrView,
            type_tag: #sys_crate::StrView,
            payload: #sys_crate::FfiSlice<u8>,
        ) -> #sys_crate::FfiResult {
            if dst_ptr.is_null() {
                return #sys_crate::FfiResult::NULL_POINTER;
            }
            let typed_ptr = dst_ptr.cast::<#enum_ident>();
            if !typed_ptr.is_aligned() {
                return #sys_crate::FfiResult::INVALID_ARGUMENT;
            }
            let s_name = field_name.validate();
            if s_name.is_err() {
                return s_name;
            }
            let s_tag = type_tag.validate();
            if s_tag.is_err() {
                return s_tag;
            }
            let s_pay = payload.validate();
            if s_pay.is_err() {
                return s_pay;
            }
            let ::core::result::Result::Ok(name_str) = field_name.try_as_str() else {
                return #sys_crate::FfiResult::INVALID_UTF8;
            };
            let ::core::result::Result::Ok(tag_str) = type_tag.try_as_str() else {
                return #sys_crate::FfiResult::INVALID_UTF8;
            };
            let bytes: &[u8] = if payload.len == 0 {
                &[]
            } else {
                unsafe { payload.as_slice() }
            };

            if name_str == "__variant" {
                if tag_str != "enum_variant" {
                    return #sys_crate::FfiResult::NOT_FOUND;
                }
                let ::core::result::Result::Ok(var_name) = ::core::str::from_utf8(bytes) else {
                    return #sys_crate::FfiResult::INVALID_UTF8;
                };
                return match var_name {
                    #(#variant_switch_arms)*
                    _ => #sys_crate::FfiResult::NOT_FOUND,
                };
            }

            match name_str {
                #(#field_import_arms)*
                _ => {
                    #(#enum_nested_checks)*
                    #sys_crate::FfiResult::NOT_FOUND
                }
            }
        }
    });
}
