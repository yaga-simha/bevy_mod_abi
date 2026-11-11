use crate::attrs::FieldAttrOverride;
use syn::{GenericArgument, PathArguments, Type};

/// Classification of a struct field for C-ABI accessor code generation.
pub(crate) enum FieldKind {
    /// Skip generating C-ABI accessors for this field (`#[mod_field(skip)]`).
    Skip,
    /// Rust `bool` field exposed across `extern "C"` as `u8` (`0` = false, non-zero = true) to prevent invalid-bool UB.
    Bool,
    /// Bit-valid `AbiPod` primitive (`f32`, `f64`, `i8`..`i64`, `u8`..`u64`, `usize`, `isize`, `FfiEntity`).
    Primitive(Type),
    /// Heap-allocated `String` owned by the host allocator.
    HostString,
    /// Heap-allocated `Vec<T>` owned by the host allocator (`T: AbiPod`).
    HostVec(Type),
    /// Nested compound struct or fixed array (`T: FfiLayout`) exposed via interior pointer accessors.
    Nested(Type),
}

/// Returns `true` if `ty` spells Rust's primitive `bool` (`bool`, `core::primitive::bool`, `std::primitive::bool`).
pub(crate) fn is_bool_type(ty: &Type) -> bool {
    if let Type::Path(tp) = ty
        && tp.qself.is_none()
    {
        if tp.path.is_ident("bool") {
            return true;
        }
        let segs = &tp.path.segments;
        if segs.len() == 3
            && (segs[0].ident == "core" || segs[0].ident == "std")
            && segs[1].ident == "primitive"
            && segs[2].ident == "bool"
            && segs[2].arguments.is_empty()
        {
            return true;
        }
    }
    false
}

/// Classifies a field type into bool, primitive, string, vector, or nested representation.
pub(crate) fn classify_type(ty: &Type, attr_override: FieldAttrOverride) -> syn::Result<FieldKind> {
    match attr_override {
        FieldAttrOverride::Skip => return Ok(FieldKind::Skip),
        FieldAttrOverride::Primitive => {
            if is_bool_type(ty) {
                return Ok(FieldKind::Bool);
            }
            return Ok(FieldKind::Primitive(ty.clone()));
        }
        FieldAttrOverride::HostString => return Ok(FieldKind::HostString),
        FieldAttrOverride::Nested => return Ok(FieldKind::Nested(ty.clone())),
        FieldAttrOverride::HostVec => {
            if let Type::Path(type_path) = ty
                && let Some(last_seg) = type_path.path.segments.last()
                && let PathArguments::AngleBracketed(args) = &last_seg.arguments
                && let Some(GenericArgument::Type(elem_ty)) = args.args.first()
            {
                return Ok(FieldKind::HostVec(elem_ty.clone()));
            }
            return Err(syn::Error::new_spanned(
                ty,
                "`#[mod_field(vec)]` requires a `Vec<T>` field type",
            ));
        }
        FieldAttrOverride::None => {}
    }

    if is_bool_type(ty) {
        return Ok(FieldKind::Bool);
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
                        | "u32" | "u64" | "usize" | "FfiEntity",
                    ) => {
                        return Ok(FieldKind::Primitive(ty.clone()));
                    }
                    (PathArguments::None, "String") => {
                        return Ok(FieldKind::HostString);
                    }
                    (PathArguments::AngleBracketed(args), "Vec") => {
                        if args.args.len() == 1
                            && let Some(GenericArgument::Type(elem_ty)) = args.args.first()
                        {
                            return Ok(FieldKind::HostVec(elem_ty.clone()));
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
