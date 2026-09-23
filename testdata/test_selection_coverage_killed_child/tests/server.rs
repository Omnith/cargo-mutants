use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

#[test]
fn server_prints_six_then_is_killed() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    // The server has already exited if it was mutated not to run.
    let _ = child.kill();
    child.wait().unwrap();
    assert_eq!(line.trim(), "6");
}
