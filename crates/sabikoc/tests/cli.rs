//! End-to-end checks of the `sabikoc` binary.

use std::process::Command;

fn sabikoc() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sabikoc"))
}

fn write_temp(name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sabikoc-cli-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

#[test]
fn version() {
    let out = sabikoc().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("sabikoc "));
}

#[test]
fn emits_ast_for_valid_file() {
    let path = write_temp("ok.sabi", "fn Main() { let x = 1 + 2; }\n");
    let out = sabikoc().arg(&path).arg("--emit=ast").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Fn Main"), "{stdout}");
    assert!(stdout.contains("Binary +"), "{stdout}");
}

#[test]
fn emits_mir_for_valid_file() {
    let path = write_temp("mir.sabi", "fn Add(a: i32, b: i32) -> i32 { a + b }\n");
    let out = sabikoc().arg(&path).arg("--emit=mir").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("fn Add(_1: i32, _2: i32) -> i32 {"),
        "{stdout}"
    );
    assert!(stdout.contains("_0 = Add(copy _3, copy _4);"), "{stdout}");
    assert!(stdout.contains("return;"), "{stdout}");
}

#[test]
fn reports_errors_with_exit_code_one() {
    let path = write_temp("bad.sabi", "fn Main() { let x = 1 }\n");
    let out = sabikoc().arg(&path).arg("--color=never").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("E0100"), "{stderr}");
    assert!(stderr.contains("expected `;`"), "{stderr}");
    assert!(
        !stderr.contains("\u{1b}["),
        "no ANSI with --color=never: {stderr}"
    );
}

#[test]
fn json_error_format() {
    let path = write_temp("bad2.sabi", "fn Main() { a < b < c; }\n");
    let out = sabikoc()
        .arg(&path)
        .arg("--error-format=json")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first = stderr.lines().next().unwrap();
    let v: serde_json::Value = serde_json::from_str(first).expect("valid json");
    assert_eq!(v["code"], "E0104");
}

#[test]
fn missing_file_is_exit_code_two() {
    let out = sabikoc().arg("/nonexistent/file.sabi").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}
