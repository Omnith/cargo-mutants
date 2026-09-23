pub fn add(a: u32, b: u32) -> u32 { a + b }

#[cfg(test)]
mod test {
    #[test]
    fn add() {
        assert_eq!(super::add(2, 3), 5);
    }

    #[test]
    fn source_is_unchanged() {
        let source = std::fs::read_to_string(file!()).unwrap();
        assert_eq!(
            source.lines().next().unwrap(),
            "pub fn add(a: u32, b: u32) -> u32 { a + b }"
        );
    }
}
