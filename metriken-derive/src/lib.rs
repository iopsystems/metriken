// Copyright 2021 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

use proc_macro::TokenStream;

mod args;
mod metric;

/// Declare a global metric that can be accessed via the `metrics` method.
///
/// Note that this will change the type of the generated static to be
/// `MetricInstance<MetricTy>`. It implements both [`Deref`] and [`DerefMut`]
/// so it can be used much the same as a normal static.
///
/// # Parameters
/// - (optional) `crate`: The path to the `metriken` crate. This allows the
///   `metric` macro to be used within other macros that get exported to
///   third-party crates which may not have added `metriken` to their
///   Cargo.toml.
/// - (optional) `formatter`: A function to be used to determine the output name
///   for this metric.
///
/// [`Deref`]: std::ops::Deref
/// [`DerefMut`]: std::ops::DerefMut
#[proc_macro_attribute]
pub fn metric(attr: TokenStream, item: TokenStream) -> TokenStream {
    match metric::metric(attr, item) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}
