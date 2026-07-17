use crate::cli::core::OutputFormat;
use crate::config::Config;
use crate::error::Result;

use super::provider_credentials::{
    run_credential_add, run_credential_delete, run_credential_list, run_credential_update,
};
use super::provider_target::{
    run_target_credential_select, run_target_provider_list_active, run_target_provider_set_active,
    run_target_provider_use,
};
use super::{AgentProviderArgs, AgentProviderCommand, AgentProviderCredentialCommand};

pub(super) fn run_agent_provider(args: AgentProviderArgs, output: OutputFormat) -> Result<()> {
    match args.command {
        AgentProviderCommand::Use(args) => {
            let config = Config::load_from_default_path()?;
            run_target_provider_use(
                &config.array.primary_target,
                &args.provider,
                args.model.as_deref(),
                output,
            )
        }
        AgentProviderCommand::SetActive(args) => {
            let config = Config::load_from_default_path()?;
            run_target_provider_set_active(&config.array.primary_target, &args.providers, output)
        }
        AgentProviderCommand::ListActive => {
            let config = Config::load_from_default_path()?;
            run_target_provider_list_active(&config.array.primary_target, false, output)
        }
        AgentProviderCommand::Credential(args) => match args.command {
            AgentProviderCredentialCommand::Add(args) => run_credential_add(args, output),
            AgentProviderCredentialCommand::Update(args) => run_credential_update(args, output),
            AgentProviderCredentialCommand::Select(args) => {
                let config = Config::load_from_default_path()?;
                run_target_credential_select(
                    &config.array.primary_target,
                    &args.provider,
                    &args.alias,
                    output,
                )
            }
            AgentProviderCredentialCommand::List(args) => run_credential_list(args, output),
            AgentProviderCredentialCommand::Delete(args) => run_credential_delete(args, output),
        },
    }
}
