//! Procedural macros of `tiermem`. Use them through the `tiermem` crate, which re-exports them.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{
    Attribute, Data, DeriveInput, Error, Expr, Fields, GenericParam, Path, parse_macro_input,
    parse_quote,
};

/// Makes a struct or an enum storable in a `TierMem`.
///
/// Adds `serde::Serialize` and `serde::Deserialize` derives and implements `get_size2::GetSize`,
/// which `TierMem` uses to count the RAM taken by cached objects. Other derives and attributes are
/// kept. Type parameters get a `GetSize` bound; lifetimes and unions are rejected.
///
/// The heap size of each field is computed with its own `GetSize` implementation, unless the field
/// has one of these options:
///
/// - `#[storable(ignore)]`: counts no heap bytes;
/// - `#[storable(heap_size = N)]`: counts the expression `N`;
/// - `#[storable(heap_size_fn = path)]`: counts `path(&field)`, a `fn(&FieldType) -> usize`.
#[proc_macro_attribute]
pub fn storable(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = TokenStream2::from(attr);
    let input = parse_macro_input!(item as DeriveInput);
    expand(attr, input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// How the heap size of a field is counted.
enum HeapSize {
    GetSize,
    Ignore,
    Fixed(Expr),
    Function(Path),
}

fn expand(attr: TokenStream2, mut input: DeriveInput) -> syn::Result<TokenStream2> {
    if !attr.is_empty() {
        return Err(Error::new(attr.span(), "`#[storable]` takes no arguments"));
    }
    let get_size = quote!(::tiermem::__private::get_size2);

    let arms = match &mut input.data {
        Data::Struct(data) => vec![arm(quote!(Self), &mut data.fields, &get_size)?],
        Data::Enum(data) => data
            .variants
            .iter_mut()
            .map(|variant| {
                let name = &variant.ident;
                arm(quote!(Self::#name), &mut variant.fields, &get_size)
            })
            .collect::<syn::Result<_>>()?,
        Data::Union(data) => {
            return Err(Error::new(
                data.union_token.span,
                "`#[storable]` does not support unions",
            ));
        }
    };

    let mut generics = input.generics.clone();
    for param in &mut generics.params {
        match param {
            GenericParam::Type(param) => param.bounds.push(parse_quote!(#get_size::GetSize)),
            GenericParam::Lifetime(param) => {
                return Err(Error::new(
                    param.span(),
                    "`#[storable]` types can't borrow: they must be `'static`",
                ));
            }
            GenericParam::Const(_) => {}
        }
    }
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let (_, type_generics, _) = input.generics.split_for_impl();
    let name = &input.ident;

    Ok(quote! {
        #[derive(::tiermem::__private::serde::Serialize, ::tiermem::__private::serde::Deserialize)]
        #[serde(crate = "::tiermem::__private::serde")]
        #input

        #[automatically_derived]
        impl #impl_generics #get_size::GetSize for #name #type_generics #where_clause {
            #[allow(unused_mut, unused_variables, unreachable_code)]
            fn get_heap_size_with_tracker<__TiermemTracker: #get_size::GetSizeTracker>(
                &self,
                tracker: __TiermemTracker,
            ) -> (usize, __TiermemTracker) {
                let mut __tiermem_tracker = tracker;
                let mut __tiermem_total = 0usize;
                match self {
                    #(#arms)*
                }
                (__tiermem_total, __tiermem_tracker)
            }
        }
    })
}

/// Match arm adding up the heap size of the fields of one struct or variant. Removes the
/// `#[storable(..)]` field attributes, which the compiler would not know.
fn arm(path: TokenStream2, fields: &mut Fields, get_size: &TokenStream2) -> syn::Result<TokenStream2> {
    let mut patterns = Vec::new();
    let mut sums = Vec::new();
    for (index, field) in fields.iter_mut().enumerate() {
        let heap_size = take_options(&mut field.attrs)?;
        let binding = format_ident!("__tiermem_field_{}", index);
        let member = match &field.ident {
            Some(ident) => quote!(#ident),
            None => {
                let index = syn::Index::from(index);
                quote!(#index)
            }
        };
        patterns.push(quote!(#member: #binding));
        sums.push(match heap_size {
            HeapSize::GetSize => quote! {
                let (size, tracker) =
                    #get_size::GetSize::get_heap_size_with_tracker(#binding, __tiermem_tracker);
                __tiermem_total += size;
                __tiermem_tracker = tracker;
            },
            HeapSize::Ignore => quote!(),
            HeapSize::Fixed(size) => quote!(__tiermem_total += #size;),
            HeapSize::Function(function) => quote!(__tiermem_total += #function(#binding);),
        });
    }
    // `Path { 0: a, 1: b }` also matches tuple structs, and `Path {}` unit ones.
    Ok(quote!(#path { #(#patterns),* } => { #(#sums)* }))
}

fn take_options(attrs: &mut Vec<Attribute>) -> syn::Result<HeapSize> {
    let mut options = Vec::new();
    attrs.retain(|attr| {
        let ours = attr.path().is_ident("storable");
        if ours {
            options.push(attr.clone());
        }
        !ours
    });

    let mut heap_size = HeapSize::GetSize;
    for attr in options {
        attr.parse_nested_meta(|meta| {
            if !matches!(heap_size, HeapSize::GetSize) {
                return Err(meta.error("only one `storable` option per field"));
            }
            heap_size = if meta.path.is_ident("ignore") {
                HeapSize::Ignore
            } else if meta.path.is_ident("heap_size") {
                HeapSize::Fixed(meta.value()?.parse()?)
            } else if meta.path.is_ident("heap_size_fn") {
                HeapSize::Function(meta.value()?.parse()?)
            } else {
                return Err(meta.error(
                    "expected `ignore`, `heap_size = N` or `heap_size_fn = path`",
                ));
            };
            Ok(())
        })?;
    }
    Ok(heap_size)
}
