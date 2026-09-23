//! Functions whose bodies are transformed by attribute macros.

use macros::in_closure;

mod respanned;

pub use respanned::{odds, product, started};

#[in_closure]
pub fn sum(a: u32, b: u32) -> u32 {
    a + b
}

/// Different replacement values have different types, so some are dropped from the schema.
#[in_closure]
pub fn evens(limit: u32) -> impl Iterator<Item = u32> {
    (0..limit).filter(|x| x % 2 == 0)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn transformed_functions() {
        assert_eq!(sum(2, 3), 5);
        assert_eq!(evens(5).collect::<Vec<_>>(), [0, 2, 4]);
        assert_eq!(odds(6).collect::<Vec<_>>(), [1, 3, 5]);
        assert_eq!(product(3, 4), 12);
    }
}
