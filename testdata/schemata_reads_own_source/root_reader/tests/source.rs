//! Checks the form of the code by reading the crate root.

#[test]
fn sub_formula_is_in_crate_root() {
    assert!(include_str!("../src/lib.rs").contains("a - b"));
}
