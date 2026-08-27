use super::*;

pub(super) struct SessionPromptDriver {
    pub(super) session: Arc<HostedInitSession>,
}

impl HostedPromptDriver for SessionPromptDriver {
    fn select(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<usize>>> {
        let Some(answer) = self.session.request_input(request.clone())? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let selection = parse_optional_index(&answer.value, &request)?;
        Ok(HostedPromptOutcome::Handled(selection))
    }

    fn confirm(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<bool>> {
        Ok(match self.confirm_with_deferral(request)? {
            HostedPromptOutcome::Handled(answer) => HostedPromptOutcome::Handled(answer.value),
            HostedPromptOutcome::Unhandled => HostedPromptOutcome::Unhandled,
        })
    }

    fn confirm_with_deferral(
        &self,
        request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<ConfirmAnswer>> {
        let Some(answer) = self.session.request_input(request)? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let Some(value) = answer.value.as_bool() else {
            return Err(StackError::InvalidParam {
                field: "init",
                reason: "confirm input must be a boolean".to_owned(),
            });
        };
        Ok(HostedPromptOutcome::Handled(ConfirmAnswer {
            value,
            deferred: answer.deferred,
        }))
    }

    fn text(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<String>>> {
        let Some(answer) = self.session.request_input(request)? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let value = answer.value;
        if value.is_null() {
            return Ok(HostedPromptOutcome::Handled(None));
        }
        let Some(value) = value.as_str() else {
            return Err(StackError::InvalidParam {
                field: "init",
                reason: "text input must be a string".to_owned(),
            });
        };
        Ok(HostedPromptOutcome::Handled(Some(value.to_owned())))
    }

    fn password(
        &self,
        request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<Option<String>>> {
        self.text(request)
    }

    fn native_config_review(
        &self,
        request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<NativeConfigSelection>> {
        let expected_revision = request
            .inspection
            .as_ref()
            .map(|inspection| inspection.revision.clone())
            .ok_or_else(|| StackError::InvalidParam {
                field: "native_config",
                reason: "native config review omitted inspection".to_owned(),
            })?;
        let Some(answer) = self.session.request_input(request)? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let selection: NativeConfigSelection =
            serde_json::from_value(answer.value).map_err(|_| StackError::InvalidParam {
                field: "native_config",
                reason: "native config review response is invalid".to_owned(),
            })?;
        if selection.revision != expected_revision {
            return Err(StackError::InvalidParam {
                field: "native_config",
                reason: "native config review revision does not match the inspection".to_owned(),
            });
        }
        Ok(HostedPromptOutcome::Handled(selection))
    }

    fn config_option(
        &self,
        request: HostedPromptRequest,
    ) -> Result<HostedPromptOutcome<Option<crate::config::AgentConfigOptionValue>>> {
        let advertised = request
            .config_option
            .clone()
            .ok_or_else(|| StackError::InvalidParam {
                field: "init",
                reason: "config-option input omitted its advertised option".to_owned(),
            })?;
        let Some(answer) = self.session.request_input(request)? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let Some(answer) = answer.value.as_object() else {
            return Err(StackError::InvalidParam {
                field: "init",
                reason: "config-option input must carry `config_id` and `value`".to_owned(),
            });
        };
        let Some(config_id) = answer.get("config_id").and_then(Value::as_str) else {
            return Err(StackError::InvalidParam {
                field: "init",
                reason: "config-option input must carry a string `config_id`".to_owned(),
            });
        };
        if config_id != advertised.id {
            return Err(StackError::InvalidParam {
                field: "init",
                reason: format!(
                    "config-option input names `{config_id}`, expected `{}`",
                    advertised.id
                ),
            });
        }
        let Some(value) = answer.get("value") else {
            return Err(StackError::InvalidParam {
                field: "init",
                reason: "config-option input omitted `value`".to_owned(),
            });
        };
        if value.is_null() {
            return Ok(HostedPromptOutcome::Handled(None));
        }
        let configured = match advertised.kind.as_str() {
            crate::runtime::agent::config_options::SNAPSHOT_KIND_BOOLEAN => value
                .as_bool()
                .map(crate::config::AgentConfigOptionValue::Bool)
                .ok_or_else(|| StackError::InvalidParam {
                    field: "init",
                    reason: format!("config option `{}` requires a boolean value", advertised.id),
                })?,
            crate::runtime::agent::config_options::SNAPSHOT_KIND_SELECT => {
                let selected = value.as_str().ok_or_else(|| StackError::InvalidParam {
                    field: "init",
                    reason: format!("config option `{}` requires a string value", advertised.id),
                })?;
                let advertised_value = advertised
                    .options
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|choice| choice.value == selected);
                if !advertised_value {
                    return Err(StackError::InvalidParam {
                        field: "init",
                        reason: format!(
                            "config option `{}` does not advertise `{selected}`",
                            advertised.id
                        ),
                    });
                }
                crate::config::AgentConfigOptionValue::Text(selected.to_owned())
            }
            _ => {
                return Err(StackError::InvalidParam {
                    field: "init",
                    reason: format!("config option `{}` has an unsupported type", advertised.id),
                });
            }
        };
        Ok(HostedPromptOutcome::Handled(Some(configured)))
    }

    fn progress(&self, message: String) {
        self.session.push_event(ServerEvent::Progress { message });
    }

    fn result(&self, payload: Value) {
        self.session.set_result(payload);
    }

    fn state_signal(&self, signal: InitStateSignal) {
        self.session.apply_state_signal(signal);
    }

    fn defer_provider_credentials(&self) -> bool {
        self.session.defer_provider_credentials()
    }
}

/// The streamed set, decided per prompt kind. The match is deliberately
/// exhaustive so a new prompt site cannot compile until someone decides whether
/// a hosted client answers it.
pub(super) fn should_handle_hosted_prompt(request: &HostedPromptRequest) -> bool {
    match request.kind {
        HostedPromptKind::Agent
        | HostedPromptKind::ProviderId
        | HostedPromptKind::ProviderName
        | HostedPromptKind::BaseUrl
        | HostedPromptKind::ApiKeyRef
        | HostedPromptKind::Model
        | HostedPromptKind::Mode
        | HostedPromptKind::Effort
        | HostedPromptKind::TestflightConfirm
        | HostedPromptKind::ProviderApiKeyValue
        | HostedPromptKind::SecretRefValue
        | HostedPromptKind::McpAdd
        | HostedPromptKind::McpTransport
        | HostedPromptKind::McpRowAction
        | HostedPromptKind::McpStdioName
        | HostedPromptKind::McpStdioCommand
        | HostedPromptKind::McpStdioArgs
        | HostedPromptKind::McpStdioEnvRefs
        | HostedPromptKind::McpHttpName
        | HostedPromptKind::McpHttpUrl
        | HostedPromptKind::McpHttpHeaders => true,
        HostedPromptKind::ConfigOption => request.config_option.is_some(),
        // A review without an inspection is unanswerable: it is what the client
        // renders and echoes back in its revision-matched selection.
        HostedPromptKind::NativeConfigReview => request.inspection.is_some(),
        HostedPromptKind::ConfigSource
        | HostedPromptKind::ConfigSourcePath
        | HostedPromptKind::ConfigSourceBase64
        | HostedPromptKind::CustomAgentId
        | HostedPromptKind::CustomAgentName
        | HostedPromptKind::CustomAgentCommand
        | HostedPromptKind::CustomAgentArgs
        | HostedPromptKind::CustomAgentInstallShell
        | HostedPromptKind::CustomAgentCreates
        | HostedPromptKind::SkillsSource
        | HostedPromptKind::SkillsGithubOwner
        | HostedPromptKind::SkillsManualNames
        | HostedPromptKind::SkillsSelect
        | HostedPromptKind::SubagentInheritConfirm
        | HostedPromptKind::StackUpdatePolicy
        | HostedPromptKind::UpdateFrequency
        | HostedPromptKind::UpdateFrequencyCustom
        | HostedPromptKind::AgentUpdateEnabled
        | HostedPromptKind::EnvironmentSetup
        | HostedPromptKind::EssentialDepsConfirm
        | HostedPromptKind::BrowserUseConfirm
        | HostedPromptKind::EssentialSkillsConfirm
        | HostedPromptKind::DataSourcesConfirm
        | HostedPromptKind::CustomDepsConfirm
        | HostedPromptKind::AgentSkillsConfirm
        | HostedPromptKind::AgentEnvRefsConfirm
        | HostedPromptKind::DataSourceType
        | HostedPromptKind::DataSourceRowAction
        | HostedPromptKind::DataSourceLocalPath
        | HostedPromptKind::DataSourceHttpsUrl
        | HostedPromptKind::DataSourceS3Bucket
        | HostedPromptKind::DataSourceS3Region
        | HostedPromptKind::DataSourceS3AccessKeyRef
        | HostedPromptKind::DataSourceS3SecretKeyRef
        | HostedPromptKind::DataSourceS3Prefix
        | HostedPromptKind::DependencyName
        | HostedPromptKind::DependencyInstallShell
        | HostedPromptKind::DependencyScope
        | HostedPromptKind::DepsApplyConfirm
        | HostedPromptKind::AgentEnvRefName => false,
    }
}

pub(super) fn public_input_request(request: HostedPromptRequest) -> PublicInputRequest {
    PublicInputRequest {
        request_id: next_input_request_id(),
        kind: request.kind.as_str(),
        style: prompt_style_label(request.style).to_owned(),
        prompt: request.prompt,
        required: request.required,
        default: request.default,
        options: request
            .items
            .into_iter()
            .enumerate()
            .map(|(index, item)| PublicInputOption {
                index,
                value: item.value,
                label: item.label,
                hint: item.hint,
            })
            .collect(),
        inspection: request.inspection,
        config_option: request.config_option,
    }
}

fn prompt_style_label(style: HostedPromptStyle) -> &'static str {
    match style {
        HostedPromptStyle::Select => "select",
        HostedPromptStyle::SearchableSelect => "searchable_select",
        HostedPromptStyle::Confirm => "confirm",
        HostedPromptStyle::Text => "text",
        HostedPromptStyle::Password => "password",
        HostedPromptStyle::NativeConfigReview => "native_config_review",
    }
}

fn parse_optional_index(value: &Value, request: &HostedPromptRequest) -> Result<Option<usize>> {
    if value.is_null() {
        return Ok(None);
    }
    if let Some(index) = value.as_u64() {
        return validate_index(index as usize, request);
    }
    if let Some(index) = value.get("index").and_then(Value::as_u64) {
        return validate_index(index as usize, request);
    }
    // Bare strings deliberately stay label-only: letting a label also match an
    // id would make a reworded label silently resolve to the wrong row.
    if let Some(id) = value.get("value").and_then(Value::as_str) {
        let index = request
            .items
            .iter()
            .position(|item| item.value == id)
            .ok_or_else(|| StackError::InvalidParam {
                field: "init",
                reason: format!("selection `{id}` does not match any option"),
            })?;
        return Ok(Some(index));
    }
    if let Some(label) = value.as_str() {
        let index = request
            .items
            .iter()
            .position(|item| item.label == label)
            .ok_or_else(|| StackError::InvalidParam {
                field: "init",
                reason: format!("selection `{label}` does not match any option"),
            })?;
        return Ok(Some(index));
    }
    Err(StackError::InvalidParam {
        field: "init",
        reason: "select input must be an index, value, label, or null".to_owned(),
    })
}

fn validate_index(index: usize, request: &HostedPromptRequest) -> Result<Option<usize>> {
    if index >= request.items.len() {
        return Err(StackError::InvalidParam {
            field: "init",
            reason: format!("selection index {index} is out of range"),
        });
    }
    Ok(Some(index))
}

fn next_input_request_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) as u128;
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    format!("ireq_{nanos:020}_{sequence:010}_{pid:010}")
}
