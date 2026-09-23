//! A function whose mutant makes one test hang and another fail.

pub fn step() -> u32 {
    1
}

#[cfg(test)]
mod test {
    use std::thread::sleep;
    use std::time::Duration;

    use super::*;

    /// Slow, and fails if `step` doesn't return 1.
    #[test]
    fn check_step_after_pause() {
        sleep(Duration::from_millis(300));
        assert_eq!(step(), 1);
    }

    /// Fast, and never finishes if `step` returns 0.
    #[test]
    fn steps_count_to_ten() {
        let mut n = 0;
        while n < 10 {
            n += step();
        }
    }
}
