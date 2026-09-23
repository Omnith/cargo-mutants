//! Functions executed by tests in different processes.

/// Executed by a unit test.
pub fn double(x: u32) -> u32 {
    x * 2
}

/// Only executed by the `server` binary, which an integration test kills.
pub fn triple(x: u32) -> u32 {
    x * 3
}

/// Not executed by any test.
pub fn untested(x: u32) -> u32 {
    x + 7
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn double_three_is_six() {
        assert_eq!(double(3), 6);
    }
}
