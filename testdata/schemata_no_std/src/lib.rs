//! A `#![no_std]` library.

#![no_std]

/// Checked by a doctest.
///
/// ```
/// assert_eq!(cargo_mutants_testdata_schemata_no_std::clamp_add(250, 10), 255);
/// ```
pub fn clamp_add(a: u8, b: u8) -> u8 {
    a.saturating_add(b)
}

macro_rules! halve {
    ($x:expr) => {
        $x / 2
    };
}

/// The code in the macro definition above isn't mutated, only this function.
pub fn half(x: u32) -> u32 {
    halve!(x)
}

#[cfg(feature = "never")]
pub fn disabled(x: u32) -> u32 {
    x + 1
}

#[cfg(test)]
mod test {
    #[test]
    fn half_of_ten() {
        assert_eq!(super::half(10), 5);
    }
}
