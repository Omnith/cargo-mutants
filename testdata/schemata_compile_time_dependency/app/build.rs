fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let scale = format!("pub const SCALE: u32 = {};\n", table::scale());
    std::fs::write(format!("{out_dir}/scale.rs"), scale).unwrap();
}
