//! Attribute macros that transform function bodies.

use proc_macro::{Delimiter, Group, Ident, Punct, Spacing, Span, TokenStream, TokenTree};

/// Run the function's body in a closure, keeping the body's spans.
#[proc_macro_attribute]
pub fn in_closure(_attr: TokenStream, item: TokenStream) -> TokenStream {
    map_body(item, |body| {
        // `(move || { body })()`
        let closure: TokenStream = [
            TokenTree::Ident(Ident::new("move", Span::call_site())),
            TokenTree::Punct(Punct::new('|', Spacing::Joint)),
            TokenTree::Punct(Punct::new('|', Spacing::Alone)),
            TokenTree::Group(body),
        ]
        .into_iter()
        .collect();
        [
            TokenTree::Group(Group::new(Delimiter::Parenthesis, closure)),
            TokenTree::Group(Group::new(Delimiter::Parenthesis, TokenStream::new())),
        ]
        .into_iter()
        .collect()
    })
}

/// Give every token of the function's body the span of the attribute.
#[proc_macro_attribute]
pub fn respan(_attr: TokenStream, item: TokenStream) -> TokenStream {
    map_body(item, |body| respan_tokens(body.stream(), Span::call_site()))
}

/// Replace the body of a function, which is its last token, with `f(body)` in braces.
fn map_body(item: TokenStream, f: impl FnOnce(Group) -> TokenStream) -> TokenStream {
    let mut tokens: Vec<TokenTree> = item.into_iter().collect();
    let Some(TokenTree::Group(body)) = tokens.pop() else {
        panic!("expected a function with a body");
    };
    assert!(matches!(body.delimiter(), Delimiter::Brace));
    tokens.push(TokenTree::Group(Group::new(Delimiter::Brace, f(body))));
    tokens.into_iter().collect()
}

fn respan_tokens(tokens: TokenStream, span: Span) -> TokenStream {
    tokens
        .into_iter()
        .map(|token| match token {
            TokenTree::Group(group) => {
                let mut group = Group::new(group.delimiter(), respan_tokens(group.stream(), span));
                group.set_span(span);
                TokenTree::Group(group)
            }
            mut token => {
                token.set_span(span);
                token
            }
        })
        .collect()
}
