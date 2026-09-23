//! Doesn't depend on package `a`, so mutating `a` doesn't by itself cause this to be rebuilt.
//!
//! This is a `const` so that it has no mutants.

/// The directory where this package was compiled.
pub const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
