use heck::ToSnakeCase;
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields};

pub(crate) fn derive_metric_dimension(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let ast: DeriveInput = syn::parse(input).expect("failed to parse derive input");
    match impl_metric_dimension(ast) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn impl_metric_dimension(ast: DeriveInput) -> syn::Result<TokenStream> {
    let enum_name = &ast.ident;

    let variants = match &ast.data {
        Data::Enum(data) => &data.variants,
        _ => {
            return Err(syn::Error::new_spanned(
                &ast.ident,
                "MetricDimension can only be derived for enums",
            ))
        }
    };

    for variant in variants {
        if !matches!(&variant.fields, Fields::Unit) {
            return Err(syn::Error::new_spanned(
                &variant.ident,
                "MetricDimension variants must have no fields",
            ));
        }
    }

    // Label key: snake_case of enum name, overridable via #[metric_dimension(name="...")]
    let label_key =
        get_dimension_name_override(&ast).unwrap_or_else(|| enum_name.to_string().to_snake_case());

    let count = variants.len();
    let variant_idents: Vec<_> = variants.iter().map(|v| &v.ident).collect();
    let variant_indices: Vec<usize> = (0..count).collect();
    let label_values: Vec<String> = variant_idents
        .iter()
        .map(|id| id.to_string().to_snake_case())
        .collect();

    let index_arms = variant_idents
        .iter()
        .zip(variant_indices.iter())
        .map(|(id, idx)| {
            quote! { Self::#id => #idx }
        });

    let labels_arms = variant_idents
        .iter()
        .zip(label_values.iter())
        .map(|(id, val)| {
            quote! {
                Self::#id => { map.insert(#label_key.to_string(), #val.to_string()); }
            }
        });

    let all_labels_entries = label_values.iter().map(|val| {
        quote! {{
            let mut m = ::std::collections::HashMap::new();
            m.insert(#label_key.to_string(), #val.to_string());
            m
        }}
    });

    Ok(quote! {
        impl ::metriken::MetricDimension for #enum_name {
            const COUNT: usize = #count;

            fn index(&self) -> usize {
                match self {
                    #( #index_arms, )*
                }
            }

            fn labels(&self) -> ::std::collections::HashMap<String, String> {
                let mut map = ::std::collections::HashMap::new();
                match self {
                    #( #labels_arms )*
                }
                map
            }

            fn all_labels() -> Vec<::std::collections::HashMap<String, String>> {
                vec![ #( #all_labels_entries, )* ]
            }
        }
    })
}

/// Look for `#[metric_dimension(name = "custom_key")]` on the enum.
fn get_dimension_name_override(ast: &DeriveInput) -> Option<String> {
    for attr in &ast.attrs {
        if attr.path().is_ident("metric_dimension") {
            if let Ok(mnv) = attr.parse_args::<syn::MetaNameValue>() {
                if mnv.path.is_ident("name") {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) = &mnv.value
                    {
                        return Some(s.value());
                    }
                }
            }
        }
    }
    None
}
