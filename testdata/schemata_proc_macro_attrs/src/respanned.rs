//! Functions whose bodies have the span of the attribute.

use macros::respan;

/// Returns `impl Trait`, so replacements of its value are tested the classic way.
#[respan]
pub fn odds(limit: u32) -> impl Iterator<Item = u32> {
    (0..limit).filter(|x| x % 2 == 1)
}

#[respan]
pub fn product(a: u32, b: u32) -> u32 {
    a * b
}

/// `Instant` has no `Default`, so replacing this function's value gives the schema
/// an error, which points at the attribute.
#[respan]
pub fn started() -> std::time::Instant {
    std::time::Instant::now()
}
