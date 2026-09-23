pub const fn double(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod test {
    #[test]
    fn double_is_wrong() {
        assert_eq!(super::double(2), 5, "this test fails in the unmutated tree");
    }
}
