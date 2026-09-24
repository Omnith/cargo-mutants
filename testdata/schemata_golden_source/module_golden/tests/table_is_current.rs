//! Checks that the generated module matches its golden copy.

#[test]
fn table_matches_golden_copy() {
    let table = std::fs::read_to_string("src/table.rs").unwrap();
    assert_eq!(table, include_str!("golden/table.rs.golden"));
}
