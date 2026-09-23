//! A test target whose only test is ignored, so it runs no tests.
//!
//! `--schemata` sees in the baseline that this target runs nothing, and doesn't
//! replay it for each mutant.

#[test]
#[ignore = "stands in for a test that needs an external service"]
fn needs_an_external_service() {
    panic!("ignored tests don't run");
}
