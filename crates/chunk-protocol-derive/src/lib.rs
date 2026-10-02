//! Struct codecs and packet metadata. Re-exported by `chunk-protocol`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::quote;
use syn::{Data, DataStruct, DeriveInput, Fields, Generics, Index, LitInt, Path, parse_macro_input, parse_quote};

#[proc_macro_derive(Encode)]
pub fn encode(input: TokenStream) -> TokenStream {
    encode_impl(&parse_macro_input!(input as DeriveInput)).unwrap_or_else(syn::Error::into_compile_error).into()
}

#[proc_macro_derive(Decode)]
pub fn decode(input: TokenStream) -> TokenStream {
    decode_impl(&parse_macro_input!(input as DeriveInput)).unwrap_or_else(syn::Error::into_compile_error).into()
}

fn struct_data(input: &DeriveInput) -> syn::Result<&DataStruct> {
    match &input.data {
        Data::Struct(data) => Ok(data),
        _ => Err(syn::Error::new_spanned(input, "codec derives support structs only")),
    }
}

/// The input's generics with every field type bounded by `bound`.
fn bounded(input: &DeriveInput, data: &DataStruct, bound: &Path) -> Generics {
    let mut generics = input.generics.clone();
    for field in &data.fields {
        let ty = &field.ty;
        generics.make_where_clause().predicates.push(parse_quote!(#ty: #bound));
    }
    generics
}

fn encode_impl(input: &DeriveInput) -> syn::Result<Tokens> {
    let data = struct_data(input)?;
    let name = &input.ident;
    let generics = bounded(input, data, &parse_quote!(::chunk_protocol::Encode));
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let fields = data.fields.iter().enumerate().map(|(index, field)| {
        let member = field.ident.clone().map_or_else(|| syn::Member::Unnamed(Index::from(index)), syn::Member::Named);
        quote!(::chunk_protocol::Encode::encode(&self.#member, output)?;)
    });
    Ok(quote! {
        impl #impl_generics ::chunk_protocol::Encode for #name #ty_generics #where_clause {
            fn encode(&self, output: &mut ::std::vec::Vec<u8>) -> ::chunk_protocol::Result<()> {
                #(#fields)*
                Ok(())
            }
        }
    })
}

fn decode_impl(input: &DeriveInput) -> syn::Result<Tokens> {
    let data = struct_data(input)?;
    let name = &input.ident;
    let generics = bounded(input, data, &parse_quote!(::chunk_protocol::Decode));
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let fields = data.fields.iter().map(|field| {
        let ty = &field.ty;
        let value = quote!(<#ty as ::chunk_protocol::Decode>::decode(input)?);
        field.ident.as_ref().map_or_else(|| value.clone(), |name| quote!(#name: #value))
    });
    let value = match &data.fields {
        Fields::Named(_) => quote!(Self { #(#fields),* }),
        Fields::Unnamed(_) => quote!(Self(#(#fields),*)),
        Fields::Unit => quote!(Self),
    };
    Ok(quote! {
        impl #impl_generics ::chunk_protocol::Decode for #name #ty_generics #where_clause {
            fn decode(input: &mut &[u8]) -> ::chunk_protocol::Result<Self> {
                Ok(#value)
            }
        }
    })
}

/// Declares `#[packet(id = 0x00, state = Status, direction = Serverbound)]`.
#[proc_macro_derive(Packet, attributes(packet))]
pub fn packet(input: TokenStream) -> TokenStream {
    packet_impl(&parse_macro_input!(input as DeriveInput)).unwrap_or_else(syn::Error::into_compile_error).into()
}

fn packet_impl(input: &DeriveInput) -> syn::Result<Tokens> {
    let mut id = None;
    let mut state = None;
    let mut direction = None;
    for attr in input.attrs.iter().filter(|attr| attr.path().is_ident("packet")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("id") {
                if id.is_some() {
                    return Err(meta.error("duplicate packet id"));
                }
                let value: LitInt = meta.value()?.parse()?;
                let value = value.base10_parse::<i32>()?;
                if value < 0 {
                    return Err(meta.error("packet id must be nonnegative"));
                }
                id = Some(value);
            } else if meta.path.is_ident("state") {
                if state.is_some() {
                    return Err(meta.error("duplicate packet state"));
                }
                state = Some(meta.value()?.parse::<syn::Ident>()?);
            } else if meta.path.is_ident("direction") {
                if direction.is_some() {
                    return Err(meta.error("duplicate packet direction"));
                }
                direction = Some(meta.value()?.parse::<syn::Ident>()?);
            } else {
                return Err(meta.error("expected id, state, or direction"));
            }
            Ok(())
        })?;
    }
    let (Some(id), Some(state), Some(direction)) = (id, state, direction) else {
        return Err(syn::Error::new_spanned(input, "packet requires id, state, and direction"));
    };
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics ::chunk_protocol::Packet for #name #ty_generics #where_clause {
            const ID: i32 = #id;
            const STATE: ::chunk_protocol::State = ::chunk_protocol::State::#state;
            const DIRECTION: ::chunk_protocol::Direction = ::chunk_protocol::Direction::#direction;
        }
    })
}

#[cfg(test)]
mod tests;
