use super::*;
use crate::cli::init::skills::InitSkillInstallPlan;
use crate::secrets::{SharedSecretStore, lock_shared_secret_store, new_shared_secret_store};

/// Everything the tracked phases inherit from the untracked preflight.
pub(super) struct InitSetup {
    pub(super) args: InitArgs,
    pub(super) output_mode: InitOutputMode,
    pub(super) home: PathBuf,
    pub(super) config_path: PathBuf,
    pub(super) state_path: PathBuf,
    pub(super) registry: RegistryCatalog,
    pub(super) config: Config,
    pub(super) config_status: &'static str,
    pub(super) creating_config: bool,
    pub(super) legacy_auth: Option<crate::config::LegacyAuthConfig>,
    pub(super) agent_env_collection: AgentEnvCollection,
    pub(super) store: StateStore,
    pub(super) init_run: crate::state::InitRunRecord,
    pub(super) prior_init_steps: Vec<crate::state::InitStepRecord>,
    pub(super) init_native_config_record:
        Option<crate::runtime::agent::native_config_import::NativeConfigOperationRecord>,
    pub(super) edge_requested: bool,
    /// `selected_agent.is_some()`; the selection itself borrows the registry, so only
    /// this fact outlives staging.
    pub(super) agent_selected: bool,
    pub(super) skill_install_plan: Option<InitSkillInstallPlan>,
    pub(super) mutation: crate::fs_util::AgentConfigMutationFileLock,
    /// The serve process's shared store handle, hoisted to `acps init serve`
    /// start so bootstrap HTTP handlers and the wizard write through one
    /// handle. Absent for terminal/dev init, which opens its own below.
    pub(super) shared_secret_store: Option<SharedSecretStore>,
}

/// The mutable frame the tracked steps thread through. Field order IS drop order:
/// `key_handover` must render before the config-mutation lock is released.
pub(super) struct InitFlow {
    pub(super) key_handover: KeyHandover,
    /// Shared with the bootstrap credential-deposit handler in serve mode; the
    /// wizard locks it per consumer call and never across an `.await` (the
    /// wizard thread is synchronous) so the async handler can always get in.
    pub(super) secret_store: SharedSecretStore,
    pub(super) handoff_context: InitHandoffContext,
    pub(super) auth_status: &'static str,
    pub(super) args: InitArgs,
    pub(super) output_mode: InitOutputMode,
    pub(super) home: PathBuf,
    pub(super) config_path: PathBuf,
    pub(super) state_path: PathBuf,
    pub(super) registry: RegistryCatalog,
    pub(super) config: Config,
    pub(super) config_status: &'static str,
    pub(super) creating_config: bool,
    pub(super) legacy_auth: Option<crate::config::LegacyAuthConfig>,
    pub(super) agent_env_collection: AgentEnvCollection,
    pub(super) store: StateStore,
    pub(super) init_run: crate::state::InitRunRecord,
    pub(super) prior_init_steps: Vec<crate::state::InitStepRecord>,
    pub(super) init_native_config_record:
        Option<crate::runtime::agent::native_config_import::NativeConfigOperationRecord>,
    pub(super) edge_requested: bool,
    pub(super) agent_selected: bool,
    pub(super) skill_install_plan: Option<InitSkillInstallPlan>,
    pub(super) install_outcome: Option<InstallerOutcome>,
    pub(super) skill_install_reports: Vec<SkillInstallReport>,
    pub(super) materialize_report:
        Option<crate::runtime::workspace_sources::workspace_init::MaterializeReport>,
    pub(super) probed_capabilities: Option<crate::runtime::agent::acp_bridge::AgentCapabilitiesDto>,
    pub(super) ignored_features: Vec<crate::runtime::agent::acp_bridge::IgnoredFeature>,
    pub(super) provisioned_agent_configs:
        Vec<crate::runtime::agent::agent_headless_config::ProvisionedAgentConfig>,
    pub(super) provisioned_edge_artifacts: Vec<crate::edge::GeneratedCloudflareArtifact>,
    /// Held, never read; released on drop, after the handover has rendered.
    pub(super) _mutation: crate::fs_util::AgentConfigMutationFileLock,
}

impl InitFlow {
    /// Opens the secret store and arms the key handover. Nothing fallible may be
    /// hoisted above it, since from here a failure return renders the failure frame.
    pub(super) fn begin(setup: InitSetup) -> Result<Self> {
        let auth_status: &'static str = "preserved existing API keys";
        let mut key_handover = KeyHandover {
            keys: None,
            output_mode: setup.output_mode,
            failure_context: None,
            auth_ready: false,
            emitted: false,
        };
        let secret_store = match setup.shared_secret_store {
            Some(handle) => handle,
            None => new_shared_secret_store(
                SecretStore::open_or_create(&setup.home)
                    .or_else(|error| finalize_failure(&setup.store, &setup.init_run, error))?,
            ),
        };
        let handoff_context = InitHandoffContext {
            config_path: setup.config_path.clone(),
            state_path: setup.state_path.clone(),
            secret_store_path: lock_shared_secret_store(&secret_store)
                .store_path()
                .to_path_buf(),
            age_key_path: age_key_path(&setup.home),
            agent_id: setup.config.agent.id.clone(),
            agent_name: setup.config.agent.name.clone(),
            native_config_import: None,
            ignored_features: Vec::new(),
            deps_apply_run_id: None,
        };
        key_handover.failure_context = Some(handoff_context.clone());
        Ok(Self {
            key_handover,
            secret_store,
            handoff_context,
            auth_status,
            args: setup.args,
            output_mode: setup.output_mode,
            home: setup.home,
            config_path: setup.config_path,
            state_path: setup.state_path,
            registry: setup.registry,
            config: setup.config,
            config_status: setup.config_status,
            creating_config: setup.creating_config,
            legacy_auth: setup.legacy_auth,
            agent_env_collection: setup.agent_env_collection,
            store: setup.store,
            init_run: setup.init_run,
            prior_init_steps: setup.prior_init_steps,
            init_native_config_record: setup.init_native_config_record,
            edge_requested: setup.edge_requested,
            agent_selected: setup.agent_selected,
            skill_install_plan: setup.skill_install_plan,
            install_outcome: None,
            skill_install_reports: Vec::new(),
            materialize_report: None,
            probed_capabilities: None,
            ignored_features: Vec::new(),
            provisioned_agent_configs: Vec::new(),
            provisioned_edge_artifacts: Vec::new(),
            _mutation: setup.mutation,
        })
    }
}
