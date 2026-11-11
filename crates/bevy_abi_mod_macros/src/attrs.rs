use proc_macro_crate::FoundCrate;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{Attribute, DeriveInput, Ident, LitStr, Path};

/// Parsed container-level `#[mod_component(...)]` or `#[mod_exposable(...)]` attributes.
pub(crate) struct ComponentAttrs {
    /// Custom component name override.
    pub(crate) custom_name: Option<String>,
    /// Custom symbol prefix override.
    pub(crate) custom_prefix: Option<String>,
    /// Optional explicit path override to the `bevy_abi_mod_sys` contract module/crate.
    pub(crate) crate_override: Option<Path>,
    /// Whether to emit `#[unsafe(export_name = ...)]` on generated accessors.
    pub(crate) export_symbols: bool,
    /// Explicit opt-out of `AbiPod` auto-implementation on `#[repr(C)]` structs (`#[mod_component(no_pod)]`).
    pub(crate) no_pod: bool,
    /// Explicit opt-out of both `FfiLayout` and `AbiPod` (`#[mod_component(opaque)]`).
    pub(crate) opaque: bool,
}

/// Parsed `#[repr(...)]` layout flags on a struct.
#[derive(Default, Copy, Clone)]
pub(crate) struct ReprFlags {
    pub(crate) has_c: bool,
    pub(crate) has_transparent: bool,
    pub(crate) has_packed: bool,
}

impl ReprFlags {
    pub(crate) fn is_deterministic_unpacked(self) -> bool {
        (self.has_c || self.has_transparent) && !self.has_packed
    }
}

/// Inspects outer `#[repr(...)]` attributes on a struct.
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

/// Parses container-level `#[mod_component(...)]` / `#[mod_exposable(...)]` attributes for `ModExposable`.
pub(crate) fn parse_component_attrs(input: &DeriveInput) -> syn::Result<ComponentAttrs> {
    let mut attrs = ComponentAttrs {
        custom_name: None,
        custom_prefix: None,
        crate_override: None,
        export_symbols: false,
        no_pod: false,
        opaque: false,
    };

    for attr in &input.attrs {
        if !attr.path().is_ident("mod_component") && !attr.path().is_ident("mod_exposable") {
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
            } else {
                Err(meta.error(
                    "unsupported `#[mod_component(...)]` attribute; expected `name = \"...\"`, `prefix = \"...\"`, `crate = ...`, `export_symbols`, `no_pod`, or `opaque`",
                ))
            }
        })?;
    }

    Ok(attrs)
}

/// Parses optional `#[mod_component(crate = ...)]` / `#[mod_exposable(crate = ...)]` override for `AbiPod`.
pub(crate) fn parse_abi_pod_crate_override(input: &DeriveInput) -> syn::Result<Option<Path>> {
    let mut crate_override = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("mod_component") && !attr.path().is_ident("mod_exposable") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate") {
                crate_override = Some(parse_crate_path_value(&meta)?);
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported attribute for `#[derive(AbiPod)]`; only `crate = ...` is supported",
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

/// Field-level override specified via `#[mod_field(...)]`.
pub(crate) enum FieldAttrOverride {
    None,
    Skip,
    Primitive,
    HostString,
    HostVec,
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

/// Sanitizes a string into a valid C identifier prefix.
pub(crate) fn sanitize_symbol_prefix(name: &str) -> String {
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
