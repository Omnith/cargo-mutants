//! Fail the build with a full disk's message when one mutant, `x * 2` to `x + 2` in
//! `double`, is applied, or when the schema holds it. Every other mutant builds, so a
//! run with two jobs shows whether the second worker stops.
//!
//! No other line of `src/lib.rs` mutates into `x + 2`.

use std::fs::read_to_string;
use std::process::exit;

fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    // the classic way writes `x + /* ~ changed by cargo-mutants ~ */ 2`, and the schema
    // writes `x + 2`
    let source = read_to_string("src/lib.rs")
        .expect("read src/lib.rs")
        .replace("/* ~ changed by cargo-mutants ~ */ ", "");
    if source.contains("x + 2") {
        eprintln!("error: No space left on device (os error 28)");
        exit(1);
    }
}
