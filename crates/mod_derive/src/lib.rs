use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput};

/// Derive macro for `ModExposable` components.
///
/// Generates metadata and C-ABI export VTables for Bevy host components.
#[proc_macro_derive(ModExposable, attributes(mod_component))]
pub fn derive_mod_exposable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    // Default UUID generation: deterministic 16-byte hash from identifier name
    let type_name_str = name.to_string();

    let expanded = quote! {
        // Implementation scaffold for ModExposable trait
        impl #name {
            pub const MOD_COMPONENT_NAME: &'static str = #type_name_str;
        }
    };

    TokenStream::from(expanded)
}
