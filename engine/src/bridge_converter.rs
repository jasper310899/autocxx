// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::additional_cpp_generator::{AdditionalNeed, ArgumentConversion};
use crate::byvalue_checker::ByValueChecker;
use crate::types::TypeName;
use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use std::collections::HashMap;
use syn::punctuated::Punctuated;
use syn::{
    parse_quote, Attribute, FnArg, ForeignItem, ForeignItemFn, GenericArgument, Ident, Item,
    ItemForeignMod, ItemMod, Path, PathArguments, PathSegment, ReturnType, Type, TypePath, TypePtr,
    TypeReference,
};

#[derive(Debug)]
pub enum ConvertError {
    NoContent,
    UnsafePODType(String),
    UnknownForeignItem,
}

/// Results of a conversion.
pub(crate) struct BridgeConversionResults {
    pub items: Vec<Item>,
    pub additional_cpp_needs: Vec<AdditionalNeed>,
}

/// Converts the bindings generated by bindgen into a form suitable
/// for use with `cxx`.
/// Tasks current performed:
/// * Replaces certain identifiers e.g. `std_unique_ptr` with `UniquePtr`
/// * Replaces pointers with references
/// * Removes repr attributes
/// * Removes link_name attributes
/// * Adds include! directives
/// * Adds #[cxx::bridge]
/// At the moment this is almost certainly not using the best practice for parsing
/// stuff. It's multiple simple-but-yucky state machines. Can undoubtedly be
/// simplified and made less error-prone: TODO. Probably the right thing to do
/// is just manipulate the syn types directly.
pub(crate) struct BridgeConverter {
    include_list: Vec<String>,
    pod_requests: Vec<TypeName>,
}

impl BridgeConverter {
    pub fn new(include_list: Vec<String>, pod_requests: Vec<TypeName>) -> Self {
        Self {
            include_list,
            pod_requests,
        }
    }

    /// Convert a TokenStream of bindgen-generated bindings to a form
    /// suitable for cxx.
    pub(crate) fn convert(
        &mut self,
        bindings: ItemMod,
        extra_inclusion: Option<&str>,
        renames: &HashMap<String, String>,
    ) -> Result<BridgeConversionResults, ConvertError> {
        match bindings.content {
            None => Err(ConvertError::NoContent),
            Some((brace, items)) => {
                let bindgen_mod = ItemMod {
                    attrs: bindings.attrs,
                    vis: bindings.vis,
                    ident: bindings.ident,
                    mod_token: bindings.mod_token,
                    content: Some((brace, Vec::new())),
                    semi: bindings.semi,
                };
                let conversion = BridgeConversion {
                    bindgen_mod,
                    all_items: Vec::new(),
                    bridge_items: Vec::new(),
                    extern_c_mod: None,
                    extern_c_mod_items: Vec::new(),
                    additional_cpp_needs: Vec::new(),
                    types_found: Vec::new(),
                    bindgen_items: Vec::new(),
                    byvalue_checker: ByValueChecker::new(),
                    pod_requests: &self.pod_requests,
                    include_list: &self.include_list,
                    renames,
                };
                conversion.convert_items(items, extra_inclusion)
            }
        }
    }
}

struct BridgeConversion<'a> {
    bindgen_mod: ItemMod,
    all_items: Vec<Item>,
    bridge_items: Vec<Item>,
    extern_c_mod: Option<ItemForeignMod>,
    extern_c_mod_items: Vec<ForeignItem>,
    additional_cpp_needs: Vec<AdditionalNeed>,
    types_found: Vec<TypeName>,
    bindgen_items: Vec<Item>,
    byvalue_checker: ByValueChecker,
    pod_requests: &'a Vec<TypeName>,
    include_list: &'a Vec<String>,
    renames: &'a HashMap<String, String>,
}

impl<'a> BridgeConversion<'a> {
    fn find_nested_pod_types(&mut self, items: &[Item]) -> Result<(), ConvertError> {
        for item in items {
            match item {
                Item::Struct(s) => self.byvalue_checker.ingest_struct(s),
                Item::Enum(e) => self
                    .byvalue_checker
                    .ingest_pod_type(TypeName::from_ident(&e.ident)),
                _ => {}
            }
        }
        self.byvalue_checker
            .satisfy_requests(self.pod_requests.clone())
            .map_err(ConvertError::UnsafePODType)
    }

    fn generate_type_alias(&mut self, tyname: TypeName, should_be_pod: bool) {
        let tyident = tyname.to_ident();
        let kind_item: Ident = Ident::new(
            if should_be_pod { "Trivial" } else { "Opaque" },
            Span::call_site(),
        );
        let tynamestring = tyname.to_cpp_name();
        let mut for_extern_c_ts = TokenStream2::new();
        // TODO - add #[rustfmt::skip] here until
        // https://github.com/rust-lang/rustfmt/issues/4159 is fixed.
        for_extern_c_ts.extend(
            [
                TokenTree::Ident(Ident::new("type", Span::call_site())),
                TokenTree::Ident(tyident.clone()),
                TokenTree::Punct(proc_macro2::Punct::new('=', proc_macro2::Spacing::Alone)),
                TokenTree::Ident(Ident::new("super", Span::call_site())),
                TokenTree::Punct(proc_macro2::Punct::new(':', proc_macro2::Spacing::Joint)),
                TokenTree::Punct(proc_macro2::Punct::new(':', proc_macro2::Spacing::Joint)),
                TokenTree::Ident(Ident::new("bindgen", Span::call_site())),
                TokenTree::Punct(proc_macro2::Punct::new(':', proc_macro2::Spacing::Joint)),
                TokenTree::Punct(proc_macro2::Punct::new(':', proc_macro2::Spacing::Joint)),
                TokenTree::Ident(tyident.clone()),
                TokenTree::Punct(proc_macro2::Punct::new(';', proc_macro2::Spacing::Alone)),
            ]
            .to_vec(),
        );
        self.extern_c_mod_items
            .push(ForeignItem::Verbatim(for_extern_c_ts));
        self.bridge_items.push(Item::Impl(parse_quote! {
            impl UniquePtr<#tyident> {}
        }));
        self.all_items.push(Item::Impl(parse_quote! {
            unsafe impl cxx::ExternType for bindgen::#tyident {
                type Id = cxx::type_id!(#tynamestring);
                type Kind = cxx::kind::#kind_item;
            }
        }));
        self.types_found.push(tyname);
    }

    fn build_include_foreign_items(&self, extra_inclusion: Option<&str>) -> Vec<ForeignItem> {
        let extra_owned = extra_inclusion.map(|x| x.to_owned());
        let chained = self.include_list.iter().chain(extra_owned.iter());
        chained
            .map(|inc| {
                ForeignItem::Macro(parse_quote! {
                    include!(#inc);
                })
            })
            .collect()
    }

    fn convert_items(
        mut self,
        items: Vec<Item>,
        extra_inclusion: Option<&str>,
    ) -> Result<BridgeConversionResults, ConvertError> {
        self.find_nested_pod_types(&items)?;
        self.extern_c_mod_items = self.build_include_foreign_items(extra_inclusion);
        for item in items {
            match item {
                Item::ForeignMod(mut fm) => {
                    let items = fm.items;
                    fm.items = Vec::new();
                    if self.extern_c_mod.is_none() {
                        self.extern_c_mod = Some(fm);
                        // We'll use the first 'extern "C"' mod we come
                        // across for attributes, spans etc. but we'll stuff
                        // the contents of all bindgen 'extern "C"' mods into this
                        // one.
                    }
                    self.convert_foreign_mod_items(items)?;
                }
                Item::Struct(mut s) => {
                    let tyname = TypeName::from_ident(&s.ident);
                    let should_be_pod = self.byvalue_checker.is_pod(&tyname);
                    self.generate_type_alias(tyname, should_be_pod);
                    if !should_be_pod {
                        s.fields = syn::Fields::Unit;
                    }
                    self.bindgen_items.push(Item::Struct(s));
                }
                Item::Enum(e) => {
                    let tyname = TypeName::from_ident(&e.ident);
                    self.generate_type_alias(tyname, true);
                    self.bindgen_items.push(Item::Enum(e));
                }
                Item::Impl(i) => {
                    if let Some(ty) = self.type_to_typename(&i.self_ty) {
                        for item in i.items.clone() {
                            match item {
                                syn::ImplItem::Method(m) if m.sig.ident == "new" => {
                                    self.convert_new_method(m, &ty, &i)
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {
                    self.all_items.push(item);
                }
            }
        }
        // We will always create an extern "C" mod even if bindgen
        // didn't generate one, e.g. because it only generated types.
        // We still want cxx to know about those types.
        let mut extern_c_mod = self
            .extern_c_mod
            .take()
            .unwrap_or_else(|| self.get_blank_extern_c_mod());
        extern_c_mod.items.append(&mut self.extern_c_mod_items);
        self.bridge_items.push(Item::ForeignMod(extern_c_mod));
        self.bindgen_mod
            .content
            .as_mut()
            .unwrap()
            .1
            .append(&mut self.bindgen_items);
        self.all_items.push(Item::Mod(self.bindgen_mod.clone()));
        let mut bridge_mod: ItemMod = parse_quote! {
            #[cxx::bridge]
            pub mod cxxbridge {
            }
        };
        bridge_mod
            .content
            .as_mut()
            .unwrap()
            .1
            .append(&mut self.bridge_items);
        self.all_items.push(Item::Mod(bridge_mod));
        Ok(BridgeConversionResults {
            items: self.all_items,
            additional_cpp_needs: self.additional_cpp_needs,
        })
    }

    fn convert_new_method(&mut self, mut m: syn::ImplItemMethod, ty: &TypeName, i: &syn::ItemImpl) {
        let (arrow, oldreturntype) = match &m.sig.output {
            ReturnType::Type(arrow, ty) => (arrow, ty),
            ReturnType::Default => return,
        };
        let constructor_args = m.sig.inputs.iter().filter_map(|x| match x {
            FnArg::Typed(pt) => {
                self.type_to_typename(&pt.ty)
                    .and_then(|x| match *(pt.pat.clone()) {
                        syn::Pat::Ident(pti) => Some((x, pti.ident)),
                        _ => None,
                    })
            }
            FnArg::Receiver(_) => None,
        });
        let (arg_types, arg_names): (Vec<_>, Vec<_>) = constructor_args.unzip();
        self.additional_cpp_needs
            .push(AdditionalNeed::MakeUnique(ty.clone(), arg_types));
        // Create a function which calls Bob_make_unique
        // from Bob::make_unique.
        let call_name = Ident::new(
            &format!("{}_make_unique", ty.to_string()),
            Span::call_site(),
        );
        m.block = parse_quote!( {
            super::cxxbridge::#call_name(
                #(#arg_names),*
            )
        });
        m.sig.ident = Ident::new("make_unique", Span::call_site());
        let new_return_type: TypePath = parse_quote! {
            cxx::UniquePtr < #oldreturntype >
        };
        m.sig.unsafety = None;
        m.sig.output = ReturnType::Type(*arrow, Box::new(Type::Path(new_return_type)));
        let new_impl_method = syn::ImplItem::Method(m);
        let mut new_item_impl = i.clone();
        new_item_impl.attrs = Vec::new();
        new_item_impl.unsafety = None;
        new_item_impl.items = vec![new_impl_method];
        self.bindgen_items.push(Item::Impl(new_item_impl));
    }

    fn get_blank_extern_c_mod(&self) -> ItemForeignMod {
        parse_quote!(
            extern "C" {}
        )
    }

    fn type_to_typename(&self, ty: &Type) -> Option<TypeName> {
        match ty {
            Type::Path(pn) => Some(TypeName::from_type_path(pn)),
            _ => None,
        }
    }

    fn convert_foreign_mod_items(
        &mut self,
        foreign_mod_items: Vec<ForeignItem>,
    ) -> Result<(), ConvertError> {
        for i in foreign_mod_items {
            match i {
                ForeignItem::Fn(f) => {
                    self.convert_foreign_fn(f)?;
                }
                _ => return Err(ConvertError::UnknownForeignItem),
            }
        }
        Ok(())
    }

    fn convert_foreign_fn(&mut self, fun: ForeignItemFn) -> Result<(), ConvertError> {
        let mut s = fun.sig.clone();
        let old_name = s.ident.to_string();
        // See if it's a constructor, in which case skip it.
        // We instead pass onto cxx an alternative make_unique implementation later.
        for ty in &self.types_found {
            let constructor_name = format!("{}_{}", ty, ty);
            if old_name == constructor_name {
                return Ok(());
            }
        }
        s.output = self.convert_return_type(s.output);
        let (new_params, param_details): (Punctuated<_, _>, Vec<_>) = fun
            .sig
            .inputs
            .into_iter()
            .map(|i| self.convert_fn_arg(i))
            .unzip();
        s.inputs = new_params;
        let is_a_method = param_details.iter().any(|b| b.was_self);

        if is_a_method {
            // bindgen generates methods with the name:
            // {class}_{method name}
            // It then generates an impl section for the Rust type
            // with the original name, but we currently discard that impl section.
            // We want to feed cxx methods with just the method name, so let's
            // strip off the class name.
            // TODO test with class names containing underscores. It should work.
            for cn in &self.types_found {
                if let Some(suffix) = cn.prefixes(&old_name) {
                    s.ident = Ident::new(suffix, s.ident.span());
                    break;
                }
            }
        }

        let unique_ptr_wrapper_needed = param_details.iter().any(|b| b.conversion.work_needed());
        let ret_type_conversion = self.unwrap_return_type(s.output.clone());
        let ret_type_conversion_needed = ret_type_conversion
            .as_ref()
            .map_or(false, |x| x.work_needed());
        if unique_ptr_wrapper_needed || ret_type_conversion_needed {
            let a = AdditionalNeed::ByValueWrapper(
                s.ident.clone(),
                ret_type_conversion,
                param_details.into_iter().map(|d| d.conversion).collect(),
            );
            self.additional_cpp_needs.push(a);
        }

        let mut attrs = self.strip_attr(fun.attrs, "link_name");
        let new_name = self.renames.get(&old_name);
        if let Some(new_name) = new_name {
            attrs.push(parse_quote!(
                #[rust_name = #new_name]
            ));
        }

        let new_item = ForeignItemFn {
            attrs,
            vis: fun.vis,
            sig: s,
            semi_token: fun.semi_token,
        };
        self.extern_c_mod
            .as_mut()
            .unwrap()
            .items
            .push(ForeignItem::Fn(new_item));
        Ok(())
    }

    fn unwrap_return_type(&self, ret_type: ReturnType) -> Option<ArgumentConversion> {
        match ret_type {
            ReturnType::Type(_, boxed_type) => Some(
                if !self
                    .byvalue_checker
                    .is_pod(&TypeName::from_type(&*boxed_type))
                {
                    ArgumentConversion::to_unique_ptr(*boxed_type)
                } else {
                    ArgumentConversion::unconverted(*boxed_type)
                },
            ),
            ReturnType::Default => None,
        }
    }

    fn strip_attr(&self, attrs: Vec<Attribute>, to_strip: &str) -> Vec<Attribute> {
        attrs
            .into_iter()
            .filter(|a| {
                let i = a.path.get_ident();
                !matches!(i, Some(i2) if *i2 == to_strip)
            })
            .collect::<Vec<Attribute>>()
    }

    /// Returns additionally a Boolean indicating whether an argument was
    /// 'this' and another one indicating whether we took a type by value
    /// and that type was non-trivial.
    fn convert_fn_arg(&self, arg: FnArg) -> (FnArg, ArgumentAnalysis) {
        match arg {
            FnArg::Typed(mut pt) => {
                let mut found_this = false;
                let old_pat = *pt.pat;
                let new_pat = match old_pat {
                    syn::Pat::Ident(mut pp) if pp.ident == "this" => {
                        found_this = true;
                        pp.ident = Ident::new("self", pp.ident.span());
                        syn::Pat::Ident(pp)
                    }
                    _ => old_pat,
                };
                let new_ty = self.convert_boxed_type(pt.ty);
                let conversion = self.conversion_required(&new_ty);
                pt.pat = Box::new(new_pat);
                pt.ty = new_ty;
                (
                    FnArg::Typed(pt),
                    ArgumentAnalysis {
                        was_self: found_this,
                        conversion,
                    },
                )
            }
            _ => panic!("FnArg::Receiver not yet handled"),
        }
    }

    fn conversion_required(&self, ty: &Type) -> ArgumentConversion {
        match ty {
            Type::Path(p) => {
                if self.byvalue_checker.is_pod(&TypeName::from_type_path(p)) {
                    ArgumentConversion::unconverted(ty.clone())
                } else {
                    ArgumentConversion::from_unique_ptr(ty.clone())
                }
            }
            _ => ArgumentConversion::unconverted(ty.clone()),
        }
    }

    fn convert_return_type(&self, rt: ReturnType) -> ReturnType {
        match rt {
            ReturnType::Default => ReturnType::Default,
            ReturnType::Type(rarrow, typebox) => {
                ReturnType::Type(rarrow, self.convert_boxed_type(typebox))
            }
        }
    }

    fn convert_boxed_type(&self, ty: Box<Type>) -> Box<Type> {
        Box::new(self.convert_type(*ty))
    }

    fn convert_type(&self, ty: Type) -> Type {
        match ty {
            Type::Path(p) => Type::Path(self.convert_type_path(p)),
            Type::Reference(mut r) => {
                r.elem = self.convert_boxed_type(r.elem);
                Type::Reference(r)
            }
            Type::Ptr(ptr) => Type::Reference(self.convert_ptr_to_reference(ptr)),
            _ => ty,
        }
    }

    fn convert_ptr_to_reference(&self, ptr: TypePtr) -> TypeReference {
        let mutability = ptr.mutability;
        let elem = self.convert_boxed_type(ptr.elem);
        parse_quote! {
            & #mutability #elem
        }
    }

    fn convert_type_path(&self, typ: TypePath) -> TypePath {
        let p = typ.path;
        let new_p = Path {
            leading_colon: p.leading_colon,
            segments: p
                .segments
                .into_iter()
                .map(|s| -> PathSegment {
                    let ident = TypeName::from_ident(&s.ident);
                    // May replace non-canonical names e.g. std_string
                    // with canonical equivalents, e.g. CxxString
                    let ident = ident.to_ident();
                    let args = match s.arguments {
                        PathArguments::AngleBracketed(mut ab) => {
                            ab.args = self.convert_punctuated(ab.args);
                            PathArguments::AngleBracketed(ab)
                        }
                        _ => s.arguments,
                    };
                    parse_quote!( #ident #args )
                })
                .collect(),
        };
        TypePath {
            qself: typ.qself,
            path: new_p,
        }
    }

    fn convert_punctuated<P>(
        &self,
        pun: Punctuated<GenericArgument, P>,
    ) -> Punctuated<GenericArgument, P>
    where
        P: Default,
    {
        let mut new_pun = Punctuated::new();
        for arg in pun.into_iter() {
            new_pun.push(match arg {
                GenericArgument::Type(t) => GenericArgument::Type(self.convert_type(t)),
                _ => arg,
            });
        }
        new_pun
    }
}

struct ArgumentAnalysis {
    conversion: ArgumentConversion,
    was_self: bool,
}
