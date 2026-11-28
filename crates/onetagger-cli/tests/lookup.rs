use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn lookup_rejects_file_arguments_and_creates_no_user_profile_on_invalid_input() {
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_onetagger-cli"))
        .args(["lookup", "--json"])
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("XDG_DATA_HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path())
        .env("APPDATA", root.path())
        .env("LOCALAPPDATA", root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"artist":"Test","title":"A track","path":"/original.mp3"}"#)
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Expected a JSON object"));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
