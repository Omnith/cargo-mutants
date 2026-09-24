pub fn area(width: u32, height: u32) -> u32 {
    width * height
}

#[cfg(test)]
mod test {
    #[test]
    fn area() {
        assert_eq!(super::area(2, 3), 6);
    }
}
