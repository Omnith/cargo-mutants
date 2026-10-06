pub fn double(x: u32) -> u32 {
    x * 2
}

pub fn triple(x: u32) -> u32 {
    x * 3
}

pub fn square(x: u32) -> u32 {
    x * x
}

pub fn larger(a: u32, b: u32) -> u32 {
    if a > b { a } else { b }
}

pub fn at_most_ten(x: u32) -> u32 {
    if x > 10 { 10 } else { x }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn double_three_is_six() {
        assert_eq!(double(3), 6);
    }

    #[test]
    fn triple_three_is_nine() {
        assert_eq!(triple(3), 9);
    }

    #[test]
    fn square_three_is_nine() {
        assert_eq!(square(3), 9);
    }

    #[test]
    fn larger_picks_either_argument() {
        assert_eq!(larger(3, 5), 5);
        assert_eq!(larger(5, 3), 5);
    }

    #[test]
    fn at_most_ten_clamps_above_and_keeps_below() {
        assert_eq!(at_most_ten(12), 10);
        assert_eq!(at_most_ten(4), 4);
    }
}
