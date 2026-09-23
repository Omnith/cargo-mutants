include!(concat!(env!("OUT_DIR"), "/scale.rs"));

pub fn scaled(x: u32) -> u32 {
    x * SCALE
}

pub fn greeting() -> &'static str {
    macros::greeting!()
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn scaled_multiplies_by_three() {
        assert_eq!(scaled(2), 6);
    }

    #[test]
    fn greeting_is_hello() {
        assert_eq!(greeting(), "hello");
    }
}
