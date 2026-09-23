//! Trait default methods and impls in a module file, which gets its own schema.
//!
//! The schema adds parentheses, so this checks that its lint allowance overrides
//! a module-level deny. (Classic mutants don't add parentheses, so this doesn't
//! make them unviable.)

#![deny(unused_parens, unused_braces)]

pub trait Area {
    fn width(&self) -> u32;
    fn height(&self) -> u32;

    fn area(&self) -> u32 {
        self.width() * self.height()
    }

    fn is_square(&self) -> bool {
        self.width() == self.height()
    }
}

pub struct Rect(pub u32, pub u32);

impl Area for Rect {
    fn width(&self) -> u32 {
        self.0
    }

    fn height(&self) -> u32 {
        self.1
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn rect_area() {
        assert_eq!(Rect(2, 3).area(), 6);
        assert!(Rect(2, 2).is_square());
    }
}
