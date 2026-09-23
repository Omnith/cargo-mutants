//! When `is_answer` is mutated to return false, `fast_test_fails_when_mutated` fails at once while
//! `slow_test_passes_when_mutated` sleeps and then passes. Stopping at the first failure means
//! `slow_test_passes_when_mutated` never gets to report `ok`.

use std::thread::sleep;
use std::time::Duration;

pub fn is_answer(x: u32) -> bool {
    x == 42
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn fast_test_fails_when_mutated() {
        assert!(is_answer(42));
    }

    #[test]
    fn slow_test_passes_when_mutated() {
        if !is_answer(42) {
            sleep(Duration::from_secs(5));
        }
    }
}
