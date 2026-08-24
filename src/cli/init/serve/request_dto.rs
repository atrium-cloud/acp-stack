use super::*;

/// A guard restating one of clap's `requires`/`conflicts_with` rules at the wire boundary.
struct WireGuard {
    field: &'static str,
    violated: bool,
    reason: &'static str,
}

impl WireGuard {
    /// First violated row wins, so row order is the reporting precedence.
    fn check<const N: usize>(guards: [Self; N]) -> Result<()> {
        match guards.into_iter().find(|guard| guard.violated) {
            Some(guard) => Err(StackError::InvalidParam {
                field: guard.field,
                reason: guard.reason.to_owned(),
            }),
            None => Ok(()),
        }
    }
}

#[derive(Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct StartInitRequest {
    agent: Option<String>,
    /// Escape-hatch agent declared inline, mirroring the `--custom-agent-*`
    /// flags: its prompts are never streamed, so the whole spec must arrive
    /// here. Requires non-blank `custom_agent_command` and
    /// `custom_agent_install`; conflicts with `agent`, `provider`, `model`,
    /// `mode`, `effort`, and `custom_provider: true`. Every other
    /// `custom_agent_*` field requires it.
    custom_agent_id: Option<String>,
    custom_agent_name: Option<String>,
    custom_agent_command: Option<String>,
    #[serde(default)]
    custom_agent_args: Vec<String>,
    custom_agent_install: Option<String>,
    custom_agent_creates: Option<String>,
    provider: Option<String>,
    /// Requires `provider`.
    api_key_ref: Option<String>,
    model: Option<String>,
    /// Declared up front like `model`, so a hosted client that already knows
    /// the session mode it wants skips the streamed picker entirely. Validated
    /// against the agent's advertised modes by the shared mode lane.
    mode: Option<String>,
    /// Declared up front like `mode`. Validated against the agent's
    /// ACP-advertised reasoning-effort values (the `thought_level` session
    /// config option) by the shared effort lane.
    effort: Option<String>,
    /// Requires `provider`. Gates the custom-provider fields below
    /// (`provider_name`, `base_url`, `provider_api`, `model_name`, `context`,
    /// `output_max_tokens`), each of which requires `custom_provider: true`.
    custom_provider: Option<bool>,
    provider_name: Option<String>,
    base_url: Option<String>,
    /// Custom-provider API flavor. Requires `custom_provider: true`.
    #[schemars(extend("enum" = ["chat-completions", "responses", "anthropic-messages", null]))]
    provider_api: Option<String>,
    model_name: Option<String>,
    context: Option<String>,
    output_max_tokens: Option<String>,
    workspace_root: Option<String>,
    workspace_uploads: Option<String>,
    runtime_user: Option<String>,
    /// Sandbox isolation backend for the agent workload.
    #[schemars(extend("enum" = ["off", "unshare", "bwrap", "custom", null]))]
    sandbox: Option<String>,
    #[serde(default)]
    code_from: Vec<String>,
    #[serde(default)]
    data_from: Vec<String>,
    skip_testflight: Option<bool>,
    testflight: Option<bool>,
    native_config: Option<NativeConfigUploadRequest>,
    #[serde(default)]
    mcp_preset: Vec<String>,
    #[serde(default)]
    mcp_stdio: Vec<McpStdioServerRequest>,
    #[serde(default)]
    mcp_http: Vec<McpHttpServerRequest>,
    /// Both-or-neither with `skills`.
    skills_source: Option<String>,
    /// Both-or-neither with `skills_source`.
    #[serde(default)]
    skills: Vec<String>,
    /// Conflicts with `skills_source` / `skills`.
    essential_skills: Option<bool>,
    #[serde(default)]
    deps: Vec<DepRequest>,
    #[serde(default)]
    deps_system: Vec<DepRequest>,
    /// Both-or-neither with `deps_apply_yes`.
    deps_apply: Option<bool>,
    /// Both-or-neither with `deps_apply`.
    deps_apply_yes: Option<bool>,
    /// Requires `deps_apply`. Runs the confirmed install in a detached
    /// background worker; the deps step settles as `background` and the run
    /// is polled via `GET /v1/deps/apply/runs/{apply_run_id}`.
    deps_apply_async: Option<bool>,
    standard_agent_work_deps: Option<bool>,
    browser_use: Option<bool>,
    /// Stack self-update policy, declared up-front rather than streamed,
    /// mirroring `--stack-update`. Absent leaves the `[updates.acp_stack]`
    /// schema default.
    #[schemars(extend("enum" = ["on", "security", "off", null]))]
    stack_update: Option<String>,
    /// Requires `stack_update`. Day/week units, e.g. `1d`, `3w`.
    stack_update_frequency: Option<String>,
    /// Agent auto-update policy, mirroring `--agent-update`; honored only for
    /// managed registry agents. Absent leaves the `[agent.auto_update]` default.
    #[schemars(extend("enum" = ["on", "off", null]))]
    agent_update: Option<String>,
    /// Requires `agent_update`. Hour/day/week units, e.g. `12h`, `1d`.
    agent_update_frequency: Option<String>,
    /// The caller's declaration that it will push the configured provider's
    /// credential through the managed-state extension after init. Only then does
    /// a missing provider ref soft-pass, and only for a ref the push can deliver:
    /// a custom provider's api-key ref, or a mapped key-based provider's api-key
    /// and companion env vars under the names the agent reads. A noncanonical
    /// api-key alias, a `VAR=template` inner ref, or an agent-native-auth
    /// provider's refs cannot arrive through the push and still hard-fail.
    /// Absent → false.
    defer_provider_credentials: Option<bool>,
    #[serde(default)]
    data_sources: Vec<DataSourceRequest>,
    /// Continue the most recent unfinished or failed run instead of starting a
    /// new one, matching `--resume`. Conflicts with `fresh`.
    resume: Option<bool>,
    /// Force a new run, matching `--fresh`. Conflicts with `resume`.
    fresh: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NativeConfigUploadRequest {
    filename: String,
    content: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct McpStdioServerRequest {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    /// A bare secret-ref name exported under that name, or a `VAR=template`
    /// entry whose template interpolates `${SECRET_REF}` (see config.md).
    #[serde(default)]
    env: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct McpHttpServerRequest {
    name: String,
    url: String,
    #[serde(default)]
    headers: Vec<McpHttpHeaderRequest>,
}

/// Exactly one of `value_ref` (whole-value secret ref) or `value`
/// (`${NAME}`-interpolated template) must be set; enforced in
/// `into_init_args` so a malformed declaration is a 400 at the boundary.
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct McpHttpHeaderRequest {
    name: String,
    #[serde(default)]
    value_ref: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DepRequest {
    name: String,
    shell: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum DataSourceRequest {
    Local {
        name: Option<String>,
        path: String,
    },
    Https {
        name: Option<String>,
        url: String,
        expected_sha256: Option<String>,
        max_download_bytes: Option<u64>,
        max_extracted_bytes: Option<u64>,
    },
    S3 {
        name: Option<String>,
        bucket: String,
        // Required here because the config validator requires it for s3 sources.
        region: String,
        prefix: Option<String>,
        access_key_ref: String,
        secret_key_ref: String,
    },
}

impl DataSourceRequest {
    fn into_data_source_config(self) -> config::DataSourceConfig {
        let mut source = config::DataSourceConfig {
            source_type: String::new(),
            name: None,
            path: None,
            url: None,
            expected_sha256: None,
            max_download_bytes: None,
            max_extracted_bytes: None,
            bucket: None,
            prefix: None,
            region: None,
            access_key_ref: None,
            secret_key_ref: None,
        };
        match self {
            DataSourceRequest::Local { name, path } => {
                source.source_type = "local".to_owned();
                source.name = name;
                source.path = Some(path);
            }
            DataSourceRequest::Https {
                name,
                url,
                expected_sha256,
                max_download_bytes,
                max_extracted_bytes,
            } => {
                source.source_type = "https".to_owned();
                source.name = name;
                source.url = Some(url);
                source.expected_sha256 = expected_sha256;
                source.max_download_bytes = max_download_bytes;
                source.max_extracted_bytes = max_extracted_bytes;
            }
            DataSourceRequest::S3 {
                name,
                bucket,
                region,
                prefix,
                access_key_ref,
                secret_key_ref,
            } => {
                source.source_type = "s3".to_owned();
                source.name = name;
                source.bucket = Some(bucket);
                source.region = Some(region);
                source.prefix = prefix;
                source.access_key_ref = Some(access_key_ref);
                source.secret_key_ref = Some(secret_key_ref);
            }
        }
        source
    }
}

impl StartInitRequest {
    /// Restates the `--custom-agent-*` clap rules at the wire boundary. Reasons name fields only:
    /// a rejected declaration must never echo the submitted id or command into the 400 body.
    fn validate_custom_agent_declaration(&self) -> Result<()> {
        let dependents: [(&'static str, bool); 5] = [
            ("custom_agent_name", self.custom_agent_name.is_some()),
            ("custom_agent_command", self.custom_agent_command.is_some()),
            ("custom_agent_args", !self.custom_agent_args.is_empty()),
            ("custom_agent_install", self.custom_agent_install.is_some()),
            ("custom_agent_creates", self.custom_agent_creates.is_some()),
        ];
        if self.custom_agent_id.is_none() {
            if let Some((field, _)) = dependents.into_iter().find(|(_, present)| *present) {
                return Err(StackError::InvalidParam {
                    field,
                    reason: format!("{field} requires custom_agent_id"),
                });
            }
            return Ok(());
        }
        // Booleans are judged on their effective value: an explicit `false` declares nothing
        // and must not collide.
        let conflicts: [(&'static str, bool); 6] = [
            ("agent", self.agent.is_some()),
            ("provider", self.provider.is_some()),
            ("model", self.model.is_some()),
            ("mode", self.mode.is_some()),
            ("effort", self.effort.is_some()),
            ("custom_provider", self.custom_provider.unwrap_or(false)),
        ];
        if let Some((field, _)) = conflicts.into_iter().find(|(_, present)| *present) {
            return Err(StackError::InvalidParam {
                field: "custom_agent_id",
                reason: format!("custom_agent_id conflicts with {field}"),
            });
        }
        // Blank counts as absent, matching `require_custom_flag`.
        let declared = |value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        };
        for (field, present) in [
            ("custom_agent_command", declared(&self.custom_agent_command)),
            ("custom_agent_install", declared(&self.custom_agent_install)),
        ] {
            if !present {
                return Err(StackError::InvalidParam {
                    field,
                    reason: format!("custom_agent_id requires {field}"),
                });
            }
        }
        Ok(())
    }

    /// Wire guards for what clap or the engine cannot structurally catch. Group order is
    /// observable: the first rejection is the one reported.
    fn validate(&self) -> Result<()> {
        let custom_provider = self.custom_provider.unwrap_or(false);
        WireGuard::check([
            // Stricter than the CLI's `requires`: the hosted driver never streams the apply
            // confirmation, so `deps_apply` alone would silently default to "not applied".
            WireGuard {
                field: "deps_apply",
                violated: self.deps_apply.unwrap_or(false) != self.deps_apply_yes.unwrap_or(false),
                reason: "deps_apply and deps_apply_yes must be set together",
            },
            WireGuard {
                field: "deps_apply_async",
                violated: self.deps_apply_async.unwrap_or(false)
                    && !self.deps_apply.unwrap_or(false),
                reason: "deps_apply_async requires deps_apply",
            },
            // A frequency with no policy would be silently dropped by the configure step.
            WireGuard {
                field: "stack_update_frequency",
                violated: self.stack_update_frequency.is_some() && self.stack_update.is_none(),
                reason: "stack_update_frequency requires stack_update",
            },
            WireGuard {
                field: "agent_update_frequency",
                violated: self.agent_update_frequency.is_some() && self.agent_update.is_none(),
                reason: "agent_update_frequency requires agent_update",
            },
        ])?;
        self.validate_custom_agent_declaration()?;
        // Ordered after the custom-agent declaration so a request that names both still reports
        // the custom-agent conflict.
        if self.provider.is_none() {
            for (field, declared) in [
                ("api_key_ref", self.api_key_ref.is_some()),
                ("custom_provider", custom_provider),
            ] {
                if declared {
                    return Err(StackError::InvalidParam {
                        field,
                        reason: format!("{field} requires provider"),
                    });
                }
            }
        }
        if !custom_provider {
            for (field, declared) in [
                ("provider_name", self.provider_name.is_some()),
                ("base_url", self.base_url.is_some()),
                ("provider_api", self.provider_api.is_some()),
                ("model_name", self.model_name.is_some()),
                ("context", self.context.is_some()),
                ("output_max_tokens", self.output_max_tokens.is_some()),
            ] {
                if declared {
                    return Err(StackError::InvalidParam {
                        field,
                        reason: format!("{field} requires custom_provider"),
                    });
                }
            }
        }
        let essential_skills = self.essential_skills.unwrap_or(false);
        WireGuard::check([
            WireGuard {
                field: "resume",
                violated: self.resume.unwrap_or(false) && self.fresh.unwrap_or(false),
                reason: "resume conflicts with fresh",
            },
            WireGuard {
                field: "essential_skills",
                violated: essential_skills
                    && (self.skills_source.is_some() || !self.skills.is_empty()),
                reason: "essential_skills conflicts with skills_source/skills",
            },
            WireGuard {
                field: "skills",
                violated: self.skills_source.is_some() == self.skills.is_empty(),
                reason: "skills and skills_source must be declared together",
            },
        ])?;
        for dep in self.deps.iter().chain(self.deps_system.iter()) {
            if dep.name.trim().is_empty() || dep.shell.trim().is_empty() {
                return Err(StackError::InvalidParam {
                    field: "deps",
                    reason: "dependency name and shell must not be empty".to_owned(),
                });
            }
            if dep.name.contains('=') {
                return Err(StackError::InvalidParam {
                    field: "deps",
                    reason: format!("dependency name `{}` must not contain `=`", dep.name),
                });
            }
        }
        Ok(())
    }

    pub(super) fn into_init_args(self) -> Result<InitArgs> {
        self.validate()?;
        let custom_provider = self.custom_provider.unwrap_or(false);
        let resume = self.resume.unwrap_or(false);
        let essential_skills = self.essential_skills.unwrap_or(false);
        let mut args = InitArgs::default();
        args.agent = self.agent;
        args.custom_agent_id = self.custom_agent_id;
        args.custom_agent_name = self.custom_agent_name;
        args.custom_agent_command = self.custom_agent_command;
        args.custom_agent_arg = self.custom_agent_args;
        args.custom_agent_install = self.custom_agent_install;
        args.custom_agent_creates = self.custom_agent_creates;
        args.resume = resume;
        args.fresh = self.fresh.unwrap_or(false);
        args.provider = self.provider;
        args.api_key_ref = self.api_key_ref;
        args.model = self.model;
        args.mode = self.mode;
        args.effort = self.effort;
        args.custom_provider = custom_provider;
        args.provider_name = self.provider_name;
        args.base_url = self.base_url;
        args.provider_api = self.provider_api;
        args.model_name = self.model_name;
        args.context = self.context;
        args.output_max_tokens = self.output_max_tokens;
        args.workspace_root = self.workspace_root;
        args.workspace_uploads = self.workspace_uploads;
        args.runtime_user = self.runtime_user;
        args.sandbox = self.sandbox;
        args.code_from = self.code_from;
        args.data_from = self.data_from;
        args.skip_testflight = self.skip_testflight.unwrap_or(false);
        args.testflight = self.testflight.unwrap_or(false);
        args.native_config_upload = self.native_config.map(|upload| InitNativeConfigUpload {
            filename: upload.filename,
            content: Zeroizing::new(upload.content),
        });
        args.mcp_preset = self.mcp_preset;
        // Screening MUST run before any name-shape check: a screening rejection redacts a pasted
        // credential, while name-shape errors echo the offending string into the 400 body.
        args.prompt_mcp_stdio = self
            .mcp_stdio
            .into_iter()
            .map(|server| {
                for entry in &server.env {
                    crate::config::screen_env_entry("mcp_stdio.env", entry)
                        .and_then(|()| crate::config::parse_env_entry("mcp_stdio.env", entry))
                        .map_err(|error| StackError::InvalidParam {
                            field: "mcp_stdio.env",
                            reason: error.to_string(),
                        })?;
                }
                Ok(InitMcpStdioServer {
                    name: server.name,
                    command: server.command,
                    args: server.args,
                    env: server.env,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        args.prompt_mcp_http = self
            .mcp_http
            .into_iter()
            .map(|server| {
                let headers = server
                    .headers
                    .into_iter()
                    .map(|header| {
                        match (header.value_ref.as_deref(), header.value.as_deref()) {
                            (Some(value_ref), None) => {
                                crate::config::screen_ref_name("mcp_http.headers", value_ref)
                                    .and_then(|()| {
                                        crate::config::validate_secret_ref_name_value(value_ref)
                                    })
                                    .map_err(|error| StackError::InvalidParam {
                                        field: "mcp_http.headers",
                                        reason: error.to_string(),
                                    })?;
                            }
                            (None, Some(template)) => {
                                crate::config::screen_template("mcp_http.headers", template)
                                    .and_then(|()| {
                                        crate::config::SecretTemplate::parse(
                                            "mcp_http.headers",
                                            template,
                                        )
                                        .map(|_| ())
                                    })
                                    .map_err(|error| StackError::InvalidParam {
                                        field: "mcp_http.headers",
                                        reason: error.to_string(),
                                    })?;
                            }
                            _ => {
                                return Err(StackError::InvalidParam {
                                    field: "mcp_http.headers",
                                    reason: format!(
                                        "header `{}` must set exactly one of `value_ref` or `value`",
                                        header.name
                                    ),
                                });
                            }
                        }
                        Ok(InitMcpHttpHeader {
                            name: header.name,
                            value_ref: header.value_ref,
                            value: header.value,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(InitMcpHttpServer {
                    name: server.name,
                    url: server.url,
                    headers,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // `InitArgs::default` sets `no_skills: true` and both the plan resolver and the recorded
        // skills replay short-circuit on it, so a declaration (or a resume) must clear it or the
        // original run's skill plan is dropped and the resumed step fails as corrupted.
        args.no_skills =
            !resume && self.skills_source.is_none() && self.skills.is_empty() && !essential_skills;
        args.skills_source = self.skills_source;
        args.skills = self.skills;
        args.essential_skills = essential_skills;
        args.dep = self
            .deps
            .iter()
            .map(|dep| format!("{}={}", dep.name, dep.shell))
            .collect();
        args.dep_system = self
            .deps_system
            .iter()
            .map(|dep| format!("{}={}", dep.name, dep.shell))
            .collect();
        args.deps_apply = self.deps_apply.unwrap_or(false);
        args.deps_apply_yes = self.deps_apply_yes.unwrap_or(false);
        args.deps_apply_async = self.deps_apply_async.unwrap_or(false);
        args.standard_agent_work_deps = self.standard_agent_work_deps.unwrap_or(false);
        args.browser_use_profile = self.browser_use.unwrap_or(false);
        args.stack_update = self.stack_update;
        args.stack_update_frequency = self.stack_update_frequency;
        args.agent_update = self.agent_update;
        args.agent_update_frequency = self.agent_update_frequency;
        args.defer_provider_credentials = self.defer_provider_credentials.unwrap_or(false);
        args.prompt_data_sources = self
            .data_sources
            .into_iter()
            .map(DataSourceRequest::into_data_source_config)
            .collect();
        // No request field for `rotate_keys`: hosted mode forces it true at init entry, so any
        // resume rotates exactly once per resumed run.
        Ok(args)
    }
}
