use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::{Expr, FnArg, ItemFn, Path, Token};

pub(crate) fn handler(contract: Expr, input: ItemFn) -> syn::Result<TokenStream> {
    let sig = &input.sig;
    if sig.unsafety.is_some()
        || sig.abi.is_some()
        || sig.constness.is_some()
        || sig.variadic.is_some()
        || !sig.generics.params.is_empty()
        || sig.generics.where_clause.is_some()
        || !(1..=2).contains(&sig.inputs.len())
        || sig
            .inputs
            .iter()
            .any(|arg| matches!(arg, FnArg::Receiver(_)))
    {
        return Err(syn::Error::new_spanned(sig,
            "handler requires fn(Input) -> Output or fn(RequestContext, Input) -> Output; async is supported, generics, methods and unsafe are not"));
    }
    let name = &sig.ident;
    let vis = &input.vis;
    let plain = name.unraw();
    let dispatch = format_ident!("__reknot_dispatch_{plain}");
    let key_fn = format_ident!("__reknot_key_{plain}");
    let descriptor = format_ident!("__reknot_descriptor_{plain}");
    let args = if sig.inputs.len() == 2 {
        quote!(context, input)
    } else {
        quote!(input)
    };
    let wait = sig.asyncness.map(|_| quote!(.await));
    // Conditional compilation must remove the generated peers too.
    let cfg: Vec<_> = input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"))
        .collect();
    Ok(quote! {
        #input
        #(#cfg)*
        #[doc(hidden)]
        #vis fn #key_fn() -> u64 { (#contract).key() }
        #(#cfg)*
        #[doc(hidden)]
        #vis fn #descriptor() -> ::reknot::typed::HandlerSpec {
            (#contract).descriptor().at_source(file!(), line!(), column!(), concat!(module_path!(), "::", stringify!(#name)))
        }
        #(#cfg)*
        #[doc(hidden)]
        #vis fn #dispatch(context: ::reknot::RequestContext)
            -> ::std::pin::Pin<Box<dyn ::std::future::Future<Output = Result<(), ::reknot::ServiceError>>>>
        {
            Box::pin(::reknot::typed::__private::invoke(
                #contract, context, |context, input| async move {
                    let _ = &context;
                    #name(#args)#wait
                }
            ))
        }
    })
}

pub(crate) struct Export {
    module: Expr,
    handlers: Vec<Path>,
    requires: Option<Expr>,
}
impl syn::parse::Parse for Export {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let module = input.parse()?;
        input.parse::<Token![,]>()?;
        let content;
        syn::bracketed!(content in input);
        let handlers = content
            .parse_terminated(Path::parse_mod_style, Token![,])?
            .into_iter()
            .collect::<Vec<_>>();
        if handlers.is_empty() {
            return Err(content.error("export_handlers needs at least one handler"));
        }
        let mut requires = None;
        if !input.is_empty() {
            input.parse::<Token![,]>()?;
            if !input.is_empty() {
                let option: syn::Ident = input.parse()?;
                if option != "requires" {
                    return Err(syn::Error::new_spanned(
                        option,
                        "expected requires = SERVICES",
                    ));
                }
                input.parse::<Token![=]>()?;
                requires = Some(input.parse()?);
                if !input.is_empty() {
                    input.parse::<Token![,]>()?;
                }
            }
        }
        Ok(Self {
            module,
            handlers,
            requires,
        })
    }
}
fn peer(path: &Path, prefix: &str) -> Path {
    let mut result = path.clone();
    let last = result.segments.last_mut().unwrap();
    last.ident = format_ident!("{prefix}{}", last.ident.unraw());
    result
}
pub(crate) fn export(options: Export) -> TokenStream {
    let module = options.module;
    let descriptors: Vec<_> = options
        .handlers
        .iter()
        .map(|p| peer(p, "__reknot_descriptor_"))
        .collect();
    let keys: Vec<_> = options
        .handlers
        .iter()
        .map(|p| peer(p, "__reknot_key_"))
        .collect();
    let dispatches: Vec<_> = options
        .handlers
        .iter()
        .map(|p| peer(p, "__reknot_dispatch_"))
        .collect();
    let requires = options.requires.map(|value| quote!(.with_services(#value)));
    quote! {
        #[no_mangle]
        pub extern "C" fn hot_handler_api() -> *const ::reknot::HandlerApi {
            static API: ::std::sync::OnceLock<::reknot::HandlerApi> = ::std::sync::OnceLock::new();
            ::std::panic::catch_unwind(|| API.get_or_init(handler_api) as *const ::reknot::HandlerApi)
                .unwrap_or(::std::ptr::null())
        }
        /// Handler table for a statically linked host. Export once per logic crate.
        pub fn handler_api() -> ::reknot::HandlerApi {
            unsafe extern "C" fn validate(bytes: *const u8, len: usize) -> u32 {
                ::reknot::typed::__private::validate(bytes, len, || {
                    (#module, vec![#(#descriptors()),*])
                })
            }
            unsafe extern "C" fn describe(bytes: *mut u8, capacity: usize) -> usize {
                ::reknot::typed::__private::describe(bytes, capacity, || {
                    (#module, vec![#(#descriptors()),*])
                })
            }
            unsafe extern "C" fn make(raw: *const ::reknot::HandlerRequest) -> ::reknot::TaskHandle {
                ::reknot::typed::__private::make(raw, |context| {
                    let key = ::reknot::typed::__private::request_key(&context)?;
                    #(if key == #keys() { return Ok(#dispatches(context)); })*
                    Err(::reknot::ServiceError::UNSUPPORTED_OPERATION)
                })
            }
            ::reknot::HandlerApi::new(
                ::reknot::typed::__private::SERVICE, make
            ).with_init(::reknot::typed::__private::SCHEMA, validate)
             .with_metadata(describe).with_state((#module).state_key())#requires
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsupported_signatures_with_actionable_message() {
        for source in [
            "fn f() {}",
            "unsafe fn f(x: String) -> String { x }",
            "fn f<T>(x: T) -> T { x }",
            "fn f(&self, x: String) -> String { x }",
        ] {
            let error =
                handler(syn::parse_quote!(KEY), syn::parse_str(source).unwrap()).unwrap_err();
            assert!(error.to_string().contains("fn(Input) -> Output"));
        }
    }
    #[test]
    fn accepts_sync_async_and_context_forms() {
        for source in [
            "fn f(x: String) -> String { x }",
            "async fn f(ctx: RequestContext, x: String) -> String { x }",
        ] {
            let tokens = handler(syn::parse_quote!(KEY), syn::parse_str(source).unwrap()).unwrap();
            syn::parse2::<syn::File>(tokens).unwrap();
        }
        let options =
            syn::parse_str::<Export>("module(), [a, nested::b], requires = SERVICES,").unwrap();
        syn::parse2::<syn::File>(export(options)).unwrap();
    }
}
