use crate::build_in_types::BuiltInType;
use crate::parse::HostLang;
use crate::{BridgedType, SWIFT_BRIDGE_PREFIX};
use proc_macro2::{Ident, TokenStream};
use quote::{quote, quote_spanned, ToTokens};
use std::ops::Deref;
use syn::spanned::Spanned;
use syn::{FnArg, ForeignItemFn, Lifetime, Pat, ReturnType, Token, Type};

mod to_extern_c_fn;
mod to_swift;

/// A method or associated function associated with a type.
///
/// fn bar (&self);
/// fn buzz (self: &Foo) -> u8;
///
/// #\[swift_bridge(init)\]
/// fn new () -> Foo;
///
/// ... etc
pub(crate) struct ParsedExternFn {
    pub func: ForeignItemFn,
    pub associated_type: Option<BridgedType>,
    pub is_initializer: bool,
    pub host_lang: HostLang,
}

impl ParsedExternFn {
    pub fn is_method(&self) -> bool {
        self.func.sig.receiver().is_some()
    }

    pub fn self_reference(&self) -> Option<(Token![&], Option<Lifetime>)> {
        match self.func.sig.receiver()? {
            FnArg::Receiver(receiver) => receiver.reference.clone(),
            FnArg::Typed(pat_ty) => match pat_ty.ty.deref() {
                Type::Reference(type_ref) => Some((type_ref.and_token, type_ref.lifetime.clone())),
                _ => None,
            },
        }
    }

    pub fn self_mutability(&self) -> Option<Token![mut]> {
        match self.func.sig.receiver()? {
            FnArg::Receiver(receiver) => receiver.mutability,
            FnArg::Typed(pat_ty) => match pat_ty.ty.deref() {
                Type::Reference(type_ref) => type_ref.mutability,
                _ => None,
            },
        }
    }

    pub fn returns_slice(&self) -> bool {
        match &self.func.sig.output {
            ReturnType::Default => false,
            ReturnType::Type(_, ty) => match BuiltInType::with_type(&ty) {
                Some(ty) => match ty {
                    BuiltInType::RefSlice(_) => true,
                    _ => false,
                },
                _ => false,
            },
        }
    }
}

impl ParsedExternFn {
    pub fn to_rust_param_names_and_types(&self) -> TokenStream {
        let host_type = self.associated_type.as_ref().map(|h| &h.ident);
        let mut params = vec![];
        let inputs = &self.func.sig.inputs;
        for arg in inputs {
            match arg {
                FnArg::Receiver(receiver) => {
                    let this = host_type.as_ref().unwrap();
                    let this = quote! { this: *mut super:: #this };
                    params.push(this);
                }
                FnArg::Typed(pat_ty) => {
                    if let Some(built_in) = BuiltInType::with_type(&pat_ty.ty) {
                        params.push(quote! {#pat_ty});
                    } else {
                        let arg_name = match pat_ty.pat.deref() {
                            Pat::Ident(this) if this.ident.to_string() == "self" => {
                                let this = Ident::new("this", this.span());
                                quote! {
                                    #this
                                }
                            }
                            _ => {
                                let arg_name = &pat_ty.pat;
                                quote! {
                                    #arg_name
                                }
                            }
                        };

                        let declared_ty = match pat_ty.ty.deref() {
                            Type::Reference(ty_ref) => {
                                let ty = &ty_ref.elem;
                                quote! {#ty}
                            }
                            Type::Path(path) => {
                                quote! {#path}
                            }
                            _ => todo!(),
                        };

                        params.push(quote! {
                             #arg_name: *mut super::#declared_ty
                        })
                    }
                }
            };
        }

        quote! {
            #(#params),*
        }
    }

    // fn foo (&self, arg1: u8, arg2: u32, &SomeType)
    //  becomes..
    // arg1, arg2, & unsafe { Box::from_raw(bar }
    pub fn to_rust_call_args(&self) -> TokenStream {
        let mut args = vec![];
        let inputs = &self.func.sig.inputs;
        for arg in inputs {
            match arg {
                FnArg::Receiver(_receiver) => {}
                FnArg::Typed(pat_ty) => {
                    match pat_ty.pat.deref() {
                        Pat::Ident(this) if this.ident.to_string() == "self" => {
                            continue;
                        }
                        _ => {}
                    };

                    let pat = &pat_ty.pat;

                    args.push(quote! {#pat});
                }
            };
        }

        quote! {
            #(#args),*
        }
    }

    // fn foo (&self, arg1: u8, arg2: u32)
    //  becomes..
    // void* self, uint8_t u8, uint32_t arg2
    pub fn to_c_header_params(&self) -> String {
        let mut params = vec![];
        let inputs = &self.func.sig.inputs;
        for arg in inputs {
            match arg {
                FnArg::Receiver(_receiver) => params.push("void* self".to_string()),
                FnArg::Typed(pat_ty) => {
                    let pat = &pat_ty.pat;

                    match pat.deref() {
                        Pat::Ident(pat_ident) if pat_ident.ident.to_string() == "self" => {
                            params.push("void* self".to_string());
                        }
                        _ => {
                            let ty = if let Some(built_in) = BuiltInType::with_type(&pat_ty.ty) {
                                built_in.to_c().to_string()
                            } else {
                                pat.to_token_stream().to_string()
                            };

                            let arg_name = pat_ty.pat.to_token_stream().to_string();
                            params.push(format!("{} {}", ty, arg_name));
                        }
                    };
                }
            };
        }

        if params.len() == 0 {
            "void".to_string()
        } else {
            params.join(", ")
        }
    }

    pub fn to_c_header_return(&self) -> String {
        match &self.func.sig.output {
            ReturnType::Default => "void".to_string(),
            ReturnType::Type(_, ty) => {
                if let Some(ty) = BuiltInType::with_type(&ty) {
                    ty.to_c()
                } else {
                    "void*".to_string()
                }
            }
        }
    }

    pub fn contains_ints(&self) -> bool {
        if let ReturnType::Type(_, ty) = &self.func.sig.output {
            if let Some(ty) = BuiltInType::with_type(&ty) {
                if ty.needs_include_int_header() {
                    return true;
                }
            }
        }

        for param in &self.func.sig.inputs {
            if let FnArg::Typed(pat_ty) = param {
                if let Some(ty) = BuiltInType::with_type(&pat_ty.ty) {
                    if ty.needs_include_int_header() {
                        return true;
                    }
                }
            }
        }

        false
    }
}

impl ParsedExternFn {
    pub fn link_name(&self) -> String {
        let host_type = self
            .associated_type
            .as_ref()
            .map(|h| format!("${}", h.ident.to_string()))
            .unwrap_or("".to_string());

        format!(
            "{}{}${}",
            SWIFT_BRIDGE_PREFIX,
            host_type,
            self.func.sig.ident.to_string()
        )
    }

    pub fn prefixed_fn_name(&self) -> Ident {
        let host_type_prefix = self
            .associated_type
            .as_ref()
            .map(|h| format!("{}_", h.ident.to_token_stream().to_string()))
            .unwrap_or_default();
        let fn_name = &self.func.sig.ident;
        let prefixed_fn_name = Ident::new(
            &format!(
                "{}{}{}",
                SWIFT_BRIDGE_PREFIX,
                host_type_prefix,
                fn_name.to_string()
            ),
            fn_name.span(),
        );

        prefixed_fn_name
    }
}

impl Deref for ParsedExternFn {
    type Target = ForeignItemFn;

    fn deref(&self) -> &Self::Target {
        &self.func
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{ParseError, ParseErrors};
    use crate::parse::SwiftBridgeModuleAndErrors;
    use crate::test_utils::{assert_tokens_contain, assert_tokens_eq};
    use crate::SwiftBridgeModule;

    /// Verify that we rename `self` parameters to `this`
    #[test]
    fn renames_self_to_this_in_params() {
        let tokens = quote! {
            #[swift_bridge::bridge]
            mod ffi {
                extern "Rust" {
                    type Foo;
                    fn make1 (self);
                    fn make2 (&self);
                    fn make3 (&mut self);
                    fn make4 (self: Foo);
                    fn make5 (self: &Foo);
                    fn make6 (self: &mut Foo);
                }
            }
        };
        let module = parse_ok(tokens);
        let methods = &module.functions;
        assert_eq!(methods.len(), 6);

        for method in methods {
            assert_tokens_contain(&method.to_rust_param_names_and_types(), &quote! { this });
        }
    }

    /// Verify that when generating rust call args we do not include the receiver.
    #[test]
    fn does_not_include_self_in_rust_call_args() {
        let tokens = quote! {
            #[swift_bridge::bridge]
            mod ffi {
                extern "Rust" {
                    type Foo;
                    fn make1 (self);
                    fn make2 (&self);
                    fn make3 (&mut self);
                    fn make4 (self: Foo);
                    fn make5 (self: &Foo);
                    fn make6 (self: &mut Foo);
                }
            }
        };
        let module = parse_ok(tokens);
        let methods = &module.functions;
        assert_eq!(methods.len(), 6);

        for method in methods {
            let rust_call_args = &method.to_rust_call_args();
            assert_eq!(
                rust_call_args.to_string(),
                "",
                "\n Function Tokens:\n{:#?}",
                method.func.to_token_stream()
            );
        }
    }

    /// Verify that arguments that are owned declared types get unboxed.
    #[test]
    fn does_not_allow_owned_foreign_type_args() {
        let tokens = quote! {
            #[swift_bridge::bridge]
            mod ffi {
                extern "Rust" {
                    type Foo;

                    fn freestanding (arg: Foo);

                    #[swift_bridge(associated_to = Foo)]
                    fn associated_func (arg: Foo);

                    fn method (&self, arg: Foo);

                    fn owned_method(self);

                    fn owned_method_explicit(self: Foo);
                }
            }
        };
        let errors = parse_errors(tokens);
        assert_eq!(errors.len(), 5);

        for err in errors.iter() {
            match err {
                ParseError::OwnedForeignTypeArgNotAllowed { ty } => {
                    assert_eq!(ty.to_token_stream().to_string(), "Foo");
                }
                _ => panic!(),
            };
        }
    }

    /// Verify that if a foreign type is marked as enabled we allow taking owned foreign type args.
    #[test]
    fn allow_foreign_type_arg_if_type_marked_enabled_or_enabled_unchecked() {
        let tokens = quote! {
            #[swift_bridge::bridge]
            mod ffi {
                extern "Rust" {
                    #[swift_bridge(owned_arg = "enabled")]
                    type Foo;
                    #[swift_bridge(owned_arg = "enabled_unchecked")]
                    type Bar;

                    fn a (arg: Foo);
                    fn b (arg: Bar);
                }
            }
        };
        let module = parse_ok(tokens);
        assert_eq!(module.functions.len(), 2);
    }

    fn parse_ok(tokens: TokenStream) -> SwiftBridgeModule {
        let module_and_errors: SwiftBridgeModuleAndErrors = syn::parse2(tokens).unwrap();
        module_and_errors.module
    }

    fn parse_errors(tokens: TokenStream) -> ParseErrors {
        let parsed: SwiftBridgeModuleAndErrors = syn::parse2(tokens).unwrap();
        parsed.errors
    }
}
