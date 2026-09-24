//! Checks that the crate root matches its golden copy.

#[test]
fn lib_matches_golden_copy() {
    let lib = std::fs::read_to_string("src/lib.rs").unwrap();
    assert_eq!(lib, include_str!("golden/lib.rs.golden"));
}
