pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[cfg(test)]
mod test {
    #[test]
    fn add_sums() {
        assert_eq!(super::add(2, 3), 5);
    }
}
