use std::process::Command;

#[test]
fn unknown_subcommand_is_a_clear_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("unknown")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown subcommand"), "stderr: {stderr}");
    assert!(stderr.contains("daemon"), "stderr: {stderr}");
}

#[test]
fn missing_subcommand_prints_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("usage: lantern daemon"), "stderr: {stderr}");
}
