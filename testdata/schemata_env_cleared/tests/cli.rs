//! Runs the binary with an empty environment.

use std::process::Command;

#[test]
fn greets_with_cleared_environment() {
    let output = Command::new(env!(
        "CARGO_BIN_EXE_cargo-mutants-testdata-schemata-env-cleared"
    ))
    .arg("world")
    .env_clear()
    .output()
    .unwrap();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "Hello, world!\n");
}
