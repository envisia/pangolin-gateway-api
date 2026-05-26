//! Smoke test for the binary entrypoint.
//!
//! Runs the compiled `pangolin-gateway-controller` binary with a deliberately
//! empty environment and checks it exits cleanly with the documented exit code
//! and a helpful error message — i.e. it doesn't panic, doesn't hang, and the
//! "invalid configuration" path is wired up end-to-end.

use std::process::Command;
use std::time::Duration;

/// Exit code the binary uses when `Config::from_env` fails.
const CONFIG_ERROR_EXIT: i32 = 2;

#[test]
fn binary_rejects_missing_config_endpoint() {
    let bin = env!("CARGO_BIN_EXE_pangolin-gateway-controller");

    let mut cmd = Command::new(bin);
    cmd.env_clear();
    // Keep PATH so the dynamic linker can find shared libs on macOS/Linux CI.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }

    let output =
        run_with_timeout(cmd, Duration::from_secs(10)).expect("binary did not exit within timeout");

    let code = output
        .status
        .code()
        .expect("binary terminated by signal, not exit");
    assert_eq!(
        code,
        CONFIG_ERROR_EXIT,
        "expected exit code {CONFIG_ERROR_EXIT}, got {code}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid configuration"),
        "stderr should mention invalid configuration, got: {stderr}"
    );
    assert!(
        stderr.contains("CONFIG_ENDPOINT"),
        "stderr should name the missing variable, got: {stderr}"
    );
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn controller binary");

    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(_status) = child.try_wait().expect("try_wait") {
            return Some(child.wait_with_output().expect("wait_with_output"));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
