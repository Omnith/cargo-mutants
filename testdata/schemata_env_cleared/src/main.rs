//! Prints a greeting for its argument.

fn greeting(name: &str) -> String {
    format!("Hello, {name}!")
}

fn main() {
    let name = std::env::args().nth(1).unwrap_or_default();
    println!("{}", greeting(&name));
}
