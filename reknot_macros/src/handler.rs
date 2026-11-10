use proc_macro2::TokenStream;
use quote::quote;
use syn::{Expr, FnArg, ItemFn, Type};

pub(crate) struct Options {
    service: Expr,
    state: Option<Expr>,
    requires: Option<Expr>,
    init: Option<Type>,
}
impl syn::parse::Parse for Options {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let service = input.parse()?;
        let mut state = None;
        let mut requires = None;
        let mut init = None;
        while !input.is_empty() {
            input.parse::<syn::Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let name: syn::Ident = input.parse()?;
            input.parse::<syn::Token![=]>()?;
            match name.to_string().as_str() {
                "state" if state.is_none() => state = Some(input.parse()?),
                "requires" if requires.is_none() => requires = Some(input.parse()?),
                "init" if init.is_none() => init = Some(input.parse()?),
                _ => {
                    return Err(syn::Error::new_spanned(
                        name,
                        "expected unique state, requires or init option",
                    ))
                }
            }
        }
        Ok(Self {
            service,
            state,
            requires,
            init,
        })
    }
}

pub(crate) fn expand(mut input: ItemFn, options: Options) -> syn::Result<TokenStream> {
    let service = options.service;
    let state = options.state.map(|schema| quote!(.with_state(#schema)));
    let requires = options
        .requires
        .map(|requirements| quote!(.with_services(#requirements)));
    let init = options.init.map(|ty| {
        quote!(.with_init(
            <#ty as ::reknot::InitPayload>::SCHEMA,
            ::reknot::internal::validate_init_data::<#ty>
        ))
    });
    let valid_arg = match input.sig.inputs.first() {
        Some(FnArg::Typed(arg)) => matches!(arg.ty.as_ref(), Type::Path(path)
            if path.path.segments.last().is_some_and(|segment| segment.ident == "RequestContext")),
        _ => false,
    };
    if input.sig.asyncness.is_none()
        || input.sig.unsafety.is_some()
        || input.sig.inputs.len() != 1
        || !valid_arg
        || !input.sig.generics.params.is_empty()
        || input.sig.generics.where_clause.is_some()
        || input.sig.variadic.is_some()
    {
        return Err(syn::Error::new_spanned(&input.sig,
            "reload_handler requires async fn(RequestContext) -> Result<(), ServiceError> without generics or unsafe"));
    }
    let inner = syn::Ident::new(
        &format!("_inner_{}", input.sig.ident),
        input.sig.ident.span(),
    );
    input.sig.ident = inner.clone();
    input.vis = syn::Visibility::Inherited;
    Ok(quote! {
        #input

        unsafe extern "C" fn make_handler_request(
            raw: *const ::reknot::HandlerRequest,
        ) -> ::reknot::TaskHandle {
            match ::reknot::RequestContext::from_raw(raw) {
                Ok(context) => ::reknot::internal::make_handler_future(
                    Box::pin(async move { #inner(context).await })
                ),
                Err(_) => ::reknot::TaskHandle {
                    data: std::ptr::null_mut(), vtable: std::ptr::null(),
                },
            }
        }

        static HANDLER_API: ::reknot::HandlerApi =
            ::reknot::HandlerApi::new(#service, make_handler_request)#state #requires #init;

        /// Returns the handler table for a statically linked host.
        pub fn handler_api() -> ::reknot::HandlerApi {
            HANDLER_API
        }

        #[no_mangle]
        pub extern "C" fn hot_handler_api() -> *const ::reknot::HandlerApi {
            &HANDLER_API
        }
    })
}
