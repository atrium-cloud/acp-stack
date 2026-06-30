//! End-to-end isolation guarantee for the `unshare` sandbox backend: a process
//! spawned through the wrapper cannot read a masked path. This is the security
//! claim the `sandbox.rs` unit tests (argv construction) do not cover.
//!
//! Requires Linux and `CAP_SYS_ADMIN` (a privileged container). It probes for
//! that and skips cleanly when unavailable, so it is a no-op on macOS dev hosts
//! and unprivileged CI but a real assertion on a privileged Linux runner.

#![cfg(target_os = "linux")]

use std::process::Command;

/// True when this process can create the namespaces and mount a fresh `/proc`
/// the `unshare` backend relies on (i.e. it holds `CAP_SYS_ADMIN`).
fn unshare_usable() -> bool {
    Command::new("unshare")
        .args(["--mount", "--pid", "--fork", "--mount-proc", "--", "true"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn unshare_backend_masks_a_secret_from_the_workload() {
    if !unshare_usable() {
        eprintln!("skipping sandbox isolation test: unshare/CAP_SYS_ADMIN unavailable");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let secret_dir = tmp.path().join("secret-dir");
    std::fs::create_dir_all(&secret_dir).expect("create secret dir");
    let secret = secret_dir.join("age.key");
    std::fs::write(&secret, b"TOP-SECRET-KEY-MATERIAL").expect("write secret");

    // Sanity: readable outside the sandbox.
    assert_eq!(
        std::fs::read(&secret).expect("read secret outside"),
        b"TOP-SECRET-KEY-MATERIAL"
    );

    // Mirror the `unshare` wrap: unshare builds the namespaces + fresh /proc,
    // `acps __sandbox-exec` masks the dir with tmpfs (mount(2) under
    // CAP_SYS_ADMIN), then runs the workload — here `cat` of the now-masked file.
    let acps = env!("CARGO_BIN_EXE_acps");
    let output = Command::new("unshare")
        .args([
            "--mount",
            "--uts",
            "--ipc",
            "--pid",
            "--fork",
            "--mount-proc",
            "--propagation",
            "private",
            "--",
            acps,
            "__sandbox-exec",
            "--mask",
        ])
        .arg(&secret_dir)
        .arg("--")
        .arg("/bin/cat")
        .arg(&secret)
        .output()
        .expect("run sandboxed workload");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("TOP-SECRET-KEY-MATERIAL"),
        "masked secret leaked into the sandbox: stdout={stdout:?}"
    );
    assert!(
        !output.status.success(),
        "cat of a tmpfs-masked path should fail (ENOENT), got success"
    );

    // The host's view is untouched — masking happened only inside the namespace.
    assert_eq!(
        std::fs::read(&secret).expect("read secret outside after"),
        b"TOP-SECRET-KEY-MATERIAL"
    );
}
