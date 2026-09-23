use std::process::Command;

#[test]
fn shout_binary_shouts_its_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_shout"))
        .args(["hello", "world"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "HELLO WORLD!\n");
}
