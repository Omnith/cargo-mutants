//! A function tested only in a child process: the test binary, run again.

pub fn double(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod test {
    use std::env;
    use std::process::Command;

    /// Check `double` in a child process that runs only this test, and inherits the
    /// environment.
    #[test]
    fn double_in_child_process() {
        if env::var_os("DOUBLE_IN_CHILD").is_some() {
            assert_eq!(super::double(21), 42);
            return;
        }
        let status = Command::new(env::current_exe().unwrap())
            .args(["--exact", "test::double_in_child_process"])
            .env("DOUBLE_IN_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
