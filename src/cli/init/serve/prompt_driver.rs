use super::*;

pub(super) struct SessionPromptDriver {
    pub(super) session: Arc<HostedInitSession>,
}

impl HostedPromptDriver for SessionPromptDriver {
    fn select(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<usize>>> {
        let Some(value) = self.session.request_input(request.clone())? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let selection = parse_optional_index(&value, &request)?;
        Ok(HostedPromptOutcome::Handled(selection))
    }

    fn confirm(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<bool>> {
        let Some(value) = self.session.request_input(request.clone())? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let Some(value) = value.as_bool() else {
            return Err(StackError::InvalidParam {
                field: "init",
                reason: "confirm input must be a boolean".to_owned(),
            });
        };
        Ok(HostedPromptOutcome::Handled(value))
    }

    fn text(&self, request: HostedPromptRequest) -> Result<HostedPromptOutcome<Option<String>>> {
        let Some(value) = self.session.request_input(request)? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
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
        let Some(value) = self.session.request_input(request)? else {
            return Ok(HostedPromptOutcome::Unhandled);
        };
        let selection: NativeConfigSelection =
            serde_json::from_value(value).map_err(|_| StackError::InvalidParam {
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

    fn progress(&self, message: String) {
        self.session
            .push_event("progress", json!({ "message": message }));
    }

    fn result(&self, payload: Value) {
        self.session.set_result(payload);
    }
}

pub(super) fn should_handle_hosted_prompt(request: &HostedPromptRequest) -> bool {
    match request.style {
        HostedPromptStyle::Select | HostedPromptStyle::SearchableSelect => {
            request.prompt == "Agent"
                || request.prompt.starts_with("provider for ")
                || request.prompt.starts_with("select ")
        }
        HostedPromptStyle::Confirm => {
            request.prompt.contains("configure it as a custom provider")
                || request.prompt == "run testflight now?"
        }
        HostedPromptStyle::Text => matches!(
            request.prompt.as_str(),
            "provider id" | "provider-name" | "base-url" | "api-key-ref" | "model"
        ),
        HostedPromptStyle::Password => true,
        HostedPromptStyle::NativeConfigReview => request.inspection.is_some(),
    }
}

pub(super) fn public_input_request(request: HostedPromptRequest) -> PublicInputRequest {
    PublicInputRequest {
        request_id: next_input_request_id(),
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
                label: item.label,
                hint: item.hint,
            })
            .collect(),
        inspection: request.inspection,
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
        reason: "select input must be an index, label, or null".to_owned(),
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
