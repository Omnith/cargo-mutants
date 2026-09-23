use proc_macro::TokenStream;

/// Expand to the greeting from `words`, as a string literal.
#[proc_macro]
pub fn greeting(_input: TokenStream) -> TokenStream {
    format!("{:?}", words::greeting()).parse().unwrap()
}
