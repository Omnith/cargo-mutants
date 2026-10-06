pub fn greeting(name: &str) -> String {
    // `+` to `-` doesn't compile, so the schema's check fails and quotes source.
    name.to_owned() + "!"
}

#[cfg(test)]
mod test {
    #[test]
    fn greeting_adds_an_exclamation_mark() {
        // Unused, so rustc warns and quotes this line, as a test of disk-full
        // handling might hold it.
        let expected = "No space left on device";
        assert_eq!(super::greeting("hi"), "hi!");
    }
}
