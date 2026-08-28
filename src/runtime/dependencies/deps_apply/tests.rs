use super::*;
use crate::config::{DependenciesConfig, DependencyEntry, DependencyInstallAction};

fn config_with_dep(entry: DependencyEntry) -> Config {
    let toml_text = include_str!("../../../../tests/fixtures/valid-opencode-stack.toml");
    let mut config = crate::config::load_config_from_str(toml_text).expect("config");
    config.dependencies = DependenciesConfig {
        commands: vec![entry],
        ..Default::default()
    };
    config
}

#[test]
fn candidates_filter_to_install_blocks_only() {
    let mut config = config_with_dep(DependencyEntry {
        name: "with-install".into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: "true".into(),
            creates: Some("true".into()),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    });
    config.dependencies.commands.push(DependencyEntry {
        name: "no-install".into(),
        required: true,
        feature: None,
        install: None,
    });
    let candidates = candidates_for(&config, None);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].name, "with-install");
}

#[test]
fn candidates_honor_feature_filter() {
    let mut config = config_with_dep(DependencyEntry {
        name: "cloudflared".into(),
        required: true,
        feature: Some("cloudflare-tunnel".into()),
        install: Some(DependencyInstallAction {
            shell: "true".into(),
            creates: Some("true".into()),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    });
    config.dependencies.commands.push(DependencyEntry {
        name: "rg".into(),
        required: true,
        feature: Some("search".into()),
        install: Some(DependencyInstallAction {
            shell: "true".into(),
            creates: Some("true".into()),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    });
    let only_cf = candidates_for(&config, Some("cloudflare-tunnel"));
    assert_eq!(only_cf.len(), 1);
    assert_eq!(only_cf[0].name, "cloudflared");
    let none = candidates_for(&config, Some("nothing-matches"));
    assert!(none.is_empty());
}

#[test]
fn apply_skips_when_creates_already_resolves() {
    let config = config_with_dep(DependencyEntry {
        name: "sh".into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: "exit 1".into(),
            creates: Some("sh".into()),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    });
    let report = apply_dependencies(&config, None, None, "/bin/sh", Path::new("/")).expect("apply");
    assert_eq!(report.results.len(), 1);
    assert!(
        matches!(report.results[0].outcome, DepApplyOutcome::AlreadyPresent),
        "expected AlreadyPresent shortcut; got {:?}",
        report.results[0].outcome,
    );
}

#[test]
fn apply_runs_shell_and_verifies_creates_postcheck() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let bin = tempdir.path().join("apply-test-marker");
    let bin_str = bin.to_string_lossy().into_owned();
    let config = config_with_dep(DependencyEntry {
        name: "apply-test-marker".into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
            creates: Some(bin_str.clone()),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    });
    let report = apply_dependencies(&config, None, None, "/bin/sh", tempdir.path()).expect("apply");
    assert_eq!(report.results.len(), 1);
    assert!(
        matches!(report.results[0].outcome, DepApplyOutcome::Installed),
        "expected Installed; got {:?}",
        report.results[0].outcome,
    );
    assert!(bin.is_file(), "shell should have created the sentinel");
}

#[test]
fn apply_marks_failed_when_shell_exits_nonzero() {
    let config = config_with_dep(DependencyEntry {
        name: "definitely-not-installed-acps-apply-fail".into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: "echo nope >&2; exit 7".into(),
            creates: Some("definitely-not-installed-acps-apply-fail".into()),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    });
    let report = apply_dependencies(&config, None, None, "/bin/sh", Path::new("/")).expect("apply");
    match &report.results[0].outcome {
        DepApplyOutcome::Failed {
            exit_code,
            stderr_tail,
        } => {
            assert_eq!(*exit_code, Some(7));
            assert!(
                stderr_tail.contains("nope"),
                "stderr tail missing captured stderr: {stderr_tail:?}",
            );
        }
        other => panic!("expected Failed; got {other:?}"),
    }
}

fn system_dep(name: &str, shell: &str, creates: &str) -> DependencyEntry {
    DependencyEntry {
        name: name.into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: shell.into(),
            creates: Some(creates.into()),
            scope: DependencyInstallScope::System,
            timeout_secs: None,
        }),
    }
}

#[test]
fn escalation_unavailable_still_refuses_system_scope() {
    let config = config_with_dep(system_dep(
        "definitely-not-installed-acps-priv-check",
        "echo SHOULD NOT EXECUTE >&2; exit 99",
        "definitely-not-installed-acps-priv-check",
    ));
    let report = apply_dependencies_with_escalation(
        &config,
        None,
        None,
        "/bin/sh",
        &PrivilegeEscalation::Unavailable { uid: 1001 },
        Path::new("/"),
        None,
        |_, _, _| Ok(()),
    )
    .expect("apply");
    assert!(
        matches!(
            report.results[0].outcome,
            DepApplyOutcome::PrivilegeRequired { uid: 1001 }
        ),
        "unavailable escalation must short-circuit to PrivilegeRequired; got {:?}",
        report.results[0].outcome,
    );
}

#[test]
fn outcome_kinds_serialize_as_snake_case() {
    // `kind` is a wire value read by API clients and mirrored by hand in
    // `crate::cli::deps`.
    let kind_of = |outcome: &DepApplyOutcome| {
        serde_json::to_value(outcome).expect("serialize outcome")["kind"]
            .as_str()
            .expect("kind string")
            .to_owned()
    };
    assert_eq!(kind_of(&DepApplyOutcome::Installed), "installed");
    assert_eq!(kind_of(&DepApplyOutcome::AlreadyPresent), "already_present");
    assert_eq!(
        kind_of(&DepApplyOutcome::PrivilegeRequired { uid: 1001 }),
        "privilege_required"
    );
    assert_eq!(
        kind_of(&DepApplyOutcome::Failed {
            exit_code: Some(1),
            stderr_tail: String::new(),
        }),
        "failed"
    );
}

#[test]
fn not_needed_escalation_is_revalidated_against_euid_at_apply_time() {
    // `NotNeeded` also means "no probe ran", so apply_one must re-derive from
    // the live euid rather than run a root-intended script unprivileged.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let bin = tempdir.path().join("system-direct-marker");
    let bin_str = bin.to_string_lossy().into_owned();
    let config = config_with_dep(system_dep(
        "system-direct-marker",
        &format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
        &bin_str,
    ));
    let report = apply_dependencies_with_escalation(
        &config,
        None,
        None,
        "/bin/sh",
        &PrivilegeEscalation::NotNeeded,
        tempdir.path(),
        None,
        |_, _, _| Ok(()),
    )
    .expect("apply");
    if current_uid() == 0 {
        assert!(
            matches!(report.results[0].outcome, DepApplyOutcome::Installed),
            "root must run system scope directly; got {:?}",
            report.results[0].outcome,
        );
    } else {
        assert!(
            matches!(
                report.results[0].outcome,
                DepApplyOutcome::PrivilegeRequired { .. }
            ),
            "stale NotNeeded must not run system scope unprivileged; got {:?}",
            report.results[0].outcome,
        );
        assert!(!bin.exists(), "script must not have executed");
    }
}

/// Fake `sudo` that records its argv and then execs the remaining command.
fn write_fake_sudo(dir: &Path, argv_log: &Path) -> PathBuf {
    let path = dir.join("sudo");
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> {log}; done\nshift\nexec \"$@\"\n",
        log = argv_log.to_string_lossy(),
    );
    std::fs::write(&path, script).expect("write fake sudo");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake sudo");
    }
    path
}

#[test]
fn sudo_escalation_wraps_shell_invocation() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let argv_log = tempdir.path().join("sudo-argv.log");
    let fake_sudo = write_fake_sudo(tempdir.path(), &argv_log);
    let bin = tempdir.path().join("sudo-escalated-marker");
    let bin_str = bin.to_string_lossy().into_owned();
    let script = format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}");
    let config = config_with_dep(system_dep("sudo-escalated-marker", &script, &bin_str));
    let report = apply_dependencies_with_escalation(
        &config,
        None,
        None,
        "/bin/sh",
        &PrivilegeEscalation::Sudo {
            sudo_path: fake_sudo,
            uid: 1001,
        },
        tempdir.path(),
        None,
        |_, _, _| Ok(()),
    )
    .expect("apply");
    assert!(
        matches!(report.results[0].outcome, DepApplyOutcome::Installed),
        "expected Installed through fake sudo; got {:?}",
        report.results[0].outcome,
    );
    let argv = std::fs::read_to_string(&argv_log).expect("argv log");
    let lines: Vec<&str> = argv.lines().collect();
    assert_eq!(&lines[..3], &[SUDO_NON_INTERACTIVE_FLAG, "/bin/sh", "-c"]);
    let escalated = lines[3..].join("\n");
    assert!(
        escalated.ends_with(&script),
        "operator script must be verbatim and last: {escalated:?}",
    );
    assert!(
        escalated.contains("export DEBIAN_FRONTEND=noninteractive"),
        "non-interactive env must be re-exported inside the escalated shell: {escalated:?}",
    );
}

#[test]
fn escalated_run_records_sudo_marker_in_stdout() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let argv_log = tempdir.path().join("sudo-argv.log");
    let fake_sudo = write_fake_sudo(tempdir.path(), &argv_log);
    let bin = tempdir.path().join("sudo-marker-audit");
    let bin_str = bin.to_string_lossy().into_owned();
    let config = config_with_dep(system_dep(
        "sudo-marker-audit",
        &format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
        &bin_str,
    ));
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    apply_dependencies_with_escalation(
        &config,
        None,
        Some(&store),
        "/bin/sh",
        &PrivilegeEscalation::Sudo {
            sudo_path: fake_sudo,
            uid: 1001,
        },
        tempdir.path(),
        None,
        |_, _, _| Ok(()),
    )
    .expect("apply");
    let rows = store
        .query_installer_runs_filtered(Some(DEPS_APPLY_AGENT_ID), 10)
        .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "installed");
    assert_eq!(rows[0].method.as_deref(), Some("shell"));
    assert!(
        rows[0].stdout.starts_with(ESCALATED_STDOUT_MARKER),
        "persisted stdout must lead with the escalation marker: {:?}",
        rows[0].stdout,
    );
}

#[test]
fn escalated_script_reexports_non_interactive_env() {
    let script = escalated_script("apt-get install -y jq");
    for (name, value) in NON_INTERACTIVE_ENV {
        assert!(
            script.contains(&format!("export {name}={value}")),
            "missing export for {name}: {script:?}",
        );
    }
    assert!(
        script.ends_with("apt-get install -y jq"),
        "operator script must be appended verbatim and last: {script:?}",
    );
}

#[test]
fn manual_privileged_command_quotes_embedded_single_quotes() {
    let candidate = DepApplyCandidate {
        name: "quoted".into(),
        scope: DependencyInstallScope::System,
        shell: "echo 'hi'".into(),
        creates: "quoted".into(),
    };
    assert_eq!(
        manual_privileged_command("/bin/sh", &candidate),
        r"sudo /bin/sh -c 'echo '\''hi'\'''",
    );
}

/// Write an executable script that exits with `code`.
fn write_exit_stub(dir: &Path, name: &str, code: i32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\nexit {code}\n")).expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
    }
    path
}

#[test]
fn probe_collapses_missing_and_denied_sudo_to_unavailable() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        probe_privilege_escalation_with(1001, None, tempdir.path()),
        PrivilegeEscalation::Unavailable { uid: 1001 },
    );
    let denied_sudo = write_exit_stub(tempdir.path(), "sudo-denied", 1);
    assert_eq!(
        probe_privilege_escalation_with(1001, Some(denied_sudo), tempdir.path()),
        PrivilegeEscalation::Unavailable { uid: 1001 },
    );
    // The probe collapses transient spawn errors (fork EAGAIN under a parallel
    // suite) to Unavailable, so retry the granted case before failing.
    let granted_sudo = write_exit_stub(tempdir.path(), "sudo-granted", 0);
    let mut granted =
        probe_privilege_escalation_with(1001, Some(granted_sudo.clone()), tempdir.path());
    for _ in 0..2 {
        if matches!(granted, PrivilegeEscalation::Sudo { .. }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        granted = probe_privilege_escalation_with(1001, Some(granted_sudo.clone()), tempdir.path());
    }
    assert_eq!(
        granted,
        PrivilegeEscalation::Sudo {
            sudo_path: granted_sudo,
            uid: 1001,
        },
    );
    assert_eq!(
        probe_privilege_escalation_with(0, None, tempdir.path()),
        PrivilegeEscalation::NotNeeded,
    );
}

#[test]
fn escalation_notice_lines_cover_all_modes() {
    let candidate = DepApplyCandidate {
        name: "acpstack-system-dep".into(),
        scope: DependencyInstallScope::System,
        shell: "apt-get install -y jq".into(),
        creates: "jq".into(),
    };
    let candidates = vec![candidate];
    assert!(
        escalation_notice_lines(&PrivilegeEscalation::NotNeeded, "/bin/sh", &[]).is_empty(),
        "no system candidates must yield no notice",
    );
    let root = escalation_notice_lines(&PrivilegeEscalation::NotNeeded, "/bin/sh", &candidates);
    assert_eq!(root.len(), 1);
    assert!(root[0].contains("run them directly"), "{root:?}");
    let sudo = escalation_notice_lines(
        &PrivilegeEscalation::Sudo {
            sudo_path: PathBuf::from("/usr/bin/sudo"),
            uid: 1001,
        },
        "/bin/sh",
        &candidates,
    );
    assert_eq!(sudo.len(), 1);
    assert!(sudo[0].contains("`sudo -n`"), "{sudo:?}");
    let unavailable = escalation_notice_lines(
        &PrivilegeEscalation::Unavailable { uid: 1001 },
        "/bin/sh",
        &candidates,
    );
    assert!(
        unavailable[0].contains("skipped and recorded as privilege_required"),
        "{unavailable:?}",
    );
    assert!(
        unavailable
            .iter()
            .any(|line| line.contains("sudo /bin/sh -c 'apt-get install -y jq'")),
        "manual command must be listed per candidate: {unavailable:?}",
    );
}

#[test]
fn pending_system_candidates_filters_scope_and_presence() {
    let mut config = config_with_dep(system_dep(
        "definitely-not-installed-acps-system-pending",
        "true",
        "definitely-not-installed-acps-system-pending",
    ));
    config
        .dependencies
        .commands
        .push(system_dep("sh-present", "true", "sh"));
    config.dependencies.commands.push(DependencyEntry {
        name: "definitely-not-installed-acps-user-pending".into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: "true".into(),
            creates: Some("definitely-not-installed-acps-user-pending".into()),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    });
    let pending = pending_system_candidates(&config, None);
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].name,
        "definitely-not-installed-acps-system-pending"
    );
}

#[test]
fn reap_with_grace_bounds_wait_and_reaps_exited_children() {
    let mut exited = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .expect("spawn");
    let status = reap_with_grace(&mut exited, Duration::from_secs(5));
    assert!(status.is_some(), "exited child must reap within grace");

    let mut running = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 30")
        .spawn()
        .expect("spawn");
    let started = Instant::now();
    let status = reap_with_grace(&mut running, Duration::from_millis(200));
    assert!(status.is_none(), "running child must return None");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "grace must bound the wait",
    );
    let _ = running.kill();
    let _ = running.wait();
}

#[test]
fn before_after_status_honors_absolute_creates_path() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let bin = tempdir.path().join("apply-before-after");
    let bin_str = bin.to_string_lossy().into_owned();
    let config = config_with_dep(DependencyEntry {
        name: "apply-before-after".into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
            creates: Some(bin_str.clone()),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    });
    let report = apply_dependencies(&config, None, None, "/bin/sh", tempdir.path()).expect("apply");
    let after_entry = report
        .after
        .iter()
        .find(|s| s.name == "apply-before-after")
        .expect("after row");
    assert!(
        after_entry.available,
        "report.after must honor absolute creates path; got {after_entry:?}",
    );
}

#[test]
fn timeout_kills_entire_process_group() {
    // Killing only the shell child would leave grandchildren holding the pipes
    // open, hanging the join threads past the declared timeout.
    let config = config_with_dep(DependencyEntry {
        name: "definitely-not-installed-timeout-check".into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: "sleep 60 & sleep 60".into(),
            creates: Some("definitely-not-installed-timeout-check".into()),
            scope: DependencyInstallScope::User,
            timeout_secs: Some(1),
        }),
    });
    let started = std::time::Instant::now();
    let report = apply_dependencies(&config, None, None, "/bin/sh", Path::new("/")).expect("apply");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "1s timeout must kill the whole group; took {elapsed:?}",
    );
    match &report.results[0].outcome {
        DepApplyOutcome::Failed { exit_code, .. } => {
            assert!(
                exit_code.is_none(),
                "timed-out runs report None exit_code, got {exit_code:?}",
            );
        }
        other => panic!("expected Failed on timeout; got {other:?}"),
    }
}

#[test]
fn stderr_tail_captures_actual_tail_when_stream_blows_past_cap() {
    let marker = "FINAL_DIAGNOSTIC_AT_THE_END_aaa";
    // ~80 KiB of stderr noise fills the reader's 64 KiB prefix before the
    // marker arrives, so the marker survives only via the rolling tail.
    let shell = format!(
        "yes 'noise line that is long enough to push past 64 KiB quickly' | head -n 1500 1>&2; \
         printf %s {marker} 1>&2; exit 1"
    );
    let config = config_with_dep(DependencyEntry {
        name: "definitely-not-installed-tail-check".into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell,
            creates: Some("definitely-not-installed-tail-check".into()),
            scope: DependencyInstallScope::User,
            timeout_secs: Some(30),
        }),
    });
    let report = apply_dependencies(&config, None, None, "/bin/sh", Path::new("/")).expect("apply");
    match &report.results[0].outcome {
        DepApplyOutcome::Failed { stderr_tail, .. } => {
            assert!(
                stderr_tail.contains(marker),
                "stderr_tail must contain the final diagnostic; got {stderr_tail:?}",
            );
        }
        other => panic!("expected Failed; got {other:?}"),
    }
}

#[test]
fn finish_failure_does_not_abort_apply() {
    // Hold a write lock past the apply store's busy timeout so both the finish
    // and the error mark hit SQLITE_BUSY.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let path = tempdir.path().join("state.sqlite");
    let store = StateStore::open(&path).expect("open");
    store.migrate().expect("migrate");
    store
        .set_busy_timeout_for_test(Duration::from_millis(100))
        .expect("lower busy timeout");
    let bin = tempdir.path().join("finish-blocked-marker");
    let bin_str = bin.to_string_lossy().into_owned();
    let config = config_with_dep(DependencyEntry {
        name: "finish-blocked-marker".into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            // The sleep leaves a window to take the write lock after the
            // running row lands but before the step finishes.
            shell: format!(
                "sleep 0.5; printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"
            ),
            creates: Some(bin_str),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    });

    let home = tempdir.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        apply_dependencies(&config, None, Some(&store), "/bin/sh", &home)
    });

    let reader = StateStore::open(&path).expect("reader connection");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if !reader
            .query_active_installer_runs(None)
            .expect("active query")
            .is_empty()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "running row never appeared while the dep shell was blocked"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let blocker = rusqlite::Connection::open(&path).expect("blocker connection");
    blocker
        .busy_timeout(Duration::from_secs(5))
        .expect("blocker busy timeout");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("blocker takes write lock");
    std::thread::sleep(Duration::from_millis(1200));
    blocker.execute_batch("COMMIT").expect("release write lock");

    let report = worker
        .join()
        .expect("worker join")
        .expect("finalize failure must not abort the apply");
    assert_eq!(report.results.len(), 1);
    assert!(
        matches!(report.results[0].outcome, DepApplyOutcome::Installed),
        "the step itself succeeded; got {:?}",
        report.results[0].outcome,
    );
    // One row total: the running row is never duplicated by a fallback append.
    let runs = reader
        .query_installer_runs_filtered(None, 10)
        .expect("history");
    assert_eq!(runs.len(), 1);
}

fn user_dep_entry(name: &str, shell: &str, creates: &str) -> DependencyEntry {
    DependencyEntry {
        name: name.into(),
        required: true,
        feature: None,
        install: Some(DependencyInstallAction {
            shell: shell.into(),
            creates: Some(creates.into()),
            scope: DependencyInstallScope::User,
            timeout_secs: None,
        }),
    }
}

fn open_tracked_store(dir: &Path) -> StateStore {
    let store = StateStore::open(dir.join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    store
}

#[test]
fn tracked_apply_settles_succeeded_with_matching_run_and_action_ids() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let bin = tempdir.path().join("tracked-ok-marker");
    let bin_str = bin.to_string_lossy().into_owned();
    let config = config_with_dep(user_dep_entry(
        "tracked-ok-marker",
        &format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
        &bin_str,
    ));
    let store = open_tracked_store(tempdir.path());
    let report = apply_dependencies_tracked(
        &config,
        &store,
        TrackedApplyRun::Claim {
            origin: crate::state::DEPS_APPLY_ORIGIN_CLI,
            init_run_id: None,
        },
        None,
        "/bin/sh",
        &PrivilegeEscalation::NotNeeded,
        tempdir.path(),
        |_, _, _| Ok(()),
    )
    .expect("tracked apply");

    let run = store
        .lookup_deps_apply_run(&report.apply_run_id)
        .expect("lookup")
        .expect("run row must exist");
    assert_eq!(run.status, crate::state::DEPS_APPLY_RUN_SUCCEEDED);
    assert_eq!(run.installed, 1);
    assert_eq!(run.completed, 1);
    assert!(run.finished_at.is_some());
    let actions = store
        .query_installer_runs_for_apply_run(DEPS_APPLY_AGENT_ID, DEPS_APPLY_STEP, &run.id)
        .expect("actions");
    assert_eq!(actions.len(), 1);
}

#[test]
fn tracked_apply_settles_failed_and_privilege_blocked() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = open_tracked_store(tempdir.path());

    let failing = config_with_dep(user_dep_entry(
        "tracked-fail-marker",
        "exit 7",
        "acps-tracked-never-resolves",
    ));
    let report = apply_dependencies_tracked(
        &failing,
        &store,
        TrackedApplyRun::Claim {
            origin: crate::state::DEPS_APPLY_ORIGIN_CLI,
            init_run_id: None,
        },
        None,
        "/bin/sh",
        &PrivilegeEscalation::NotNeeded,
        tempdir.path(),
        |_, _, _| Ok(()),
    )
    .expect("apply itself returns a report");
    let failed_run = store
        .lookup_deps_apply_run(&report.apply_run_id)
        .expect("lookup")
        .expect("run row");
    assert_eq!(failed_run.status, crate::state::DEPS_APPLY_RUN_FAILED);
    assert_eq!(failed_run.failed, 1);
    assert_eq!(failed_run.error_code.as_deref(), Some("deps.apply_failed"));

    let blocked = config_with_dep(system_dep(
        "tracked-priv-marker",
        "exit 0",
        "acps-tracked-priv-never-resolves",
    ));
    let report = apply_dependencies_tracked(
        &blocked,
        &store,
        TrackedApplyRun::Claim {
            origin: crate::state::DEPS_APPLY_ORIGIN_CLI,
            init_run_id: None,
        },
        None,
        "/bin/sh",
        &PrivilegeEscalation::Unavailable { uid: 1001 },
        tempdir.path(),
        |_, _, _| Ok(()),
    )
    .expect("apply");
    let blocked_run = store
        .lookup_deps_apply_run(&report.apply_run_id)
        .expect("lookup")
        .expect("run row");
    assert_eq!(
        blocked_run.status,
        crate::state::DEPS_APPLY_RUN_PRIVILEGE_BLOCKED
    );
    assert_eq!(blocked_run.privilege_required, 1);
    assert!(blocked_run.error_code.is_none());
}

#[test]
fn tracked_apply_adopts_a_preclaimed_row() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let bin = tempdir.path().join("tracked-adopt-marker");
    let bin_str = bin.to_string_lossy().into_owned();
    let config = config_with_dep(user_dep_entry(
        "tracked-adopt-marker",
        &format!("printf '#!/bin/sh\\nexit 0\\n' > {bin_str} && chmod 755 {bin_str}"),
        &bin_str,
    ));
    let store = open_tracked_store(tempdir.path());
    // Parent-side claim, as the init async branch performs before spawning.
    store
        .claim_deps_apply_run(
            crate::state::NewDepsApplyRun {
                id: "dap_adopt",
                origin: crate::state::DEPS_APPLY_ORIGIN_INIT_BACKGROUND,
                init_run_id: Some("irun_adopt"),
                feature: None,
                pid: None,
                boot_id: None,
                total: 1,
            },
            &|_, _| true,
        )
        .expect("claim");

    let report = apply_dependencies_tracked(
        &config,
        &store,
        TrackedApplyRun::Adopt {
            apply_run_id: "dap_adopt",
        },
        None,
        "/bin/sh",
        &PrivilegeEscalation::NotNeeded,
        tempdir.path(),
        |_, _, _| Ok(()),
    )
    .expect("adopted apply");
    assert_eq!(report.apply_run_id, "dap_adopt");
    let run = store
        .lookup_deps_apply_run("dap_adopt")
        .expect("lookup")
        .expect("run row");
    assert_eq!(run.status, crate::state::DEPS_APPLY_RUN_SUCCEEDED);
    assert_eq!(run.origin, crate::state::DEPS_APPLY_ORIGIN_INIT_BACKGROUND);
    assert_eq!(run.init_run_id.as_deref(), Some("irun_adopt"));
}

#[test]
fn tracked_apply_rejects_adopting_a_settled_row() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = open_tracked_store(tempdir.path());
    let config = config_with_dep(user_dep_entry("never-runs", "exit 0", "never-resolves"));
    let error = apply_dependencies_tracked(
        &config,
        &store,
        TrackedApplyRun::Adopt {
            apply_run_id: "dap_missing",
        },
        None,
        "/bin/sh",
        &PrivilegeEscalation::NotNeeded,
        tempdir.path(),
        |_, _, _| Ok(()),
    )
    .expect_err("adopting a missing row must fail");
    assert!(matches!(error, StackError::InvalidParam { .. }));
}

#[test]
fn tracked_claim_is_rejected_while_another_apply_is_live() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = open_tracked_store(tempdir.path());
    // A live claim stamped with this test process's own pid.
    store
        .claim_deps_apply_run(
            crate::state::NewDepsApplyRun {
                id: "dap_live",
                origin: crate::state::DEPS_APPLY_ORIGIN_API,
                init_run_id: None,
                feature: None,
                pid: Some(i64::from(std::process::id())),
                boot_id: crate::runtime::process_runner::current_boot_id().as_deref(),
                total: 1,
            },
            &|_, _| true,
        )
        .expect("claim");
    let config = config_with_dep(user_dep_entry("never-runs", "exit 0", "never-resolves"));
    let error = apply_dependencies_tracked(
        &config,
        &store,
        TrackedApplyRun::Claim {
            origin: crate::state::DEPS_APPLY_ORIGIN_CLI,
            init_run_id: None,
        },
        None,
        "/bin/sh",
        &PrivilegeEscalation::NotNeeded,
        tempdir.path(),
        |_, _, _| Ok(()),
    )
    .expect_err("claim while live must be rejected");
    match error {
        StackError::DepsApplyInFlight { apply_run_id } => assert_eq!(apply_run_id, "dap_live"),
        other => panic!("expected DepsApplyInFlight, got {other:?}"),
    }
}
