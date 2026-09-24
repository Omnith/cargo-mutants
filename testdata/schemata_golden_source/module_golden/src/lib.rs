pub mod table;

pub fn double(x: u32) -> u32 {
    x * 2
}

pub fn triple(x: u32) -> u32 {
    x * 3
}

#[cfg(test)]
mod test {
    #[test]
    fn double() {
        assert_eq!(super::double(4), 8);
    }
}
