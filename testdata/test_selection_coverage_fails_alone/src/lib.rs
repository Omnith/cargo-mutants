//! A test that depends on state set by another test.

use std::sync::atomic::{AtomicBool, Ordering};

pub static READY: AtomicBool = AtomicBool::new(false);

pub fn set_ready() {
    READY.store(true, Ordering::SeqCst);
}

pub fn quadruple(x: u32) -> u32 {
    x * 4
}

#[cfg(test)]
mod test {
    use std::thread::sleep;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_sets_ready() {
        set_ready();
    }

    /// Fails when run alone, since only `a_sets_ready` makes it ready.
    #[test]
    fn b_quadruples_once_ready() {
        for _ in 0..200 {
            if READY.load(Ordering::SeqCst) {
                break;
            }
            sleep(Duration::from_millis(10));
        }
        assert!(READY.load(Ordering::SeqCst), "not ready");
        assert_eq!(quadruple(2), 8);
    }
}
