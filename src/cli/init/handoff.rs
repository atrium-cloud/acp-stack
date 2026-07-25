use super::*;

const KEY_HANDOVER_PRINTED_EVENT: &str = "auth.keys_handover_printed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitOutputMode {
    Text,
    HandoffJson,
    Hosted,
}

impl InitOutputMode {
    pub(super) fn is_text(self) -> bool {
        matches!(self, Self::Text)
    }

    pub(super) fn is_handoff_json(self) -> bool {
        matches!(self, Self::HandoffJson)
    }

    pub(super) fn is_hosted(self) -> bool {
        matches!(self, Self::Hosted)
    }

    pub(super) fn is_machine_handoff(self) -> bool {
        matches!(self, Self::HandoffJson | Self::Hosted)
    }
}

#[derive(Debug, Clone)]
pub(super) struct InitHandoffContext {
    pub(super) config_path: PathBuf,
    pub(super) state_path: PathBuf,
    pub(super) secret_store_path: PathBuf,
    pub(super) age_key_path: PathBuf,
    pub(super) agent_id: String,
    pub(super) agent_name: String,
    pub(super) native_config_import:
        Option<crate::runtime::agent::native_config_import::NativeConfigOperation>,
}

/// Drop guard that performs the session/admin key handover as the very last
/// thing the operator sees. Holding the plaintext across the whole run (instead
/// of printing at generation time) keeps the keys from scrolling off-screen
/// behind install/workspace/testflight output; rendering on Drop means a fresh
/// run that fails AFTER key generation still surfaces the otherwise
/// unrecoverable, non-regenerable admin key before init exits. In handoff JSON
/// mode, preserved keys are reported without reprinting plaintext material.
pub(super) struct KeyHandover {
    pub(super) keys: Option<FreshKeys>,
    pub(super) output_mode: InitOutputMode,
    pub(super) failure_context: Option<InitHandoffContext>,
    pub(super) auth_ready: bool,
    pub(super) emitted: bool,
}

impl Drop for KeyHandover {
    fn drop(&mut self) {
        if self.emitted {
            return;
        }
        match self.output_mode {
            InitOutputMode::Text => {
                self.print_text();
            }
            InitOutputMode::HandoffJson => {
                self.print_failed_json();
            }
            InitOutputMode::Hosted => {
                self.emit_failed_hosted_json();
            }
        }
    }
}

impl KeyHandover {
    fn print_text(&mut self) -> Option<(String, String)> {
        let keys = self.keys.take()?;
        println!("---");
        println!("session key: {}", keys.session_value.as_str());
        println!("admin key: {}", keys.admin_value.as_str());
        println!(
            "save the admin key now; it is never regenerable. use `acps reset --yes` to rotate it."
        );
        println!("---");
        Some(("session".to_owned(), "admin".to_owned()))
    }

    pub(super) fn print_and_record(&mut self, store: &StateStore, run_id: &str) -> Result<()> {
        if self.keys.is_some() {
            self.record(store, run_id)?;
            self.print_text();
        }
        Ok(())
    }

    pub(super) fn record(&self, store: &StateStore, run_id: &str) -> Result<()> {
        if self.keys.is_none() {
            return Ok(());
        }
        store.append_event_with_source(
            "info",
            KEY_HANDOVER_PRINTED_EVENT,
            crate::state::EVENT_SOURCE_CLI,
            "session and admin API keys were shown to the operator",
            &serde_json::json!({
                "init_run_id": run_id,
                "key_kinds": ["session", "admin"],
            })
            .to_string(),
        )?;
        Ok(())
    }

    pub(super) fn print_handoff_json(
        &mut self,
        status: &'static str,
        context: &InitHandoffContext,
    ) -> Result<()> {
        let payload = init_handoff_payload(status, context, self.keys.as_ref());
        let rendered =
            serde_json::to_string_pretty(&payload).map_err(|source| StackError::ServeIo {
                source: std::io::Error::other(format!("serialize init handoff JSON: {source}")),
            })?;
        println!("{rendered}");
        self.keys.take();
        self.emitted = true;
        Ok(())
    }

    pub(super) fn emit_handoff_payload(
        &mut self,
        status: &'static str,
        context: &InitHandoffContext,
    ) {
        let payload = init_handoff_payload(status, context, self.keys.as_ref());
        prompt::emit_result(payload);
        self.keys.take();
        self.emitted = true;
    }

    fn print_failed_json(&mut self) {
        let Some(context) = self.failure_context.as_ref() else {
            return;
        };
        if !self.auth_ready {
            return;
        }
        let payload = init_handoff_payload("failed", context, self.keys.as_ref());
        match serde_json::to_string_pretty(&payload) {
            Ok(rendered) => println!("{rendered}"),
            Err(error) => eprintln!("failed to serialize init handoff JSON: {error}"),
        }
        self.keys.take();
        self.emitted = true;
    }

    fn emit_failed_hosted_json(&mut self) {
        let Some(context) = self.failure_context.as_ref() else {
            return;
        };
        if !self.auth_ready {
            return;
        }
        let payload = init_handoff_payload("failed", context, self.keys.as_ref());
        prompt::emit_result(payload);
        self.keys.take();
        self.emitted = true;
    }
}

fn init_handoff_payload(
    status: &'static str,
    context: &InitHandoffContext,
    fresh_keys: Option<&FreshKeys>,
) -> serde_json::Value {
    let generated_keys = if fresh_keys.is_some() {
        serde_json::json!(["session", "admin"])
    } else {
        serde_json::json!([])
    };
    let preserved_keys = if fresh_keys.is_some() {
        serde_json::json!([])
    } else {
        serde_json::json!(["session", "admin"])
    };
    let mut payload = serde_json::json!({
        "status": status,
        "config_path": context.config_path.display().to_string(),
        "state_path": context.state_path.display().to_string(),
        "secret_store_path": context.secret_store_path.display().to_string(),
        "age_key_path": context.age_key_path.display().to_string(),
        "agent": {
            "id": context.agent_id,
            "name": context.agent_name,
        },
        "auth": {
            "generated_keys": generated_keys,
            "preserved_keys": preserved_keys,
        },
    });
    if let Some(keys) = fresh_keys {
        let object = payload
            .as_object_mut()
            .expect("init handoff payload is an object");
        object.insert(
            "session_key".to_owned(),
            serde_json::Value::String(keys.session_value.as_str().to_owned()),
        );
        object.insert(
            "admin_key".to_owned(),
            serde_json::Value::String(keys.admin_value.as_str().to_owned()),
        );
    }
    if let Some(operation) = context.native_config_import.as_ref() {
        let object = payload
            .as_object_mut()
            .expect("init handoff payload is an object");
        object.insert(
            "native_config_import".to_owned(),
            serde_json::json!(operation),
        );
    }
    payload
}
