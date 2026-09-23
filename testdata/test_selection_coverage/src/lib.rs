//! Functions with different coverage by tests.

use std::thread::sleep;
use std::time::Duration;

/// Executed by a fast and a slow test, which both check the result.
pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

/// Not executed by any test.
pub fn untested(n: u32) -> u32 {
    n * 3 + 1
}

/// Executed by a test that doesn't check the boundary.
pub fn is_big(n: u32) -> bool {
    n > 100
}

/// Only executed by the `shout` binary, which only an integration test runs.
pub fn shout(s: &str) -> String {
    s.to_uppercase() + "!"
}

/// Waits, so that a test is slow.
pub fn pause() {
    sleep(Duration::from_millis(300));
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn add_small_numbers() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn add_slowly() {
        pause();
        assert_eq!(add(10, 20), 30);
    }

    #[test]
    fn big_numbers_are_big() {
        assert!(is_big(1000));
        assert!(!is_big(1));
    }
}
