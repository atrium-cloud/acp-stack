use super::*;

#[derive(Debug, Clone)]
pub(crate) struct CreatedSession {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
}

#[derive(Debug)]
pub(crate) struct PlaceboState {
    pub(crate) args: AcpArgs,
    pub(crate) title: String,
    pub(crate) next_session: u64,
    pub(crate) created_sessions: Vec<CreatedSession>,
    /// Two cancel signals for two fixture modes that never run in one process. The set
    /// is a consumable pending-cancel for the inline finish path: a cancel with no live
    /// turn is claimed by the next turn to complete. The count is epoch state for the
    /// off-loop settle fixture (`--prompt-settle-cancel-after-ms`): a turn captures it at
    /// start and settles cancelled once it climbs, so every turn parked at cancel time
    /// observes one cancel and turns that start later do not inherit it.
    pub(crate) cancelled_sessions: HashSet<String>,
    pub(crate) session_cancels: HashMap<String, u64>,
    pub(crate) model_configured: bool,
    pub(crate) client_capabilities: Option<ClientCapabilities>,
    /// Values applied via `session/set_config_option`, keyed by `(session_id, config_id)` so a set
    /// on one session never leaks into another's advertised currents.
    pub(crate) config_option_values: BTreeMap<(String, String), SessionConfigOptionValue>,
    /// Set once a `session/set_mode` applies the `--expect-mode` value, mirroring
    /// `model_configured` so a test can prove the native mode lane actually fired.
    pub(crate) mode_configured: bool,
}

impl PlaceboState {
    pub(crate) fn new(args: AcpArgs) -> Self {
        let title = env_assertion_title(&args);
        Self {
            args,
            title,
            next_session: 0,
            created_sessions: Vec::new(),
            cancelled_sessions: HashSet::new(),
            session_cancels: HashMap::new(),
            model_configured: false,
            client_capabilities: None,
            config_option_values: BTreeMap::new(),
            mode_configured: false,
        }
    }

    /// The native `modes` block advertised on `session/new`, or `None` when no
    /// `--session-mode` was configured.
    pub(crate) fn session_modes(&self) -> Option<SessionModeState> {
        if self.args.session_mode.is_empty() {
            return None;
        }
        // A current id absent from the available set would advertise invalid
        // state, so an unknown `--session-mode-current` falls back to the first.
        let current = self
            .args
            .session_mode_current
            .clone()
            .filter(|current| self.args.session_mode.contains(current))
            .unwrap_or_else(|| self.args.session_mode[0].clone());
        let available = self
            .args
            .session_mode
            .iter()
            .map(|id| SessionMode::new(id.clone(), id.clone()))
            .collect();
        Some(SessionModeState::new(current, available))
    }

    /// The session's current `session/cancel` count, captured by a turn when it
    /// starts so it can tell an earlier cancel from one aimed at it.
    pub(crate) fn cancel_count(&self, session_id: &str) -> u64 {
        self.session_cancels.get(session_id).copied().unwrap_or(0)
    }

    /// True once a `session/cancel` for this session has arrived since `start` — the
    /// count captured when the turn began. Non-consuming, so concurrently parked
    /// turns each see the same cancel and a later turn starting at a higher `start`
    /// does not inherit it.
    pub(crate) fn cancelled_since(&self, session_id: &str, start: u64) -> bool {
        self.cancel_count(session_id) > start
    }

    pub(crate) fn client_advertised_config_options(&self) -> bool {
        self.client_capabilities
            .as_ref()
            .and_then(|caps| caps.session.as_ref())
            .and_then(|session| session.config_options.as_ref())
            .is_some()
    }

    pub(crate) fn config_options(&self, session_id: &str) -> Option<Vec<SessionConfigOption>> {
        if self.args.model_config_option.is_none()
            && self.args.config_option_select.is_empty()
            && self.args.config_option_boolean.is_empty()
        {
            return None;
        }
        if self.args.require_client_config_options && !self.client_advertised_config_options() {
            return None;
        }
        let mut options = Vec::new();
        if let Some(model) = self.args.model_config_option.as_ref() {
            let current = self
                .applied_value_id(session_id, &self.args.model_config_option_id)
                .unwrap_or_else(|| model.clone());
            options.push(
                SessionConfigOption::select(
                    self.args.model_config_option_id.clone(),
                    "Model",
                    current,
                    vec![SessionConfigSelectOption::new(model.clone(), model.clone())],
                )
                .category(SessionConfigOptionCategory::Model),
            );
        }
        for spec in &self.args.config_option_select {
            let Some((id, category, current, values)) = parse_select_spec(spec) else {
                continue;
            };
            let current = self.applied_value_id(session_id, &id).unwrap_or(current);
            options.push(
                SessionConfigOption::select(
                    id.clone(),
                    id.clone(),
                    current,
                    values
                        .into_iter()
                        .map(|value| SessionConfigSelectOption::new(value.clone(), value))
                        .collect::<Vec<_>>(),
                )
                .category(category),
            );
        }
        for spec in &self.args.config_option_boolean {
            let Some((id, category, default)) = parse_boolean_spec(spec) else {
                continue;
            };
            let current = match self
                .config_option_values
                .get(&(session_id.to_owned(), id.clone()))
            {
                Some(SessionConfigOptionValue::Boolean { value }) => *value,
                _ => default,
            };
            options.push(SessionConfigOption::boolean(id.clone(), id, current).category(category));
        }
        Some(options)
    }

    fn applied_value_id(&self, session_id: &str, id: &str) -> Option<String> {
        match self
            .config_option_values
            .get(&(session_id.to_owned(), id.to_owned()))
        {
            Some(SessionConfigOptionValue::ValueId { value }) => Some(value.0.to_string()),
            _ => None,
        }
    }
}

/// `<id>[@<category>]=<current>:<v1>,<v2>,...`
fn parse_select_spec(
    spec: &str,
) -> Option<(
    String,
    Option<SessionConfigOptionCategory>,
    String,
    Vec<String>,
)> {
    let (head, rest) = spec.split_once('=')?;
    let (id, category) = parse_id_and_category(head);
    let (current, values) = rest.split_once(':')?;
    let values: Vec<String> = values.split(',').map(str::to_owned).collect();
    Some((id, category, current.to_owned(), values))
}

/// `<id>[@<category>]=<true|false>`. A typo'd value yields `None` so the option is skipped and the
/// asserting test fails visibly, rather than silently defaulting to `false`.
fn parse_boolean_spec(spec: &str) -> Option<(String, Option<SessionConfigOptionCategory>, bool)> {
    let (head, value) = spec.split_once('=')?;
    let (id, category) = parse_id_and_category(head);
    let default = match value {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    Some((id, category, default))
}

fn parse_id_and_category(head: &str) -> (String, Option<SessionConfigOptionCategory>) {
    let Some((id, category)) = head.split_once('@') else {
        return (head.to_owned(), None);
    };
    if category.is_empty() {
        return (id.to_owned(), None);
    }
    (
        id.to_owned(),
        serde_json::from_value(serde_json::Value::String(category.to_owned())).ok(),
    )
}

pub(crate) type SharedState = Arc<Mutex<PlaceboState>>;

fn env_assertion_title(args: &AcpArgs) -> String {
    let mut failures = Vec::new();
    for name in &args.assert_env_absent {
        if std::env::var_os(name).is_some() {
            failures.push(format!("env leaked: {name}"));
        }
    }
    for name in &args.assert_env_present {
        if std::env::var_os(name).is_none() {
            failures.push(format!("env missing: {name}"));
        }
    }
    for pair in args.assert_env_not_equals.chunks_exact(2) {
        if std::env::var_os(&pair[0]).as_deref() == Some(std::ffi::OsStr::new(&pair[1])) {
            failures.push(format!("env override: {}", pair[0]));
        }
    }
    if args.assert_env_absent.is_empty()
        && args.assert_env_present.is_empty()
        && args.assert_env_not_equals.is_empty()
    {
        "ACP placebo agent".to_owned()
    } else if failures.is_empty() {
        "env assertions passed".to_owned()
    } else {
        failures.join(", ")
    }
}
