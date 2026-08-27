use super::deps::*;
use super::*;
use clap::Parser;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

#[derive(Debug, Parser)]
struct TestInitArgs {
    #[command(flatten)]
    args: InitArgs,
}

fn parse_init_args(args: &[&str]) -> InitArgs {
    let mut argv = vec!["init-test"];
    argv.extend_from_slice(args);
    TestInitArgs::parse_from(argv).args
}

fn starter_config_from_args(args: &InitArgs) -> Config {
    let raw = starter_config(args).expect("starter config");
    config::load_config_from_str(&raw).expect("starter config validates")
}

fn collection(fresh: &[(&str, &str)]) -> AgentEnvCollection {
    AgentEnvCollection {
        flag_refs: Vec::new(),
        fresh: fresh
            .iter()
            .map(|(name, value)| {
                (
                    (*name).to_owned(),
                    zeroize::Zeroizing::new((*value).to_owned()),
                )
            })
            .collect(),
    }
}

#[test]
fn mcp_http_header_flag_supports_ref_and_template_forms() {
    let args = parse_init_args(&[
        "--agent",
        "placebo",
        "--mcp-http",
        "search=https://mcp.example.com/mcp",
        "--mcp-http-header",
        "search=Authorization:=Bearer ${SEARCH_KEY}",
        "--mcp-http-header",
        "search=X-Plain:PLAIN_REF",
    ]);
    let config = starter_config_from_args(&args);
    let config::McpServerConfig::Http(http) = &config.mcp.servers[0] else {
        panic!("expected http server");
    };
    assert_eq!(http.headers[0].name, "Authorization");
    assert_eq!(
        http.headers[0].value.as_deref(),
        Some("Bearer ${SEARCH_KEY}")
    );
    assert_eq!(http.headers[0].value_ref, None);
    assert_eq!(http.headers[1].name, "X-Plain");
    assert_eq!(http.headers[1].value_ref.as_deref(), Some("PLAIN_REF"));
}

#[test]
fn mcp_http_header_flag_rejects_pure_literal_template() {
    let args = parse_init_args(&[
        "--agent",
        "placebo",
        "--mcp-http",
        "search=https://mcp.example.com/mcp",
        "--mcp-http-header",
        "search=Authorization:=Bearer plaintext",
    ]);
    let error = starter_config(&args).expect_err("pure literal must be rejected");
    assert!(
        error.to_string().contains("no `${NAME}` reference"),
        "{error}"
    );
}

#[test]
fn mcp_flag_rejection_of_pasted_credentials_never_echoes_them() {
    let secret = "sk-live-AAAABBBBCCCC";
    let args = parse_init_args(&[
        "--agent",
        "placebo",
        "--mcp-http",
        "search=https://mcp.example.com/mcp",
        "--mcp-http-header",
        &format!("search=Authorization:{secret}"),
    ]);
    let error = starter_config(&args).expect_err("secret-shaped header ref must be rejected");
    assert!(!error.to_string().contains(secret), "{error}");

    let args = parse_init_args(&[
        "--agent",
        "placebo",
        "--mcp-stdio",
        "db=db-mcp",
        "--mcp-stdio-env",
        &format!("db=URL=x-${{{secret}}}"),
    ]);
    let error = starter_config(&args).expect_err("secret-shaped env template must be rejected");
    assert!(!error.to_string().contains(secret), "{error}");
}

#[test]
fn mcp_stdio_env_flag_supports_templated_entries() {
    let args = parse_init_args(&[
        "--agent",
        "placebo",
        "--mcp-stdio",
        "db=db-mcp",
        "--mcp-stdio-env",
        "db=DATABASE_URL=postgres://u:${DB_PASS}@h/db",
    ]);
    let config = starter_config_from_args(&args);
    let config::McpServerConfig::Stdio(stdio) = &config.mcp.servers[0] else {
        panic!("expected stdio server");
    };
    assert_eq!(stdio.env, ["DATABASE_URL=postgres://u:${DB_PASS}@h/db"]);
}

#[test]
fn mcp_http_url_allows_loopback_http_only() {
    let args = parse_init_args(&[
        "--agent",
        "placebo",
        "--mcp-http",
        "relay=http://127.0.0.1:8787/mcp",
    ]);
    starter_config(&args).expect("loopback http is allowed");

    let args = parse_init_args(&[
        "--agent",
        "placebo",
        "--mcp-http",
        "relay=http://[::1]:8787/mcp",
    ]);
    starter_config(&args).expect("ipv6 loopback http is allowed");

    let args = parse_init_args(&[
        "--agent",
        "placebo",
        "--mcp-http",
        "external=http://mcp.example.com/mcp",
    ]);
    let error = starter_config(&args).expect_err("non-loopback http must be rejected");
    assert!(error.to_string().contains("https://"), "{error}");
}

// A colliding name must be rejected before the upsert, leaving the existing secret untouched.
#[test]
fn apply_agent_env_refuses_to_overwrite_existing_secret() {
    let home = tempdir().expect("tempdir");
    let mut store = SecretStore::open_or_create(home.path()).expect("store");
    store
        .set("ADMIN_KEY", "original-admin-secret")
        .expect("seed");

    let error = apply_agent_env_collection(&mut store, &collection(&[("ADMIN_KEY", "attacker")]))
        .expect_err("collision with an existing secret must be rejected");
    assert!(error.to_string().contains("already exists"), "got: {error}");
    // A ref name can itself be credential-shaped, so the complaint quotes nothing back.
    assert!(!error.to_string().contains("ADMIN_KEY"), "got: {error}");
    assert_eq!(
        store.get("ADMIN_KEY").expect("preserved"),
        "original-admin-secret",
        "the existing secret must not be overwritten"
    );
}

#[test]
fn apply_agent_env_rejects_invalid_ref_name() {
    let home = tempdir().expect("tempdir");
    let mut store = SecretStore::open_or_create(home.path()).expect("store");

    let error = apply_agent_env_collection(&mut store, &collection(&[("bad-name", "v")]))
        .expect_err("an invalid ref name must be rejected");
    assert!(
        error.to_string().contains("secret ref name must use"),
        "got: {error}"
    );
    // The rejected entry is often a credential pasted where a ref name belongs, so the
    // complaint states the constraint and quotes nothing back.
    assert!(!error.to_string().contains("bad-name"), "got: {error}");
}

#[test]
fn apply_agent_env_stores_a_new_secret() {
    let home = tempdir().expect("tempdir");
    let mut store = SecretStore::open_or_create(home.path()).expect("store");

    apply_agent_env_collection(&mut store, &collection(&[("GITHUB_TOKEN", "ghp_value")]))
        .expect("a new, valid ref should be stored");
    assert_eq!(store.get("GITHUB_TOKEN").expect("stored"), "ghp_value");
}

// Scripted hosted-prompt driver: `selects`/`confirms` dequeue in call order.
struct ScriptedPromptDriver {
    selects: Mutex<VecDeque<Option<usize>>>,
    confirms: Mutex<VecDeque<bool>>,
    passwords: Mutex<VecDeque<Option<String>>>,
    requests: Mutex<Vec<prompt::HostedPromptRequest>>,
}

impl ScriptedPromptDriver {
    fn new(selects: Vec<Option<usize>>, confirms: Vec<bool>) -> Self {
        Self::with_passwords(selects, confirms, Vec::new())
    }

    fn with_passwords(
        selects: Vec<Option<usize>>,
        confirms: Vec<bool>,
        passwords: Vec<Option<String>>,
    ) -> Self {
        Self {
            selects: Mutex::new(VecDeque::from(selects)),
            confirms: Mutex::new(VecDeque::from(confirms)),
            passwords: Mutex::new(VecDeque::from(passwords)),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl prompt::HostedPromptDriver for ScriptedPromptDriver {
    fn select(
        &self,
        request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
        self.requests.lock().expect("requests lock").push(request);
        Ok(prompt::HostedPromptOutcome::Handled(
            self.selects
                .lock()
                .expect("selects lock")
                .pop_front()
                .expect("scripted select"),
        ))
    }

    fn confirm(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<bool>> {
        Ok(prompt::HostedPromptOutcome::Handled(
            self.confirms
                .lock()
                .expect("confirms lock")
                .pop_front()
                .expect("scripted confirm"),
        ))
    }

    fn text(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
        Ok(prompt::HostedPromptOutcome::Handled(None))
    }

    fn password(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
        // An empty queue yields None, ending any add-loop, so wizard tests need no scripting.
        Ok(prompt::HostedPromptOutcome::Handled(
            self.passwords
                .lock()
                .expect("passwords lock")
                .pop_front()
                .unwrap_or(None),
        ))
    }

    fn progress(&self, _message: String) {}

    fn result(&self, _payload: serde_json::Value) {}
}

// A hosted driver whose prompts are all Unhandled, so flows must skip cleanly.
struct UnhandledPromptDriver;

impl prompt::HostedPromptDriver for UnhandledPromptDriver {
    fn select(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<usize>>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn confirm(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<bool>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn text(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn password(
        &self,
        _request: prompt::HostedPromptRequest,
    ) -> Result<prompt::HostedPromptOutcome<Option<String>>> {
        Ok(prompt::HostedPromptOutcome::Unhandled)
    }

    fn progress(&self, _message: String) {}

    fn result(&self, _payload: serde_json::Value) {}
}

fn run_environment_configuration(
    driver: Arc<dyn prompt::HostedPromptDriver>,
    args: &mut InitArgs,
) -> Result<()> {
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let skill_catalog = SkillCatalog::load_embedded().expect("skill catalog");
    prompt::with_hosted_driver(driver, || {
        prompt_environment_configuration_if_needed(args, &registry, &skill_catalog)
    })
}

// Standard Setup (path index 0) makes exactly one select; an extra one drains the queue
// and panics, which is what pins the lane to the Standard seams only.
#[test]
fn standard_setup_enables_essential_deps_and_browser_use() {
    let driver = Arc::new(ScriptedPromptDriver::new(
        vec![Some(0)],
        vec![true, true, false],
    ));
    let mut args = parse_init_args(&["--agent", "placebo"]);

    run_environment_configuration(driver, &mut args).expect("standard setup");

    assert!(args.standard_agent_work_deps);
    assert!(args.browser_use_profile);
    assert!(!args.prompt_skills);
    assert!(!args.prompt_agent_env_refs);
    assert!(args.prompt_data_sources.is_empty());
}

// Standard Setup with every prompt declined enables nothing.
#[test]
fn standard_setup_decline_all_enables_nothing() {
    let driver = Arc::new(ScriptedPromptDriver::new(
        vec![Some(0)],
        vec![false, false, false],
    ));
    let mut args = parse_init_args(&["--agent", "placebo"]);

    run_environment_configuration(driver, &mut args).expect("standard setup");

    assert!(!args.standard_agent_work_deps);
    assert!(!args.browser_use_profile);
    assert!(!args.prompt_skills);
    assert!(args.prompt_data_sources.is_empty());
}

#[test]
fn standard_setup_adds_essential_skills_for_skills_capable_agent() {
    let driver = Arc::new(ScriptedPromptDriver::new(
        vec![Some(0)],
        vec![false, false, true, false],
    ));
    let mut args = parse_init_args(&["--agent", "opencode"]);

    run_environment_configuration(driver, &mut args).expect("standard setup");

    assert!(args.essential_skills);
    assert!(args.skills_source.is_none());
    assert!(args.skills.is_empty());
    assert!(!args.prompt_skills);
    assert!(args.prompt_data_sources.is_empty());
}

#[test]
fn standard_setup_keeps_explicit_skill_flags() {
    let driver = Arc::new(ScriptedPromptDriver::new(
        vec![Some(0)],
        vec![false, false, false],
    ));
    let mut args = parse_init_args(&[
        "--agent",
        "opencode",
        "--skills-source",
        "anthropic",
        "--skills",
        "docx",
    ]);

    run_environment_configuration(driver, &mut args).expect("standard setup");

    assert_eq!(args.skills_source.as_deref(), Some("anthropic"));
    assert_eq!(args.skills, ["docx"]);
    assert!(!args.essential_skills);
}

// Advanced Setup (path index 1): `placebo` is absent from the embedded registry, so the
// skills prompt is skipped and only three confirms are consumed.
#[test]
fn advanced_setup_routes_agent_env_without_standard_fields() {
    let driver = Arc::new(ScriptedPromptDriver::new(
        vec![Some(1)],
        vec![false, true, false],
    ));
    let mut args = parse_init_args(&["--agent", "placebo"]);

    run_environment_configuration(driver, &mut args).expect("advanced setup");

    assert!(args.prompt_agent_env_refs);
    assert!(!args.prompt_skills);
    assert!(!args.standard_agent_work_deps);
    assert!(!args.browser_use_profile);
}

// The skills step is offered only for a skills-capable agent, which `opencode` is.
#[test]
fn advanced_setup_routes_agent_skills_for_skills_capable_agent() {
    let driver = Arc::new(ScriptedPromptDriver::new(
        vec![Some(1)],
        vec![false, true, false, false],
    ));
    let mut args = parse_init_args(&["--agent", "opencode"]);

    run_environment_configuration(driver, &mut args).expect("advanced setup");

    assert!(args.prompt_skills);
    assert!(!args.prompt_agent_env_refs);
}

// An Unhandled path prompt skips configuration instead of failing.
#[test]
fn unhandled_hosted_prompt_skips_environment_configuration() {
    let driver = Arc::new(UnhandledPromptDriver);
    let mut args = parse_init_args(&["--agent", "placebo"]);

    run_environment_configuration(driver, &mut args).expect("skip is not an error");

    assert!(!args.standard_agent_work_deps);
    assert!(!args.browser_use_profile);
    assert!(!args.prompt_skills);
    assert!(!args.prompt_agent_env_refs);
}

// Drives the post-probe `mcp_configure` transport loop; callers keep a clone to inspect requests.
fn run_mcp_prompt(
    driver: Arc<ScriptedPromptDriver>,
    args: &mut InitArgs,
    offer_http: bool,
) -> Result<()> {
    let hosted: Arc<dyn prompt::HostedPromptDriver> = driver;
    prompt::with_hosted_driver(hosted, || prompt_mcp_servers(true, args, offer_http))
}

fn select_labels(driver: &ScriptedPromptDriver, index: usize) -> Vec<String> {
    let requests = driver.requests.lock().expect("requests lock");
    requests[index]
        .items
        .iter()
        .map(|item| item.label.clone())
        .collect()
}

// HTTP is offered only when the agent advertised `mcpCapabilities.http`.
#[test]
fn mcp_prompt_omits_http_transport_when_not_advertised() {
    let driver = Arc::new(ScriptedPromptDriver::new(vec![Some(1)], Vec::new()));
    let mut args = parse_init_args(&["--agent", "placebo"]);

    run_mcp_prompt(driver.clone(), &mut args, false).expect("mcp prompt");

    assert_eq!(driver.requests.lock().expect("requests lock").len(), 1);
    assert_eq!(select_labels(&driver, 0), ["stdio server", "Done"]);
    assert!(args.prompt_mcp_stdio.is_empty());
    assert!(args.prompt_mcp_http.is_empty());
}

#[test]
fn mcp_prompt_offers_http_transport_when_advertised() {
    let driver = Arc::new(ScriptedPromptDriver::new(vec![Some(2)], Vec::new()));
    let mut args = parse_init_args(&["--agent", "placebo"]);

    run_mcp_prompt(driver.clone(), &mut args, true).expect("mcp prompt");

    assert_eq!(driver.requests.lock().expect("requests lock").len(), 1);
    assert_eq!(
        select_labels(&driver, 0),
        ["stdio server", "HTTP server", "Done"]
    );
    assert!(args.prompt_mcp_stdio.is_empty());
    assert!(args.prompt_mcp_http.is_empty());
}

// Choosing stdio enters the row loop and returns to the transport select, where Done exits.
#[test]
fn mcp_prompt_stdio_choice_returns_to_transport_select() {
    let driver = Arc::new(ScriptedPromptDriver::new(
        vec![Some(0), Some(1)],
        Vec::new(),
    ));
    let mut args = parse_init_args(&["--agent", "placebo"]);

    run_mcp_prompt(driver.clone(), &mut args, false).expect("mcp prompt");

    assert_eq!(driver.requests.lock().expect("requests lock").len(), 2);
    assert!(args.prompt_mcp_stdio.is_empty());
    assert!(args.prompt_mcp_http.is_empty());
}

fn prompted_stdio(name: &str) -> InitMcpStdioServer {
    InitMcpStdioServer {
        name: name.to_owned(),
        command: format!("mcp-{name}"),
        args: Vec::new(),
        env: Vec::new(),
    }
}

#[test]
fn merge_prompted_mcp_servers_appends_and_reports_added_names() {
    let mut existing =
        mcp_servers_from_prompted(&[prompted_stdio("files")], &[]).expect("valid servers");
    let batch = mcp_servers_from_prompted(&[prompted_stdio("search")], &[]).expect("valid servers");

    let added = merge_prompted_mcp_servers(&mut existing, batch).expect("merge");

    assert_eq!(added, ["search".to_owned()]);
    assert_eq!(existing.len(), 2);
    assert_eq!(existing[1].name(), "search");
}

#[test]
fn merge_prompted_mcp_servers_rejects_duplicate_within_batch() {
    let mut existing = Vec::new();
    let batch = mcp_servers_from_prompted(&[prompted_stdio("files"), prompted_stdio("files")], &[])
        .expect("valid servers");

    match merge_prompted_mcp_servers(&mut existing, batch) {
        Err(StackError::InvalidParam { reason, .. }) => {
            assert!(reason.contains("`files`"), "{reason}");
        }
        other => panic!("expected InvalidParam, got {other:?}"),
    }
    assert!(existing.is_empty(), "rejected batch must not append");
}

#[test]
fn merge_prompted_mcp_servers_rejects_name_already_in_config() {
    let mut existing =
        mcp_servers_from_prompted(&[prompted_stdio("files")], &[]).expect("valid servers");
    let batch = mcp_servers_from_prompted(&[prompted_stdio("files")], &[]).expect("valid servers");

    match merge_prompted_mcp_servers(&mut existing, batch) {
        Err(StackError::InvalidParam { reason, .. }) => {
            assert!(reason.contains("`files`"), "{reason}");
        }
        other => panic!("expected InvalidParam, got {other:?}"),
    }
    assert_eq!(existing.len(), 1, "rejected batch must not append");
}

// Declared refs iterate in BTreeSet order: AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
// FILES_TOKEN, PRESENT_REF, SEARCH_API_KEY.
fn declared_refs_config() -> Config {
    let mut args = parse_init_args(&["--agent", "placebo"]);
    args.prompt_mcp_stdio.push(InitMcpStdioServer {
        name: "files".to_owned(),
        command: "mcp-files".to_owned(),
        args: Vec::new(),
        env: vec!["FILES_TOKEN".to_owned(), "PRESENT_REF".to_owned()],
    });
    args.prompt_mcp_http.push(InitMcpHttpServer {
        name: "search".to_owned(),
        url: "https://mcp.example.com/mcp".to_owned(),
        headers: vec![InitMcpHttpHeader {
            name: "Authorization".to_owned(),
            value_ref: Some("SEARCH_API_KEY".to_owned()),
            value: None,
        }],
    });
    args.prompt_data_sources.push(config::DataSourceConfig {
        source_type: "s3".to_owned(),
        name: Some("corpus".to_owned()),
        path: None,
        url: None,
        expected_sha256: None,
        max_download_bytes: None,
        max_extracted_bytes: None,
        bucket: Some("my-bucket".to_owned()),
        prefix: None,
        region: Some("us-east-1".to_owned()),
        access_key_ref: Some("AWS_ACCESS_KEY_ID".to_owned()),
        secret_key_ref: Some("AWS_SECRET_ACCESS_KEY".to_owned()),
    });
    starter_config_from_args(&args)
}

#[test]
fn collect_declared_secret_refs_prompts_missing_and_skips_unanswered() {
    let home = tempdir().expect("tempdir");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    store
        .set_many([("PRESENT_REF", "already-there")])
        .expect("seed store");
    let config = declared_refs_config();
    // AWS_ACCESS_KEY_ID answered, AWS_SECRET_ACCESS_KEY skipped (None),
    // FILES_TOKEN answered, PRESENT_REF not prompted, SEARCH_API_KEY answered.
    let driver = Arc::new(ScriptedPromptDriver::with_passwords(
        Vec::new(),
        Vec::new(),
        vec![
            Some("ak-value".to_owned()),
            None,
            Some("ft-value".to_owned()),
            Some("sk-value".to_owned()),
        ],
    ));
    let stored = prompt::with_hosted_driver(driver, || {
        super::super::provider::collect_declared_secret_refs_for_init(true, &config, &mut store)
    })
    .expect("collection must not fail on a skipped ref");
    assert_eq!(
        stored,
        vec![
            "AWS_ACCESS_KEY_ID".to_owned(),
            "FILES_TOKEN".to_owned(),
            "SEARCH_API_KEY".to_owned(),
        ]
    );
    assert!(store.contains("AWS_ACCESS_KEY_ID"));
    assert!(store.contains("FILES_TOKEN"));
    assert!(store.contains("SEARCH_API_KEY"));
    assert!(!store.contains("AWS_SECRET_ACCESS_KEY"));
}

// Unhandled password prompts map to skip, so collection is a clean no-op rather than a wedge.
#[test]
fn collect_declared_secret_refs_is_noop_under_unhandled_driver() {
    let home = tempdir().expect("tempdir");
    let mut store = SecretStore::open_or_create(home.path()).expect("secret store");
    let config = declared_refs_config();
    let stored = prompt::with_hosted_driver(Arc::new(UnhandledPromptDriver), || {
        super::super::provider::collect_declared_secret_refs_for_init(true, &config, &mut store)
    })
    .expect("unhandled prompts must not fail collection");
    assert!(stored.is_empty());
    assert!(!store.contains("FILES_TOKEN"));
}

#[test]
fn structured_declarations_rejected_for_existing_config() {
    let mut mcp_args = parse_init_args(&[]);
    mcp_args.prompt_mcp_stdio.push(InitMcpStdioServer {
        name: "files".to_owned(),
        command: "mcp-files".to_owned(),
        args: Vec::new(),
        env: Vec::new(),
    });
    assert!(reject_starter_only_mcp_args_for_existing_config(&mcp_args).is_err());

    let mut http_args = parse_init_args(&[]);
    http_args.prompt_mcp_http.push(InitMcpHttpServer {
        name: "search".to_owned(),
        url: "https://mcp.example.com/mcp".to_owned(),
        headers: Vec::new(),
    });
    assert!(reject_starter_only_mcp_args_for_existing_config(&http_args).is_err());

    let mut standard_args = parse_init_args(&[]);
    standard_args.standard_agent_work_deps = true;
    assert!(reject_deps_args_for_existing_config(&standard_args).is_err());

    let mut browser_args = parse_init_args(&[]);
    browser_args.browser_use_profile = true;
    assert!(reject_deps_args_for_existing_config(&browser_args).is_err());

    let data_args = parse_init_args(&["--data-from", "/srv/import"]);
    assert!(reject_data_source_args_for_existing_config(&data_args).is_err());

    let mut source_args = parse_init_args(&[]);
    source_args
        .prompt_data_sources
        .push(config::DataSourceConfig {
            source_type: "local".to_owned(),
            name: None,
            path: Some("/srv/import".to_owned()),
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        });
    assert!(reject_data_source_args_for_existing_config(&source_args).is_err());

    assert!(reject_starter_only_mcp_args_for_existing_config(&parse_init_args(&[])).is_ok());
    assert!(reject_deps_args_for_existing_config(&parse_init_args(&[])).is_ok());
    assert!(reject_data_source_args_for_existing_config(&parse_init_args(&[])).is_ok());
}

// A declared-but-unsatisfiable skills install is a hard error; the wizard gates the offer
// instead, and silently skipping a hosted declaration would be worse.
#[test]
fn essential_skills_declaration_fails_for_agent_without_skills_support() {
    let home = tempdir().expect("tempdir");
    let mut args = parse_init_args(&["--agent", "placebo"]);
    args.essential_skills = true;
    let config = starter_config_from_args(&args);
    let registry = RegistryCatalog::load_embedded().expect("registry");
    let skill_catalog = SkillCatalog::load_embedded().expect("skill catalog");
    let error = super::super::skills::resolve_skill_install_plan(
        &args,
        home.path(),
        &config,
        &registry,
        &skill_catalog,
    )
    .expect_err("essential skills for a non-skills agent must fail");
    assert!(matches!(error, StackError::SkillInstallFailed { .. }));
}

#[test]
fn sandbox_flag_sets_workspace_sandbox_mode() {
    let args = parse_init_args(&["--agent", "placebo", "--sandbox", "unshare"]);
    let config = starter_config_from_args(&args);
    assert_eq!(config.workspace.sandbox.mode, SandboxMode::Unshare);

    let canonical = config.to_canonical_toml().expect("canonical config");
    assert!(
        canonical.contains("[workspace.sandbox]") && canonical.contains("mode = \"unshare\""),
        "expected the sandbox section in the generated config:\n{canonical}"
    );
}

#[test]
fn sandbox_absent_keeps_off_and_omits_section() {
    let args = parse_init_args(&["--agent", "placebo"]);
    let config = starter_config_from_args(&args);
    assert_eq!(config.workspace.sandbox.mode, SandboxMode::Off);

    let canonical = config.to_canonical_toml().expect("canonical config");
    assert!(
        !canonical.contains("[workspace.sandbox]"),
        "an `off` sandbox must not serialize a section:\n{canonical}"
    );
}

#[test]
fn sandbox_flag_rejects_unknown_mode() {
    let args = parse_init_args(&["--agent", "placebo", "--sandbox", "bogus"]);
    let error = starter_config(&args).expect_err("an unknown sandbox mode must be rejected");
    assert!(
        error
            .to_string()
            .contains("expected off|unshare|bwrap|custom"),
        "got: {error}"
    );
}

#[test]
fn sandbox_override_must_match_existing_config() {
    let existing = starter_config_from_args(&parse_init_args(&[
        "--agent",
        "placebo",
        "--sandbox",
        "unshare",
    ]));

    let same = parse_init_args(&["--agent", "placebo", "--sandbox", "unshare"]);
    validate_deployment_overrides_match_existing(&same, &existing)
        .expect("a matching sandbox override is accepted");

    // A conflicting value is rejected, not ignored: an operator must never believe they
    // enabled a sandbox that stays off.
    let conflict = parse_init_args(&["--agent", "placebo", "--sandbox", "off"]);
    let error = validate_deployment_overrides_match_existing(&conflict, &existing)
        .expect_err("a conflicting sandbox override must be rejected");
    assert!(error.to_string().contains("unshare"), "got: {error}");
}

#[test]
fn standard_setup_profile_declares_base_dependencies_without_build_toolchain() {
    let mut args = parse_init_args(&["--agent", "placebo"]);
    args.standard_agent_work_deps = true;
    let mut config = starter_config_from_args(&args);

    push_args_deps_to_config(&mut config, &args).expect("push standard deps");

    let bundle = config
        .dependencies
        .commands
        .iter()
        .find(|entry| entry.name == STANDARD_AGENT_WORK_BUNDLE_NAME)
        .expect("standard bundle dependency");
    let install = bundle.install.as_ref().expect("bundle install action");
    assert_eq!(bundle.feature.as_deref(), Some(STANDARD_AGENT_WORK_FEATURE));
    assert_eq!(install.scope, DependencyInstallScope::System);
    assert_eq!(
        install.creates.as_deref(),
        Some(STANDARD_AGENT_WORK_BUNDLE_CREATES)
    );
    assert!(install.shell.contains("apt-get install"));
    assert!(
            install
                .shell
                .contains("UV_PYTHON_INSTALL_DIR=/opt/acp-stack/python UV_PYTHON_BIN_DIR=/usr/local/bin uv python install 3.14"),
            "{}",
            install.shell
        );

    for command in [
        "node",
        "npm",
        "python3",
        "python3.14",
        "uv",
        "git",
        "rg",
        "jq",
    ] {
        assert!(
            config
                .dependencies
                .commands
                .iter()
                .any(|entry| entry.name == command
                    && entry.feature.as_deref() == Some(STANDARD_AGENT_WORK_FEATURE)
                    && entry.install.is_none()),
            "missing command dependency {command}"
        );
    }
    for package in STANDARD_AGENT_WORK_APT_PACKAGES {
        assert!(
            config
                .dependencies
                .packages
                .iter()
                .any(|entry| entry.name == *package
                    && entry.feature.as_deref() == Some(STANDARD_AGENT_WORK_FEATURE)),
            "missing package dependency {package}"
        );
    }
    for package in BUILD_HEAVY_APT_PACKAGES {
        assert!(
            !config
                .dependencies
                .packages
                .iter()
                .any(|entry| entry.name == *package),
            "standard setup must not include {package}"
        );
        assert!(
            !install.shell.contains(package),
            "standard install shell must not include {package}"
        );
    }

    let canonical = config.to_canonical_toml().expect("canonical config");
    config::load_config_from_str(&canonical).expect("canonical config validates");
}

#[test]
fn browser_use_profile_declares_dependency_without_generic_mcp_prompt_config() {
    let mut args = parse_init_args(&["--agent", "placebo"]);
    args.browser_use_profile = true;
    let mut config = starter_config_from_args(&args);

    push_args_deps_to_config(&mut config, &args).expect("push browser deps");

    let browser = config
        .dependencies
        .commands
        .iter()
        .find(|entry| entry.name == BROWSER_USE_MCP_COMMAND)
        .expect("browser-use launcher dependency");
    assert_eq!(browser.feature.as_deref(), Some(BROWSER_USE_FEATURE));
    let install = browser.install.as_ref().expect("browser install action");
    assert_eq!(install.scope, DependencyInstallScope::System);
    assert_eq!(install.creates.as_deref(), Some(BROWSER_USE_MCP_COMMAND));
    for required in [
        "apt-get install",
        "chromium",
        "chromium-browser",
        "uv venv --python 3.14",
        "browser-use[core]",
        BROWSER_USE_PREFIX,
        BROWSER_USE_WRAPPER_PATH,
        BROWSER_USE_LAUNCHER_PATH,
        "FastMCP",
        "BROWSER_USE_API_KEY",
        "BROWSER_USE_VENV=\"${BROWSER_USE_VENV:-/opt/acp-stack/browser-use}\"",
        "BROWSER_USE_MCP_SCRIPT=\"${BROWSER_USE_MCP_SCRIPT:-/usr/local/share/acp-stack/browser-use-mcp.py}\"",
        "exec \"${BROWSER_USE_VENV}/bin/python\"",
    ] {
        assert!(
            install.shell.contains(required),
            "browser install shell must include {required}"
        );
    }
    assert!(config.mcp.servers.is_empty());

    let canonical = config.to_canonical_toml().expect("canonical config");
    config::load_config_from_str(&canonical).expect("canonical config validates");
}

fn network_provider_extension() -> config::ExtensionConfig {
    config::ExtensionConfig {
        extension_type: config::ExtensionType::NetworkProvider,
        provider: vec!["/usr/local/bin/egress-provider".to_owned()],
        provider_timeout: None,
        provider_stderr: Default::default(),
        workload_env: Default::default(),
        capability: None,
    }
}

#[test]
fn hosted_network_provider_extension_stages_into_starter_config() {
    let mut args = parse_init_args(&["--agent", "placebo", "--sandbox", "unshare"]);
    args.prompt_extensions
        .insert("network-egress".to_owned(), network_provider_extension());
    let config = starter_config_from_args(&args);
    let extension = config
        .extensions
        .get("network-egress")
        .expect("the declaration must be staged before any tracked step runs");
    assert_eq!(
        extension.extension_type,
        config::ExtensionType::NetworkProvider
    );
    assert_eq!(
        extension.provider,
        ["/usr/local/bin/egress-provider".to_owned()]
    );
}

#[test]
fn hosted_network_provider_extension_requires_unshare_sandbox() {
    // Without `--sandbox unshare` the starter sandbox stays off, so the
    // declaration must fail config validation rather than stage an
    // unenforceable network guarantee.
    let mut args = parse_init_args(&["--agent", "placebo"]);
    args.prompt_extensions
        .insert("network-egress".to_owned(), network_provider_extension());
    let error = starter_config(&args).expect_err("validation must reject the pairing");
    assert!(
        error.to_string().contains("unshare"),
        "error must name the required sandbox mode: {error}"
    );
}

#[test]
fn hosted_extensions_are_rejected_against_an_existing_config() {
    let mut args = parse_init_args(&["--agent", "placebo"]);
    args.prompt_extensions
        .insert("network-egress".to_owned(), network_provider_extension());
    let error = reject_extensions_args_for_existing_config(&args)
        .expect_err("declarations against an existing config must be rejected");
    assert!(
        error.to_string().contains("starter config"),
        "error must explain the starter-only scope: {error}"
    );
    reject_extensions_args_for_existing_config(&parse_init_args(&["--agent", "placebo"]))
        .expect("no declarations, no rejection");
}

#[test]
fn hosted_sandbox_mask_paths_stage_into_the_starter_config() {
    let mut args = parse_init_args(&["--agent", "placebo", "--sandbox", "unshare"]);
    args.prompt_sandbox_mask_paths = vec![
        "/var/lib/network-egress".to_owned(),
        "/etc/network-egress".to_owned(),
        "/var/lib/network-egress".to_owned(),
    ];
    let config = starter_config_from_args(&args);
    assert_eq!(
        config.workspace.sandbox.mask_paths,
        [
            "/var/lib/network-egress".to_owned(),
            "/etc/network-egress".to_owned()
        ],
        "declared paths stage in declaration order, duplicates collapse"
    );
    assert_eq!(
        config.workspace.sandbox.mode,
        config::SandboxMode::Unshare,
        "the declaration leaves the sandbox mode untouched"
    );
}

#[test]
fn hosted_sandbox_mask_paths_stage_without_a_sandbox_mode_flag() {
    // The mask set matters from the first sandboxed spawn, which the extension
    // declaration can cause even while `--sandbox` is absent, so the paths stage
    // regardless of the mode flag.
    let mut args = parse_init_args(&["--agent", "placebo"]);
    args.prompt_sandbox_mask_paths = vec!["/var/lib/network-egress".to_owned()];
    let config = starter_config_from_args(&args);
    assert_eq!(
        config.workspace.sandbox.mask_paths,
        ["/var/lib/network-egress".to_owned()]
    );
}

#[test]
fn union_sandbox_mask_paths_preserves_existing_entries() {
    let merged = super::builders::union_sandbox_mask_paths(
        vec!["/daemon/state".to_owned()],
        &["/egress/config".to_owned(), "/daemon/state".to_owned()],
    )
    .expect("valid paths merge");
    assert_eq!(
        merged,
        ["/daemon/state".to_owned(), "/egress/config".to_owned()],
        "existing entries keep their place and are never dropped or duplicated"
    );
}

#[test]
fn hosted_sandbox_mask_paths_reject_blank_and_relative_entries() {
    for (declared, reason) in [
        (vec!["   ".to_owned()], "non-blank"),
        (vec!["egress/config".to_owned()], "must be absolute"),
    ] {
        let mut args = parse_init_args(&["--agent", "placebo"]);
        args.prompt_sandbox_mask_paths = declared;
        let error = starter_config(&args).expect_err("an invalid mask path must fail");
        assert!(
            error.to_string().contains(reason),
            "error must explain the rejection ({reason}): {error}"
        );
    }
}

#[test]
fn hosted_sandbox_mask_paths_are_rejected_against_an_existing_config() {
    let mut args = parse_init_args(&["--agent", "placebo"]);
    args.prompt_sandbox_mask_paths = vec!["/var/lib/network-egress".to_owned()];
    let error = reject_sandbox_mask_paths_args_for_existing_config(&args)
        .expect_err("declarations against an existing config must be rejected");
    assert!(
        error.to_string().contains("starter config"),
        "error must explain the starter-only scope: {error}"
    );
    reject_sandbox_mask_paths_args_for_existing_config(&parse_init_args(&["--agent", "placebo"]))
        .expect("no declarations, no rejection");
}
