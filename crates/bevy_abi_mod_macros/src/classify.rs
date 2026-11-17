//! Syntactic field type classification and explicit `#[mod_field(...)]` override handling.
//!
//! Because Rust procedural macros inspect token syntax prior to type resolution, [`classify_type`]
//! follows a two-layer soundness protocol:
//! 1. **Explicit `#[mod_field(...)]` Always Wins:** Annotating a field with `#[mod_field(skip)]`,
//!    `#[mod_field(bool)]`, `#[mod_field(primitive)]`, `#[mod_field(string)]`,
//!    `#[mod_field(vec)]` / `#[mod_field(vec(ElemTy))]`, or `#[mod_field(nested)]` unconditionally
//!    selects that accessor strategy (enabling type aliases such as `type Label = String;`).
//! 2. **Compile-Time Type & Trait Guards:** Every classified field emits a spanned compile-time
//!    type or trait assertion (`__BamPrimBool`, `assert_abi_pod` / `assert_raw_insertable`,
//!    `__BamStdString`, `__BamStdVec`) so a user-defined type that happens to share a syntactic
//!    name such as `struct String;` or `struct Vec<T>(T);` fails to compile cleanly instead of
//!    generating unsound accessors.

use crate::attrs::FieldAttrOverride;
use quote::quote;
use syn::{GenericArgument, PathArguments, Type};

/// Classification of a struct field for C-ABI VTable accessor generation.
pub(crate) enum FieldKind {
    /// Skip generating C-ABI accessors for this field (`#[mod_field(skip)]`).
    Skip,
    /// Rust `bool` field exposed across `extern "C"` as `FfiBool`.
    Bool(Box<Type>),
    /// By-value primitive (`AbiPod + Default` when `is_abi_pod = true`, or `usize`/`isize`
    /// implementing `RawInsertable + Copy + Default` when `is_abi_pod = false`).
    Primitive { ty: Box<Type>, is_abi_pod: bool },
    /// Heap-allocated `::std::string::String` owned by the host allocator.
    HostString(Box<Type>),
    /// Heap-allocated `::std::vec::Vec<T>` owned by the host allocator (`T: AbiPod`).
    HostVec {
        field_ty: Box<Type>,
        elem_ty: Box<Type>,
    },
    /// Nested compound struct or fixed array exposed via `*const c_void` / `*mut c_void` pointers.
    Nested {
        ty: Box<Type>,
        is_abi_pod_candidate: bool,
    },
}

fn match_unqualified_or_std_primitive(ty: &Type, target: &str) -> bool {
    if let Type::Path(tp) = ty
        && tp.qself.is_none()
    {
        if tp.path.is_ident(target) {
            return true;
        }
        let segs = &tp.path.segments;
        if segs.len() == 3
            && (segs[0].ident == "core" || segs[0].ident == "std")
            && segs[1].ident == "primitive"
            && segs[2].ident == target
            && segs[2].arguments.is_empty()
        {
            return true;
        }
    }
    false
}

/// Returns `true` if `ty` syntactically spells Rust's primitive `bool`.
pub(crate) fn is_bool_type(ty: &Type) -> bool {
    match_unqualified_or_std_primitive(ty, "bool")
}

/// Returns `true` if `ty` syntactically spells `usize` or `isize` (`RawInsertable` but not `AbiPod`).
pub(crate) fn is_platform_size_int(ty: &Type) -> bool {
    match_unqualified_or_std_primitive(ty, "usize")
        || match_unqualified_or_std_primitive(ty, "isize")
}

/// Returns `true` if `ty` syntactically spells `u128` or `i128` (`RawInsertable` but not `AbiPod`
/// and not FFI-safe by value across `extern "C"`).
pub(crate) fn is_128bit_int(ty: &Type) -> bool {
    match_unqualified_or_std_primitive(ty, "u128") || match_unqualified_or_std_primitive(ty, "i128")
}

/// Extracts a deterministic, whitespace-normalized field type tag for
/// `ComponentUuid::hash_field_schema` on `#[repr(C)]` `AbiPod` structs.
pub(crate) fn canonical_field_type_tag(ty: &Type) -> String {
    match ty {
        Type::Path(tp) if tp.qself.is_none() => {
            if let Some(last) = tp.path.segments.last()
                && last.arguments.is_empty()
            {
                return last.ident.to_string();
            }
            quote!(#ty).to_string().replace(' ', "")
        }
        Type::Array(arr) => {
            let elem = canonical_field_type_tag(&arr.elem);
            let len_tokens = &arr.len;
            let len_str = quote!(#len_tokens).to_string().replace(' ', "");
            format!("[{elem};{len_str}]")
        }
        _ => quote!(#ty).to_string().replace(' ', ""),
    }
}

/// Classifies a struct field into bool, primitive, string, vector, nested, or skipped representation.
///
/// Explicit `#[mod_field(...)]` overrides always take precedence over syntactic path inspection.
pub(crate) fn classify_type(ty: &Type, attr_override: FieldAttrOverride) -> syn::Result<FieldKind> {
    let boxed_ty = || Box::new(ty.clone());
    match attr_override {
        FieldAttrOverride::Skip => return Ok(FieldKind::Skip),
        FieldAttrOverride::Bool => return Ok(FieldKind::Bool(boxed_ty())),
        FieldAttrOverride::Primitive => {
            if is_bool_type(ty) {
                return Ok(FieldKind::Bool(boxed_ty()));
            }
            return Ok(FieldKind::Primitive {
                ty: boxed_ty(),
                is_abi_pod: !is_platform_size_int(ty),
            });
        }
        FieldAttrOverride::HostString => return Ok(FieldKind::HostString(boxed_ty())),
        FieldAttrOverride::Nested => {
            return Ok(FieldKind::Nested {
                ty: boxed_ty(),
                is_abi_pod_candidate: !is_128bit_int(ty) && !is_platform_size_int(ty),
            });
        }
        FieldAttrOverride::HostVec(explicit_elem) => {
            if let Some(elem_ty) = explicit_elem {
                return Ok(FieldKind::HostVec {
                    field_ty: boxed_ty(),
                    elem_ty,
                });
            }
            if let Type::Path(type_path) = ty
                && let Some(last_seg) = type_path.path.segments.last()
                && let PathArguments::AngleBracketed(args) = &last_seg.arguments
                && let Some(GenericArgument::Type(elem_ty)) = args.args.first()
            {
                return Ok(FieldKind::HostVec {
                    field_ty: boxed_ty(),
                    elem_ty: Box::new(elem_ty.clone()),
                });
            }
            return Err(syn::Error::new_spanned(
                ty,
                "`#[mod_field(vec)]` requires a `Vec<T>` generic field type, or `#[mod_field(vec(ElemTy))]` for concrete type aliases",
            ));
        }
        FieldAttrOverride::None => {}
    }

    if is_bool_type(ty) {
        return Ok(FieldKind::Bool(boxed_ty()));
    }
    if is_platform_size_int(ty) {
        return Ok(FieldKind::Primitive {
            ty: boxed_ty(),
            is_abi_pod: false,
        });
    }
    if is_128bit_int(ty) {
        return Ok(FieldKind::Nested {
            ty: boxed_ty(),
            is_abi_pod_candidate: false,
        });
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
                        "f32" | "f64" | "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64"
                        | "FfiBool" | "FfiEntity" | "TargetEntity" | "BamComponent"
                        | "ComponentUuid",
                    ) => {
                        return Ok(FieldKind::Primitive {
                            ty: boxed_ty(),
                            is_abi_pod: true,
                        });
                    }
                    (PathArguments::None, "String") => {
                        return Ok(FieldKind::HostString(boxed_ty()));
                    }
                    (PathArguments::AngleBracketed(args), "Vec") => {
                        if args.args.len() == 1
                            && let Some(GenericArgument::Type(elem_ty)) = args.args.first()
                        {
                            return Ok(FieldKind::HostVec {
                                field_ty: boxed_ty(),
                                elem_ty: Box::new(elem_ty.clone()),
                            });
                        }
                        return Err(syn::Error::new_spanned(
                            ty,
                            "unsupported `Vec` type arguments for `ModComponent` (custom allocators are not supported across the C-ABI)",
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
                                "`{ident_str}` owns heap or Rust-specific layout state and cannot be exposed as a raw nested pointer; use `#[mod_field(skip)]` or a C-ABI safe field type"
                            ),
                        ));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    Ok(FieldKind::Nested {
        ty: boxed_ty(),
        is_abi_pod_candidate: true,
    })
}
