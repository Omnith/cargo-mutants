//! Prints one result, then runs until it's killed.

use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

use cargo_mutants_testdata_test_selection_coverage_killed_child::triple;

fn main() {
    println!("{}", triple(2));
    std::io::stdout().flush().unwrap();
    loop {
        sleep(Duration::from_secs(1));
    }
}
