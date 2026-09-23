mod shapes;

pub use shapes::{area, perimeter};

pub fn double(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod test {
    #[test]
    fn perimeter() {
        assert_eq!(super::perimeter(2, 3), 10);
    }

    #[test]
    fn area_formula_is_in_source() {
        assert!(include_str!("shapes.rs").contains("width * height"));
    }
}
