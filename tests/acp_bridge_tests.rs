//! Drives `AcpBridge::spawn` against the fake-agent gate baked into the
//! `acps` binary. The fake responds to `initialize` with hardcoded
//! capabilities so we can verify the spawn + handshake path end-to-end
//! without depending on a third-party agent.

use std::collections::HashMap;

use acp_stack::acp_bridge::AcpBridge;
use acp_stack::config::{AgentConfig, AgentInstallConfig};

fn fake_agent_config() -> AgentConfig {
    AgentConfig {
        id: "fake".into(),
        name: "fake".into(),
        command: env!("CARGO_BIN_EXE_acps").into(),
        args: vec!["__acps-test-fake-agent".into()],
        cwd: None,
        env: vec![],
        expected_sha256: None,
        restart: "never".into(),
        install: Some(AgentInstallConfig {
            install_type: "shell".into(),
            shell: "true".into(),
            creates: "true".into(),
        }),
    }
}

fn fake_env() -> HashMap<String, String> {
    HashMap::new()
}

#[tokio::test]
async fn spawn_completes_initialize_and_captures_capabilities() {
    let bridge = AcpBridge::spawn(&fake_agent_config(), fake_env(), std::env::temp_dir())
        .await
        .expect("bridge spawns");
    let caps = bridge.capabilities();
    assert_eq!(caps.protocol_version, 1);
    assert_eq!(caps.agent_name.as_deref(), Some("acps-fake-agent"));
    bridge.shutdown().await.expect("shutdown ok");
}

#[tokio::test]
async fn shutdown_terminates_the_child() {
    let bridge = AcpBridge::spawn(&fake_agent_config(), fake_env(), std::env::temp_dir())
        .await
        .expect("spawn ok");
    let pid = bridge.pid().expect("pid available");
    bridge.shutdown().await.expect("shutdown ok");

    // After shutdown, the child should be gone. We can't query directly via
    // a portable API, but the OS will refuse `kill(pid, 0)` for a dead pid.
    #[cfg(unix)]
    {
        // SAFETY: kill with signal 0 is the standard "does this pid exist"
        // probe; never delivers a signal.
        unsafe {
            let alive = libc::kill(pid as i32, 0);
            if alive == 0 {
                // The PID may have been reused; give the kernel a beat then
                // recheck. If still alive, we've leaked.
                std::thread::sleep(std::time::Duration::from_millis(50));
                let still_alive = libc::kill(pid as i32, 0);
                assert_ne!(
                    still_alive, 0,
                    "fake agent pid {pid} appears to still be running after shutdown"
                );
            }
        }
    }
}

#[tokio::test]
async fn spawn_does_not_forward_unlisted_parent_environment() {
    let mut config = fake_agent_config();
    config
        .args
        .extend(["--assert-env-absent".into(), "PATH".into()]);

    let bridge = AcpBridge::spawn(&config, fake_env(), std::env::temp_dir())
        .await
        .expect("bridge spawns");
    let caps = bridge.capabilities();
    assert_eq!(caps.agent_title.as_deref(), Some("env absent"));
    bridge.shutdown().await.expect("shutdown ok");
}
