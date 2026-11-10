//! Attribute macro for the message handler ABI.
use proc_macro::TokenStream;
use syn::{parse_macro_input, ItemFn};

mod handler;
mod typed;

#[proc_macro_attribute]
pub fn reload_handler(attr: TokenStream, item: TokenStream) -> TokenStream {
    let options = parse_macro_input!(attr as handler::Options);
    let input = parse_macro_input!(item as ItemFn);
    match handler::expand(input, options) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Marks a typed function for inclusion in `export_handlers!`.
#[proc_macro_attribute]
pub fn handler(attr: TokenStream, item: TokenStream) -> TokenStream {
    let contract = parse_macro_input!(attr as syn::Expr);
    let input = parse_macro_input!(item as ItemFn);
    typed::handler(contract, input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Exports one module containing the listed typed handlers.
#[proc_macro]
pub fn export_handlers(input: TokenStream) -> TokenStream {
    typed::export(parse_macro_input!(input as typed::Export)).into()
}

/// Derives the typed contract's JSON Schema, respecting supported Serde attributes.
/// Apply alongside Serialize/Deserialize to a struct or enum. Custom Serde codecs
/// need an accurate `#[schemars(with = "...")]` or manual JsonSchema implementation.
#[proc_macro_attribute]
pub fn schema(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "schema takes no arguments; use #[schemars(...)] for schema options",
        )
        .to_compile_error()
        .into();
    }
    let input = parse_macro_input!(item as syn::DeriveInput);
    if matches!(input.data, syn::Data::Union(_)) {
        return syn::Error::new_spanned(input, "schema supports structs and enums")
            .to_compile_error()
            .into();
    }
    quote::quote! {
        #[derive(::reknot::typed::JsonSchema)]
        #[schemars(crate = "::reknot::typed::schemars")]
        #input
    }
    .into()
}
