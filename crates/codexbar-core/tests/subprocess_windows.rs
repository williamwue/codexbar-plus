#![cfg(windows)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use codexbar_core::subprocess::{run_blocking, SubprocessOptions};

#[test]
fn public_runner_executes_real_windows_binary_with_explicit_environment() {
    let system_root = std::env::var("SystemRoot").expect("Windows must define SystemRoot");
    let command_prompt = PathBuf::from(&system_root).join("System32").join("cmd.exe");
    let mut environment: HashMap<String, String> = std::env::vars().collect();
    environment.insert("CB_RUNNER_VALUE".into(), "stdout-ok".into());
    let arguments = [
        "/D".into(),
        "/S".into(),
        "/C".into(),
        "echo %CB_RUNNER_VALUE% & echo stderr-ok 1>&2".into(),
    ];

    let result = run_blocking(
        command_prompt,
        &arguments,
        SubprocessOptions::new(environment, Duration::from_secs(5)),
    )
    .expect("Windows command probe should complete");

    assert_eq!(result.stdout.trim(), "stdout-ok");
    assert_eq!(result.stderr.trim(), "stderr-ok");
    assert_eq!(result.exit_code, 0);
}
