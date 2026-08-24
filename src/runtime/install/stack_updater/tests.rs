use super::*;
use crate::state::{
    LogFilter, NewStackUpdateRun, STACK_UPDATE_OPERATION_INSTALL, STACK_UPDATE_STATUS_SKIPPED,
    STACK_UPDATE_STATUS_SUCCEEDED, StateStore,
};

fn manifest(version: &str, classification: StackReleaseClassification) -> StackReleaseManifest {
    StackReleaseManifest {
        schema_version: 1,
        repository: REPOSITORY.to_owned(),
        tag: format!("v{version}"),
        version: version.to_owned(),
        classification,
        breaking: false,
        artifacts: Vec::new(),
    }
}

fn test_config() -> Config {
    crate::config::load_config_from_str(
        r#"
[api]
bind = "127.0.0.1:7700"
public_url = "http://127.0.0.1:7700"
max_request_bytes = 1048576

[security.http]
max_request_bytes = 1048576
rate_limit_per_minute = 120
burst = 30
auth_failures_per_minute = 5
auth_block_duration = "15m"
allowed_origins = []
trust_proxy_headers = false
trusted_proxies = []

[workspace]
root = "/workspace"
uploads = "/workspace/uploads"
default_shell = "/bin/bash"
runtime_user = "acp"
max_file_bytes = 8388608

[logging]
level = "info"
local_retention_days = 30

[agent]
id = "placebo"
name = "Placebo"
command = "placebo-agent"
args = []
cwd = "/workspace"
env = []
restart = "on-crash"
"#,
    )
    .expect("test config should parse")
}

fn test_store() -> (tempfile::TempDir, StateStore) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(tempdir.path().join("state.sqlite")).expect("state open");
    store.migrate().expect("migrate");
    (tempdir, store)
}

#[test]
fn compatible_policy_installs_same_major_regular_release() {
    let release = manifest("0.2.0", StackReleaseClassification::Regular);
    let decision = update_decision(
        StackUpdatePolicy::Compatible,
        "0.1.0",
        &release,
        false,
        false,
        false,
    );
    assert_eq!(decision, StackUpdateDecision::Install);
}

#[test]
fn security_policy_installs_only_security_critical_release() {
    let regular = manifest("0.1.1", StackReleaseClassification::Regular);
    let security = manifest("0.1.2", StackReleaseClassification::SecurityCritical);
    assert_eq!(
        update_decision(
            StackUpdatePolicy::SecurityCritical,
            "0.1.0",
            &regular,
            false,
            false,
            false,
        ),
        StackUpdateDecision::ManualOnly
    );
    assert_eq!(
        update_decision(
            StackUpdatePolicy::SecurityCritical,
            "0.1.0",
            &security,
            false,
            false,
            false,
        ),
        StackUpdateDecision::Install
    );
}

#[test]
fn auto_mode_never_installs_a_downgrade() {
    let older = manifest("0.0.9", StackReleaseClassification::SecurityCritical);
    assert_eq!(
        update_decision(
            StackUpdatePolicy::Compatible,
            "0.1.0",
            &older,
            false,
            false,
            true,
        ),
        StackUpdateDecision::ManualOnly
    );
    // An explicit manual command may still roll back deliberately.
    assert_eq!(
        update_decision(
            StackUpdatePolicy::Compatible,
            "0.1.0",
            &older,
            false,
            false,
            false,
        ),
        StackUpdateDecision::Install
    );
}

#[test]
fn major_or_breaking_release_is_blocked_without_override() {
    let major = manifest("1.0.0", StackReleaseClassification::SecurityCritical);
    let mut breaking = manifest("0.1.1", StackReleaseClassification::SecurityCritical);
    breaking.breaking = true;
    assert_eq!(
        update_decision(
            StackUpdatePolicy::SecurityCritical,
            "0.1.0",
            &major,
            true,
            false,
            false,
        ),
        StackUpdateDecision::Blocked
    );
    assert_eq!(
        update_decision(
            StackUpdatePolicy::SecurityCritical,
            "0.1.0",
            &breaking,
            false,
            false,
            false,
        ),
        StackUpdateDecision::Blocked
    );
}

#[test]
fn manual_policy_does_not_auto_select_release() {
    let release = manifest("0.1.1", StackReleaseClassification::SecurityCritical);
    assert_eq!(
        update_decision(
            StackUpdatePolicy::Manual,
            "0.1.0",
            &release,
            false,
            false,
            false,
        ),
        StackUpdateDecision::ManualOnly
    );
}

#[test]
fn manual_policy_never_auto_installs_with_breaking_override() {
    let release = manifest("0.1.1", StackReleaseClassification::SecurityCritical);
    assert_eq!(
        update_decision(
            StackUpdatePolicy::Manual,
            "0.1.0",
            &release,
            false,
            true,
            true,
        ),
        StackUpdateDecision::ManualOnly
    );
}

#[test]
fn manifest_version_must_match_tag_release_version() {
    let release = ReleaseResponse {
        tag_name: "v0.1.1".to_owned(),
        prerelease: false,
        assets: Vec::new(),
    };
    let mut manifest = manifest("0.1.1", StackReleaseClassification::Regular);
    manifest.version = "9.9.9".to_owned();
    let err = validate_manifest(&manifest, &release).expect_err("mismatch should fail");
    assert!(err.to_string().contains("does not match tag"));

    manifest.version = "not-semver".to_owned();
    let err = validate_manifest(&manifest, &release).expect_err("invalid version should fail");
    assert!(err.to_string().contains("not a valid release version"));
}

#[test]
fn nightly_manifest_version_validates_against_nightly_tag() {
    let release = ReleaseResponse {
        tag_name: "v0.1.1.2".to_owned(),
        prerelease: true,
        assets: Vec::new(),
    };
    let nightly = manifest("0.1.1.2", StackReleaseClassification::Regular);
    validate_manifest(&nightly, &release).expect("nightly manifest should validate");
}

#[test]
fn release_version_parsing_accepts_stable_and_nightly_shapes() {
    assert_eq!(
        parse_version("v0.1.1"),
        Some(ReleaseVersion {
            major: 0,
            minor: 1,
            patch: 1,
            nightly: None,
        })
    );
    assert_eq!(
        parse_version("0.1.1.12"),
        Some(ReleaseVersion {
            major: 0,
            minor: 1,
            patch: 1,
            nightly: Some(12),
        })
    );
    for invalid in [
        "",
        "0.1",
        "0.1.1.1.1",
        "0.1.1-rc",
        "0.1.x",
        "v",
        "0.1.1+build",
        "0.1.01",
        "01.1.1",
    ] {
        assert_eq!(parse_version(invalid), None, "{invalid} should be rejected");
    }
}

#[test]
fn mismatched_nightly_component_fails_manifest_validation() {
    let release = ReleaseResponse {
        tag_name: "v0.1.1.2".to_owned(),
        prerelease: true,
        assets: Vec::new(),
    };
    let mut mismatched = manifest("0.1.1.2", StackReleaseClassification::Regular);
    mismatched.version = "0.1.1.3".to_owned();
    let err = validate_manifest(&mismatched, &release).expect_err("mismatch should fail");
    assert!(err.to_string().contains("does not match tag"));
}

#[test]
fn major_upgrade_detection_handles_nightly_versions() {
    assert!(is_major_upgrade("0.1.1.2", "1.0.0"));
    assert!(!is_major_upgrade("0.1.1.2", "0.2.0.1"));
}

#[test]
fn nightly_versions_order_between_base_and_next_patch() {
    let base = parse_version("0.1.1").expect("base");
    let nightly_one = parse_version("0.1.1.1").expect("nightly 1");
    let nightly_two = parse_version("0.1.1.2").expect("nightly 2");
    let next_patch = parse_version("0.1.2").expect("next patch");
    assert!(base < nightly_one && nightly_one < nightly_two && nightly_two < next_patch);
    // A same-base stable is a downgrade target from a nightly install; the
    // next patch release and later nightlies are not.
    assert!(is_version_downgrade("0.1.1.2", "0.1.1"));
    assert!(!is_version_downgrade("0.1.1.2", "0.1.2"));
    assert!(!is_version_downgrade("0.1.1", "0.1.1.1"));
}

#[test]
fn compatible_policy_installs_nightly_release() {
    let nightly = manifest("0.1.1.1", StackReleaseClassification::Regular);
    let decision = update_decision(
        StackUpdatePolicy::Compatible,
        "0.1.1",
        &nightly,
        false,
        false,
        false,
    );
    assert_eq!(decision, StackUpdateDecision::Install);
}

#[test]
fn major_upgrade_detection_normalizes_v_prefix() {
    assert!(is_major_upgrade("v0.9.0", "v1.0.0"));
    assert!(!is_major_upgrade("v1.2.0", "1.3.0"));
}

#[test]
fn failed_update_attempt_writes_run_and_event() {
    let (_tempdir, store) = test_store();
    let result = Err(StackError::InvalidParam {
        field: "test",
        reason: "broken".to_owned(),
    });
    let err = persist_update_result(&store, STACK_UPDATE_OPERATION_CHECK, false, &result)
        .expect_err("failure should be returned after logging");
    assert!(err.to_string().contains("acp-stack update failed"));

    let runs = store.query_stack_update_runs(10).expect("runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].operation, STACK_UPDATE_OPERATION_CHECK);
    assert_eq!(runs[0].status, STACK_UPDATE_STATUS_FAILED);
    assert_eq!(
        runs[0].message.as_deref(),
        Some("query parameter `test` is invalid: broken")
    );

    let events = store
        .query_events(LogFilter {
            limit: 10,
            kind: Some("stack.update.failed"),
            ..LogFilter::default()
        })
        .expect("events");
    assert_eq!(events.len(), 1);
}

#[test]
fn auto_frequency_skip_writes_run_and_event_without_network() {
    let (_tempdir, store) = test_store();
    store
        .append_stack_update_run(NewStackUpdateRun {
            operation: STACK_UPDATE_OPERATION_INSTALL,
            status: STACK_UPDATE_STATUS_SUCCEEDED,
            current_version: "0.1.0",
            target_version: Some("0.1.0"),
            target_tag: Some("v0.1.0"),
            classification: Some("regular"),
            breaking: false,
            major_upgrade: false,
            policy: "security-critical",
            auto: true,
            message: Some("previous"),
            payload_json: "{}",
        })
        .expect("seed previous run");

    let report = install_stack_update(
        &test_config(),
        &store,
        StackUpdateOptions {
            target: StackUpdateTarget::Latest,
            version: None,
            allow_breaking: false,
            auto: true,
        },
    )
    .expect("frequency skip should not hit network");
    assert_eq!(report.status, StackUpdateStatus::Skipped);

    let runs = store.query_stack_update_runs(10).expect("runs");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].status, STACK_UPDATE_STATUS_SKIPPED);
    assert!(runs[0].auto);
    let events = store
        .query_events(LogFilter {
            limit: 10,
            kind: Some("stack.update.skipped"),
            ..LogFilter::default()
        })
        .expect("events");
    assert_eq!(events.len(), 1);
}

#[test]
fn auto_frequency_gate_ignores_skip_runs_as_reference() {
    let (_tempdir, store) = test_store();
    store
        .append_stack_update_run(NewStackUpdateRun {
            operation: STACK_UPDATE_OPERATION_INSTALL,
            status: STACK_UPDATE_STATUS_SKIPPED,
            current_version: "0.1.0",
            target_version: None,
            target_tag: None,
            classification: None,
            breaking: false,
            major_upgrade: false,
            policy: "security-critical",
            auto: true,
            message: Some("auto-update checked recently; next check waits for 1d"),
            payload_json: "{}",
        })
        .expect("seed skip run");

    let report = auto_frequency_skip_report(&test_config(), &store).expect("frequency gate query");
    assert!(
        report.is_none(),
        "a recent skip row must not re-arm the frequency window"
    );
}

#[test]
fn auto_frequency_gate_counts_up_to_date_check_as_reference() {
    let (_tempdir, store) = test_store();
    // An up-to-date auto run is persisted as skipped but DID resolve a
    // release upstream, so it must re-arm the frequency window.
    store
        .append_stack_update_run(NewStackUpdateRun {
            operation: STACK_UPDATE_OPERATION_INSTALL,
            status: STACK_UPDATE_STATUS_SKIPPED,
            current_version: "0.1.0",
            target_version: Some("0.1.0"),
            target_tag: Some("v0.1.0"),
            classification: Some("regular"),
            breaking: false,
            major_upgrade: false,
            policy: "security-critical",
            auto: true,
            message: Some("already up to date"),
            payload_json: "{}",
        })
        .expect("seed up-to-date run");

    let report = auto_frequency_skip_report(&test_config(), &store).expect("frequency gate query");
    assert!(
        report.is_some(),
        "a recent up-to-date check must close the frequency gate"
    );
}

#[test]
fn auto_frequency_gate_reference_survives_many_accumulated_skip_rows() {
    let (_tempdir, store) = test_store();
    store
        .append_stack_update_run(NewStackUpdateRun {
            operation: STACK_UPDATE_OPERATION_INSTALL,
            status: STACK_UPDATE_STATUS_SUCCEEDED,
            current_version: "0.1.0",
            target_version: Some("0.1.0"),
            target_tag: Some("v0.1.0"),
            classification: Some("regular"),
            breaking: false,
            major_upgrade: false,
            policy: "security-critical",
            auto: true,
            message: Some("previous real attempt"),
            payload_json: "{}",
        })
        .expect("seed real run");
    // Enough newer skip rows to overflow any fixed recent-row window.
    for _ in 0..25 {
        store
            .append_stack_update_run(NewStackUpdateRun {
                operation: STACK_UPDATE_OPERATION_INSTALL,
                status: STACK_UPDATE_STATUS_SKIPPED,
                current_version: "0.1.0",
                target_version: None,
                target_tag: None,
                classification: None,
                breaking: false,
                major_upgrade: false,
                policy: "security-critical",
                auto: true,
                message: Some("auto-update checked recently; next check waits for 1d"),
                payload_json: "{}",
            })
            .expect("seed skip run");
    }

    let report = auto_frequency_skip_report(&test_config(), &store).expect("frequency gate query");
    assert!(
        report.is_some(),
        "the real attempt must stay the reference even behind many skip rows"
    );
}

fn make_archive(contents: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    for (name, body) in contents {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, *body)
            .expect("append archive entry");
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

#[test]
fn install_archive_swaps_existing_binary_and_removes_stale_binary() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Seed the destination with old binaries to prove they are replaced.
    for binary in BINARIES {
        fs::write(dir.path().join(binary), b"old").expect("seed old binary");
    }
    fs::write(dir.path().join("acpctl"), b"old-acpctl").expect("seed old acpctl");
    let archive = make_archive(&[("acps", b"new-acps")]);

    install_archive(&archive, dir.path()).expect("install archive");

    assert_eq!(
        fs::read(dir.path().join("acps")).expect("read acps"),
        b"new-acps"
    );
    assert!(
        !dir.path().join("acpctl").exists(),
        "stale acpctl should be removed"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for binary in BINARIES {
            let mode = fs::metadata(dir.path().join(binary))
                .expect("metadata")
                .permissions()
                .mode();
            assert!(
                mode & 0o111 != 0,
                "{binary} should be executable after swap"
            );
        }
    }
}

#[test]
fn install_archive_missing_binary_leaves_destination_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    for binary in BINARIES {
        fs::write(dir.path().join(binary), b"old").expect("seed old binary");
    }
    fs::write(dir.path().join("acpctl"), b"old-acpctl").expect("seed old acpctl");
    // `acps` is absent, so the extract step must fail before any swap.
    let archive = make_archive(&[]);

    let err =
        install_archive(&archive, dir.path()).expect_err("missing binary should fail install");
    assert!(err.to_string().contains("acps"));

    // The pre-existing binaries are untouched because the swap never began.
    for binary in BINARIES {
        assert_eq!(
            fs::read(dir.path().join(binary)).expect("read seeded binary"),
            b"old"
        );
    }
    assert_eq!(
        fs::read(dir.path().join("acpctl")).expect("read seeded acpctl"),
        b"old-acpctl"
    );
}
