#![cfg(feature = "test-fixtures")]

use assert_cmd::Command;
use predicates::prelude::*;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command as StdCommand, Stdio},
    sync::mpsc,
    time::Duration,
};

#[test]
fn placebo_agent_help_lists_acp_subcommand() {
    Command::cargo_bin("placebo-agent")
        .expect("binary should build")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("placebo-agent"))
        .stdout(predicate::str::contains("acp"));
}

#[test]
fn placebo_agent_acp_help_lists_cwd() {
    Command::cargo_bin("placebo-agent")
        .expect("binary should build")
        .args(["acp", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--cwd"))
        .stdout(predicate::str::contains("--prompt-error"));
}

#[test]
fn placebo_agent_acp_initializes_without_api_key() {
    let mut child = StdCommand::new(env!("CARGO_BIN_EXE_placebo-agent"))
        .env_remove("OPENCODE_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("placebo-agent should spawn");

    let mut stdin = child.stdin.take().expect("stdin should be piped");
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":1}}\n",
        )
        .expect("initialize request should write");
    stdin.flush().expect("initialize request should flush");

    let stdout = child.stdout.take().expect("stdout should be piped");
    let (sender, receiver) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        if sender.send(result).is_err() {
            eprintln!("initialize response receiver dropped before stdout read finished");
        }
    });

    let line = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("initialize response should arrive before timeout")
        .expect("initialize response should be readable");

    if child
        .try_wait()
        .expect("child status should be readable")
        .is_none()
    {
        child.kill().expect("child should terminate");
        child.wait().expect("child should be reaped");
    }
    reader_thread.join().expect("stdout reader should finish");

    assert!(line.contains(r#""name":"placebo-agent""#), "{line}");
    assert!(line.contains(r#""authMethods":[]"#), "{line}");
}
