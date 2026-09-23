/// Weakly tested: the test calls this but doesn't check the result, so all
/// mutants of it should be missed.
pub fn double(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod test {
    use std::env::current_dir;
    use std::path::Path;

    #[test]
    fn double_runs() {
        let _ = super::double(3);
    }

    /// `cargo test` runs tests with the package root as the working directory,
    /// so this fails if `c` was compiled in a different build dir.
    #[test]
    fn c_was_compiled_in_this_build_dir() {
        let c_dir = current_dir().unwrap().join("../c").canonicalize().unwrap();
        assert_eq!(Path::new(c::MANIFEST_DIR).canonicalize().unwrap(), c_dir);
    }
}
