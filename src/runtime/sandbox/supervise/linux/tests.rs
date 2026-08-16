use super::*;

#[test]
fn pidfd_signals_the_opened_process_identity() {
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pidfd = pidfd_open(child.id() as i32).expect("open child pidfd");
    pidfd_send_signal(&pidfd, libc::SIGTERM).expect("signal child through pidfd");
    let status = child.wait().expect("wait child");
    assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn pidfd_does_not_target_a_reused_pid_after_exit() {
    let mut child = Command::new("/bin/true").spawn().expect("spawn true");
    let pidfd = pidfd_open(child.id() as i32).expect("open child pidfd");
    child.wait().expect("reap child");
    let error = pidfd_send_signal(&pidfd, libc::SIGTERM)
        .expect_err("an exited process identity must reject signaling");
    assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn workload_child_identity_requires_the_same_single_child() {
    validate_workload_child_identity(41, &[41]).expect("identity should match");
    assert!(validate_workload_child_identity(41, &[]).is_err());
    assert!(validate_workload_child_identity(41, &[42]).is_err());
    assert!(validate_workload_child_identity(41, &[41, 42]).is_err());
}

#[test]
fn pidfd_preflight_error_names_the_rejected_syscall() {
    let message = pidfd_preflight_error("pidfd_send_signal", &std::io::Error::from_raw_os_error(1));
    assert!(message.contains("pidfd_send_signal"));
    assert!(message.contains("kernel or seccomp policy"));
}
