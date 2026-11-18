use proc_macro_crate::FoundCrate;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{Attribute, DeriveInput, Ident, LitStr, Path, Type, parse::Parse};

/// Parsed container-level `#[mod_component(...)]` attributes.
pub(crate) struct ComponentAttrs {
    /// Optional component name override (`#[mod_component(name = "...")]`).
    pub(crate) custom_name: Option<String>,
    /// Optional component namespace override (`#[mod_component(namespace = "...")]`).
    pub(crate) custom_namespace: Option<String>,
    /// Optional explicit 128-bit UUID hex string (`#[mod_component(uuid = "...")]`).
    pub(crate) custom_uuid: Option<String>,
    /// Optional exported C symbol prefix override (`#[mod_component(prefix = "...")]`).
    pub(crate) custom_prefix: Option<String>,
    /// Optional explicit path override to the `bevy_abi_mod_sys` contract module/crate.
    pub(crate) crate_override: Option<Path>,
    /// Whether to emit `#[unsafe(export_name = "bam_<ns>_<Struct>_...")]` on generated accessors.
    pub(crate) export_symbols: bool,
    /// Explicit opt-out of `AbiPod` auto-implementation on `#[repr(C)]` structs (`#[mod_component(no_pod)]`).
    pub(crate) no_pod: bool,
    /// Explicit opt-out of both `RawInsertable` and `AbiPod` (`#[mod_component(opaque)]`).
    pub(crate) opaque: bool,
    /// Sets `BAM_COMPONENT_FLAG_SPARSE_SET` on `ModComponent::COMPONENT_FLAGS`.
    pub(crate) sparse_set: bool,
    /// Sets `BAM_COMPONENT_FLAG_IMMUTABLE` on `ModComponent::COMPONENT_FLAGS`.
    pub(crate) immutable: bool,
    /// Sets `BAM_COMPONENT_FLAG_SUMMARY_TICK` on `ModComponent::COMPONENT_FLAGS`.
    pub(crate) summary_tick: bool,
    /// Required component types declared via `#[mod_component(require(CompA, CompB))]`.
    pub(crate) required_components: Vec<Path>,
}

/// Explicit fixed-width integer representation parsed from `#[repr(...)]`.
#[derive(Default, Copy, Clone, PartialEq, Eq)]
pub(crate) enum IntRepr {
    #[default]
    None,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
}

impl IntRepr {
    pub(crate) fn is_some(self) -> bool {
        !matches!(self, Self::None)
    }

    pub(crate) fn rust_ty_str(self) -> &'static str {
        match self {
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::None | Self::I32 => "i32",
            Self::I64 => "i64",
        }
    }

    pub(crate) fn c_ty_str(self) -> &'static str {
        match self {
            Self::None | Self::I32 => "int32_t",
            Self::U8 => "uint8_t",
            Self::U16 => "uint16_t",
            Self::U32 => "uint32_t",
            Self::U64 => "uint64_t",
            Self::I8 => "int8_t",
            Self::I16 => "int16_t",
            Self::I64 => "int64_t",
        }
    }

    pub(crate) fn zig_ty_str(self) -> &'static str {
        self.rust_ty_str()
    }

    pub(crate) fn fits_i128(self, value: i128) -> bool {
        match self {
            Self::None | Self::I32 => value >= i32::MIN as i128 && value <= i32::MAX as i128,
            Self::U8 => value >= u8::MIN as i128 && value <= u8::MAX as i128,
            Self::U16 => value >= u16::MIN as i128 && value <= u16::MAX as i128,
            Self::U32 => value >= u32::MIN as i128 && value <= u32::MAX as i128,
            Self::U64 => value >= u64::MIN as i128 && value <= u64::MAX as i128,
            Self::I8 => value >= i8::MIN as i128 && value <= i8::MAX as i128,
            Self::I16 => value >= i16::MIN as i128 && value <= i16::MAX as i128,
            Self::I64 => value >= i64::MIN as i128 && value <= i64::MAX as i128,
        }
    }
}

/// Parsed `#[repr(...)]` layout flags on a struct or enum.
#[derive(Default, Clone)]
pub(crate) struct ReprFlags {
    pub(crate) has_c: bool,
    pub(crate) has_transparent: bool,
    pub(crate) has_packed: bool,
    pub(crate) int_repr: IntRepr,
    pub(crate) unsupported_int_repr: Option<String>,
}

impl ReprFlags {
    pub(crate) fn is_deterministic_unpacked(&self) -> bool {
        (self.has_c || self.has_transparent) && !self.has_packed
    }
}

/// Inspects outer `#[repr(...)]` attributes on a struct or enum.
pub(crate) fn parse_repr_flags(attrs: &[Attribute]) -> syn::Result<ReprFlags> {
    let mut flags = ReprFlags::default();
    for attr in attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("C") {
                flags.has_c = true;
                Ok(())
            } else if meta.path.is_ident("transparent") {
                flags.has_transparent = true;
                Ok(())
            } else if meta.path.is_ident("u8") {
                flags.int_repr = IntRepr::U8;
                Ok(())
            } else if meta.path.is_ident("u16") {
                flags.int_repr = IntRepr::U16;
                Ok(())
            } else if meta.path.is_ident("u32") {
                flags.int_repr = IntRepr::U32;
                Ok(())
            } else if meta.path.is_ident("u64") {
                flags.int_repr = IntRepr::U64;
                Ok(())
            } else if meta.path.is_ident("i8") {
                flags.int_repr = IntRepr::I8;
                Ok(())
            } else if meta.path.is_ident("i16") {
                flags.int_repr = IntRepr::I16;
                Ok(())
            } else if meta.path.is_ident("i32") {
                flags.int_repr = IntRepr::I32;
                Ok(())
            } else if meta.path.is_ident("i64") {
                flags.int_repr = IntRepr::I64;
                Ok(())
            } else if meta.path.is_ident("usize")
                || meta.path.is_ident("isize")
                || meta.path.is_ident("u128")
                || meta.path.is_ident("i128")
            {
                flags.unsupported_int_repr = meta.path.get_ident().map(ToString::to_string);
                Ok(())
            } else if meta.path.is_ident("packed") {
                flags.has_packed = true;
                if meta.input.peek(syn::token::Paren) {
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let _: TokenStream = content.parse()?;
                }
                Ok(())
            } else if meta.path.is_ident("align") {
                let content;
                syn::parenthesized!(content in meta.input);
                let _: TokenStream = content.parse()?;
                Ok(())
            } else if meta.path.is_ident("Rust") {
                Ok(())
            } else {
                if meta.input.peek(syn::token::Paren) {
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let _: TokenStream = content.parse()?;
                }
                Ok(())
            }
        })?;
    }
    Ok(flags)
}

fn parse_crate_path_value(meta: &syn::meta::ParseNestedMeta<'_>) -> syn::Result<Path> {
    let value = meta.value()?;
    if value.peek(LitStr) {
        let s: LitStr = value.parse()?;
        s.parse()
    } else {
        value.parse()
    }
}

/// Validates a UUID hex string against the exact 36-char hyphenated `"8-4-4-4-12"` or 32-char
/// contiguous format accepted by `bevy_abi_mod_sys::ComponentUuid::parse_hex`.
pub(crate) fn is_valid_uuid_hex(hex: &str) -> bool {
    let src = hex.as_bytes();
    let hyphenated = match src.len() {
        36 if src[8] == b'-' && src[13] == b'-' && src[18] == b'-' && src[23] == b'-' => true,
        32 => false,
        _ => return false,
    };

    let mut src_idx = 0;
    let mut byte_idx = 0;
    while byte_idx < 16 {
        if hyphenated && (src_idx == 8 || src_idx == 13 || src_idx == 18 || src_idx == 23) {
            src_idx += 1;
        }
        if !src[src_idx].is_ascii_hexdigit() || !src[src_idx + 1].is_ascii_hexdigit() {
            return false;
        }
        src_idx += 2;
        byte_idx += 1;
    }
    true
}

fn validate_non_empty_no_nul(lit: &LitStr, label: &str) -> syn::Result<String> {
    let val = lit.value();
    if val.is_empty() {
        return Err(syn::Error::new_spanned(
            lit,
            format!("{label} must not be empty"),
        ));
    }
    if val.as_bytes().contains(&0) {
        return Err(syn::Error::new_spanned(
            lit,
            format!("{label} must not contain interior NUL bytes"),
        ));
    }
    Ok(val)
}

/// Parses container-level `#[mod_component(...)]` attributes for `ModComponent`.
pub(crate) fn parse_component_attrs(input: &DeriveInput) -> syn::Result<ComponentAttrs> {
    let mut attrs = ComponentAttrs {
        custom_name: None,
        custom_namespace: None,
        custom_uuid: None,
        custom_prefix: None,
        crate_override: None,
        export_symbols: false,
        no_pod: false,
        opaque: false,
        sparse_set: false,
        immutable: false,
        summary_tick: false,
        required_components: Vec::new(),
    };

    for attr in &input.attrs {
        if !attr.path().is_ident("mod_component") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                let val = validate_non_empty_no_nul(&lit, "component name")?;
                if val.starts_with("::") || val.ends_with("::") || val.contains("::::") {
                    return Err(syn::Error::new_spanned(
                        &lit,
                        "component name has malformed `::` namespace separator",
                    ));
                }
                attrs.custom_name = Some(val);
                Ok(())
            } else if meta.path.is_ident("namespace") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                let val = validate_non_empty_no_nul(&lit, "component namespace")?;
                if val.starts_with("::") || val.ends_with("::") || val.contains("::::") {
                    return Err(syn::Error::new_spanned(
                        &lit,
                        "component namespace has malformed `::` separator",
                    ));
                }
                attrs.custom_namespace = Some(val);
                Ok(())
            } else if meta.path.is_ident("uuid") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                let val = lit.value();
                if !is_valid_uuid_hex(&val) {
                    return Err(syn::Error::new_spanned(
                        &lit,
                        "invalid `#[mod_component(uuid = \"...\")]` hex string; expected 36-char `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` or 32 hex digits",
                    ));
                }
                attrs.custom_uuid = Some(val);
                Ok(())
            } else if meta.path.is_ident("prefix") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                attrs.custom_prefix = Some(validate_non_empty_no_nul(&lit, "symbol prefix")?);
                Ok(())
            } else if meta.path.is_ident("crate") {
                attrs.crate_override = Some(parse_crate_path_value(&meta)?);
                Ok(())
            } else if meta.path.is_ident("export_symbols") {
                attrs.export_symbols = true;
                Ok(())
            } else if meta.path.is_ident("no_pod") {
                attrs.no_pod = true;
                Ok(())
            } else if meta.path.is_ident("opaque") {
                attrs.no_pod = true;
                attrs.opaque = true;
                Ok(())
            } else if meta.path.is_ident("sparse_set") {
                attrs.sparse_set = true;
                Ok(())
            } else if meta.path.is_ident("immutable") {
                attrs.immutable = true;
                Ok(())
            } else if meta.path.is_ident("summary_tick") {
                attrs.summary_tick = true;
                Ok(())
            } else if meta.path.is_ident("require") {
                if !meta.input.peek(syn::token::Paren) {
                    return Err(meta.error(
                        "`#[mod_component(require(...))]` requires a parenthesized list of component types",
                    ));
                }
                let content;
                syn::parenthesized!(content in meta.input);
                let paths = content.parse_terminated(Path::parse, syn::Token![,])?;
                if paths.is_empty() {
                    return Err(meta.error(
                        "`#[mod_component(require(...))]` requires at least one component type",
                    ));
                }
                attrs.required_components.extend(paths);
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[mod_component(...)]` attribute; expected `name = \"...\"`, `namespace = \"...\"`, `uuid = \"...\"`, `prefix = \"...\"`, `crate = ...`, `export_symbols`, `no_pod`, `opaque`, `sparse_set`, `immutable`, `summary_tick`, or `require(...)`",
                ))
            }
        })?;
    }

    if attrs.sparse_set && attrs.summary_tick {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "cannot combine `#[mod_component(sparse_set)]` with `#[mod_component(summary_tick)]` (`summary_tick` requires `Table` storage)",
        ));
    }

    if attrs.custom_namespace.is_some()
        && let Some(ref name) = attrs.custom_name
        && name.contains("::")
    {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "cannot combine `#[mod_component(namespace = \"...\")]` with an already-namespaced `name = \"ns::Name\"`",
        ));
    }

    Ok(attrs)
}

/// Parses optional `#[mod_component(crate = ...)]` override for `#[derive(AbiPod)]`.
pub(crate) fn parse_abi_pod_crate_override(input: &DeriveInput) -> syn::Result<Option<Path>> {
    let mut crate_override = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("mod_component") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate") {
                crate_override = Some(parse_crate_path_value(&meta)?);
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[mod_component(...)]` attribute for `#[derive(AbiPod)]`; only `crate = ...` is supported",
                ))
            }
        })?;
    }
    Ok(crate_override)
}

/// Resolves the token path to `bevy_abi_mod_sys`, respecting an explicit `crate = ...` override
/// or probing Cargo dependencies in order:
/// 1. `bevy_abi_mod_sys`
/// 2. `bevy_abi_mod` (`<crate>::bevy_abi_mod_sys`)
/// 3. `bevy_abi_mod_host` (`<crate>::bevy_abi_mod_sys`)
/// 4. `bevy_abi_mod_guest` (`<crate>::bevy_abi_mod_sys`)
pub(crate) fn resolve_sys_crate_path(crate_override: Option<&Path>) -> TokenStream {
    let current_crate = std::env::var("CARGO_CRATE_NAME").ok();
    resolve_sys_crate_with(crate_override, current_crate.as_deref(), |pkg| {
        proc_macro_crate::crate_name(pkg).ok()
    })
}

pub(crate) fn resolve_sys_crate_with<F>(
    crate_override: Option<&Path>,
    current_crate_name: Option<&str>,
    mut lookup: F,
) -> TokenStream
where
    F: FnMut(&str) -> Option<FoundCrate>,
{
    if let Some(path) = crate_override {
        return quote! { #path };
    }

    if let Some(found) = lookup("bevy_abi_mod_sys") {
        return match found {
            FoundCrate::Itself if current_crate_name == Some("bevy_abi_mod_sys") => {
                quote! { crate }
            }
            FoundCrate::Itself => quote! { ::bevy_abi_mod_sys },
            FoundCrate::Name(name) => {
                let ident = Ident::new(&name, Span::call_site());
                quote! { ::#ident }
            }
        };
    }

    for facade in ["bevy_abi_mod", "bevy_abi_mod_host", "bevy_abi_mod_guest"] {
        if let Some(found) = lookup(facade) {
            return match found {
                FoundCrate::Itself if current_crate_name == Some(facade) => {
                    quote! { crate::bevy_abi_mod_sys }
                }
                FoundCrate::Itself => {
                    let ident = Ident::new(facade, Span::call_site());
                    quote! { ::#ident::bevy_abi_mod_sys }
                }
                FoundCrate::Name(name) => {
                    let ident = Ident::new(&name, Span::call_site());
                    quote! { ::#ident::bevy_abi_mod_sys }
                }
            };
        }
    }

    quote! { ::bevy_abi_mod_sys }
}

/// Field-level classification override specified via `#[mod_field(...)]`.
pub(crate) enum FieldAttrOverride {
    None,
    Skip,
    Bool,
    Primitive,
    HostString,
    HostVec(Option<Box<Type>>),
    FfiOption(Option<Box<Type>>),
    Nested,
}

/// Parses field-level `#[mod_field(...)]` attributes.
pub(crate) fn parse_field_attr(attrs: &[Attribute]) -> syn::Result<FieldAttrOverride> {
    let mut result = FieldAttrOverride::None;
    for attr in attrs {
        if !attr.path().is_ident("mod_field") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                result = FieldAttrOverride::Skip;
                Ok(())
            } else if meta.path.is_ident("bool") {
                result = FieldAttrOverride::Bool;
                Ok(())
            } else if meta.path.is_ident("primitive") {
                result = FieldAttrOverride::Primitive;
                Ok(())
            } else if meta.path.is_ident("string") {
                result = FieldAttrOverride::HostString;
                Ok(())
            } else if meta.path.is_ident("vec") {
                if meta.input.peek(syn::token::Paren) {
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let elem_ty: Type = content.parse()?;
                    result = FieldAttrOverride::HostVec(Some(Box::new(elem_ty)));
                } else {
                    result = FieldAttrOverride::HostVec(None);
                }
                Ok(())
            } else if meta.path.is_ident("option") {
                if meta.input.peek(syn::token::Paren) {
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let inner_ty: Type = content.parse()?;
                    result = FieldAttrOverride::FfiOption(Some(Box::new(inner_ty)));
                } else {
                    result = FieldAttrOverride::FfiOption(None);
                }
                Ok(())
            } else if meta.path.is_ident("nested") {
                result = FieldAttrOverride::Nested;
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported `#[mod_field(...)]` attribute; expected `skip`, `bool`, `primitive`, `string`, `vec`, `vec(ElemTy)`, `option`, `option(InnerTy)`, or `nested`",
                ))
            }
        })?;
    }
    Ok(result)
}

/// Converts a PascalCase/CamelCase Rust identifier into `snake_case`.
pub(crate) fn to_snake_case(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 4);
    let chars: Vec<char> = raw.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                let prev = chars[i - 1];
                let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
                if prev.is_ascii_lowercase() || (prev.is_ascii_uppercase() && next_is_lower) {
                    out.push('_');
                }
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Sanitizes a namespace or identifier string into a valid C symbol segment,
/// collapsing `"::"` into a single `'_'` and replacing non-ASCII-alphanumeric characters with `'_'`.
pub(crate) fn sanitize_symbol_segment(raw: &str) -> String {
    let collapsed = raw.replace("::", "_");
    let mut out = String::with_capacity(collapsed.len());
    for ch in collapsed.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}
