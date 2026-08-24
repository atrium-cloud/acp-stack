//! End-to-end isolation guarantee for the `unshare` sandbox backend: a process spawned through the wrapper cannot read a masked path. Requires Linux and `CAP_SYS_ADMIN`.

#![cfg(target_os = "linux")]

use std::process::Command;

/// True when this process holds the `CAP_SYS_ADMIN` the `unshare` backend relies on.
fn unshare_usable() -> bool {
    Command::new("unshare")
        .args(["--mount", "--pid", "--fork", "--mount-proc", "--", "true"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
#[ignore = "requires privileged Linux sandbox capabilities"]
fn unshare_backend_masks_a_secret_from_the_workload() {
    assert!(
        unshare_usable(),
        "required sandbox test capability missing: unshare/CAP_SYS_ADMIN unavailable"
    );

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

    // Mirrors the `unshare` wrap: namespaces + fresh /proc, tmpfs masking, then the workload.
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

    // The host's view is untouched: masking happened only inside the namespace.
    assert_eq!(
        std::fs::read(&secret).expect("read secret outside after"),
        b"TOP-SECRET-KEY-MATERIAL"
    );
}
