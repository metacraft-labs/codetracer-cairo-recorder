use std::process::Command;

fn cargo_bin() -> Command {
    // Invoke the pre-built recorder binary directly via the
    // `CARGO_BIN_EXE_<name>` path Cargo exposes to integration tests.
    //
    // The previous `cargo run --quiet --` form spawned a *nested* `cargo`
    // inside the `cargo test` process.  The nested invocation contends for
    // the build lock on `target/` that the outer `cargo test` already
    // holds; under that contention `cargo run` can exit non-zero before it
    // ever launches the recorder, which surfaced as an intermittent
    // `--help should succeed` failure (the lock-contention window is a
    // race, so only whichever CLI test ran first was affected).  The
    // direct-binary form has no nested cargo and no lock contention.
    Command::new(env!("CARGO_BIN_EXE_codetracer-cairo-recorder"))
}

#[test]
fn test_help_flag() {
    let output = cargo_bin().arg("--help").output().expect("failed to run");
    assert!(output.status.success(), "--help should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("codetracer-cairo-recorder"),
        "help output should mention the program name"
    );
}

#[test]
fn test_version_subcommand() {
    let output = cargo_bin().arg("version").output().expect("failed to run");
    assert!(output.status.success(), "version should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.1.0"),
        "version output should contain the version number"
    );
}

#[test]
fn test_version_flag() {
    let output = cargo_bin()
        .arg("--version")
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "--version should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.1.0"),
        "version output should contain the version number"
    );
}

#[test]
fn test_record_nonexistent_file() {
    let output = cargo_bin()
        .args(["record", "nonexistent.cairo"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "record with nonexistent file should fail"
    );
}

/// M5: the `replay` subcommand, given a saved
/// `starknet_traceTransaction` JSON fixture via `--trace-file`,
/// produces a CTFS bundle in `--out-dir` without needing a live RPC
/// node.  This pins the offline-replay end-to-end CLI contract.
#[test]
fn test_replay_with_trace_file_writes_ct_bundle() {
    let bin = env!("CARGO_BIN_EXE_codetracer-cairo-recorder");

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp_dir.path().join("replay-traces");

    let trace_file = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-programs/starknet/mock_tx_trace.json");

    let output = Command::new(bin)
        .args([
            "replay",
            "--tx-hash",
            "0xdeadbeef",
            "--rpc-url",
            "http://unused.example.com",
            "--trace-file",
            trace_file.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run replay");

    assert!(
        output.status.success(),
        "replay should succeed with --trace-file fixture; stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Pin the .ct bundle existence: exactly one .ct file in out_dir.
    let ct_files: Vec<_> = std::fs::read_dir(&out_dir)
        .expect("read out_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "ct"))
        .collect();
    assert_eq!(ct_files.len(), 1);
}

#[test]
fn test_record_invalid_file() {
    // Create a temp file with non-Cairo data.
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let invalid_file = tmp_dir.path().join("invalid.cairo");
    std::fs::write(&invalid_file, b"this is not valid cairo code @#$%").expect("failed to write");

    let output = cargo_bin()
        .args(["record", invalid_file.to_str().unwrap()])
        .output()
        .expect("failed to run");

    assert!(
        !output.status.success(),
        "record with invalid Cairo file should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed") || stderr.contains("Error") || stderr.contains("error"),
        "error message should indicate failure, got: {}",
        stderr
    );
}
