//! Code exercising every mutant genre, for comparing `--schemata` with classic
//! mutation testing.

use std::collections::HashMap;

mod shapes;

pub use shapes::{Area, Rect};

/// Binary operators, nested inside a function that also has `FnValue` mutants.
pub fn weighted_sum(a: u32, b: u32, c: u32) -> u32 {
    a + b * c
}

/// Replacing `*` with `+` here classically parses as `(a - b) + c`, so that mutant's
/// site is the whole expression.
pub fn difference_of_product(a: i64, b: i64, c: i64) -> i64 {
    a - b * c
}

/// Tested only where `b * c == b + c`, so that replacing `*` with `+` is caught only
/// with the classic grouping `(a - b) + c`, not with `a - (b + c)`.
pub fn discount(price: i64, count: i64, each: i64) -> i64 {
    price - count * each
}

/// Compound assignments in a loop.
///
/// This uses `u8` so that replacing `i += 1` with `i *= 1`, which stops `i`
/// increasing, overflows `total` and panics after a few hundred iterations, rather
/// than after billions, which would race the test timeout.
pub fn triangle(n: u8) -> u8 {
    let mut total = 0;
    let mut i = 1;
    while i <= n {
        total += i;
        i += 1;
    }
    total
}

/// Replacing `+=` with `*=` makes this loop never terminate.
pub fn spin(limit: u64) -> u64 {
    let mut spins = 0;
    while spins < limit {
        spins += 1;
        std::hint::spin_loop();
    }
    spins
}

/// Unary operators.
pub fn negate_unless(x: i32, keep: bool) -> i32 {
    if !keep { -x } else { x }
}

/// Match arms with a catch-all, and a guard.
pub fn classify(n: i32) -> &'static str {
    match n {
        0 => "zero",
        x if x < 0 => "negative",
        1 | 2 | 3 => "small",
        _ => "large",
    }
}

/// A let chain: replacing its `&&` can't be embedded.
pub fn first_is_big(v: &[u32]) -> bool {
    if let Some(&x) = v.first()
        && x > 10
    {
        true
    } else {
        false
    }
}

/// Const contexts can't read the mutant id at runtime.
pub const LIMIT: u32 = 2 + 3;

pub const fn double(x: u32) -> u32 {
    x * 2
}

/// Deleting a field from a struct literal with a base isn't embedded.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub name: String,
    pub size: usize,
}

pub fn config(size: usize) -> Config {
    Config {
        size,
        ..Default::default()
    }
}

/// Various return types for `FnValue` mutants.
pub fn label(n: u32) -> String {
    format!("n={n}")
}

pub fn evens(limit: u32) -> Vec<u32> {
    (0..limit).filter(|x| x % 2 == 0).collect()
}

/// Each replacement value has its own type, but a function returning `impl Trait` can
/// return only one, so these replacements are tested the classic way.
pub fn odds(limit: u32) -> impl Iterator<Item = u32> {
    (0..limit).filter(|x| x % 2 == 1)
}

/// `Instant` has no `Default`, so replacing this function's value is unviable, in the
/// schema and classically.
pub fn started() -> std::time::Instant {
    std::time::Instant::now()
}

pub fn lookup(map: &HashMap<String, u32>, key: &str) -> Option<u32> {
    map.get(key).copied()
}

pub fn parse_positive(s: &str) -> Result<u32, String> {
    let n: i64 = s.parse().map_err(|e| format!("{e}"))?;
    if n > 0 {
        Ok(n as u32)
    } else {
        Err("not positive".to_owned())
    }
}

pub async fn add_async(a: u32, b: u32) -> u32 {
    a + b
}

/// Weakly tested, so some mutants are missed.
pub fn is_even_and_small(n: u32) -> bool {
    n % 2 == 0 && n < 100
}

/// Tested only by this doctest.
///
/// ```
/// assert_eq!(cargo_mutants_testdata_schemata::scale(3, 4), 7);
/// ```
pub fn scale(offset: u32, factor: u32) -> u32 {
    offset + factor
}

#[cfg(test)]
mod test {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use super::*;

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }

    #[test]
    fn arithmetic() {
        assert_eq!(weighted_sum(2, 3, 4), 14);
        assert_eq!(difference_of_product(20, 3, 4), 8);
        assert_eq!(discount(10, 2, 2), 6);
        assert_eq!(triangle(4), 10);
        assert_eq!(spin(10), 10);
        assert_eq!(double(21), 42);
        assert_eq!(LIMIT, 5);
    }

    #[test]
    fn logic() {
        assert_eq!(negate_unless(3, false), -3);
        assert_eq!(negate_unless(3, true), 3);
        assert_eq!(classify(0), "zero");
        assert_eq!(classify(-4), "negative");
        assert_eq!(classify(2), "small");
        assert_eq!(classify(7), "large");
        assert!(first_is_big(&[11]));
        assert!(!first_is_big(&[3]));
        assert!(!first_is_big(&[]));
        assert!(is_even_and_small(4));
        assert!(!is_even_and_small(3));
    }

    #[test]
    fn values() {
        assert_eq!(config(3).size, 3);
        assert_eq!(label(5), "n=5");
        assert_eq!(evens(5), [0, 2, 4]);
        assert_eq!(odds(6).collect::<Vec<_>>(), [1, 3, 5]);
        let map = HashMap::from([("a".to_owned(), 7)]);
        assert_eq!(lookup(&map, "a"), Some(7));
        assert_eq!(lookup(&map, "b"), None);
        assert_eq!(parse_positive("12"), Ok(12));
        assert!(parse_positive("-1").is_err());
        assert_eq!(block_on(add_async(2, 3)), 5);
    }
}
