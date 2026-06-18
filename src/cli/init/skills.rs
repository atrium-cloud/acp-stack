use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_installer::{
    ResolvedSkillSource, SOURCE_ANTHROPIC, SOURCE_CUSTOM_GITHUB_PREFIX, SOURCE_OPENAI,
    SkillInstallReport, all_skills_installed, expand_agent_skills_install_dir, install_from_github,
    parse_skill_names, parse_skill_source, resolve_source,
};
use crate::runtime::install::skill_registry::SkillCatalog;

use super::{InitArgs, prompt, prompts_enabled};

#[derive(Debug, Clone)]
pub(super) struct InitSkillInstallPlan {
    pub(super) source: ResolvedSkillSource,
    pub(super) destination_root: PathBuf,
    pub(super) skills: Vec<String>,
}

pub(super) fn prompt_init_skills_if_needed(
    args: &mut InitArgs,
    config: &Config,
    registry: &RegistryCatalog,
) -> Result<()> {
    if args.resume || args.no_skills || args.skills_source.is_some() || !args.skills.is_empty() {
        return Ok(());
    }
    let interactive = prompts_enabled(args);
    if !interactive || !args.prompt_skills || agent_install_dir(config, registry).is_none() {
        return Ok(());
    }

    #[derive(Clone, PartialEq, Eq)]
    enum SkillSourceChoice {
        OpenAi,
        Anthropic,
        CustomGithub,
        Skip,
    }
    let choice = prompt::select(
        interactive,
        "Select Agent Skills source",
        &[
            (
                SkillSourceChoice::OpenAi,
                "OpenAI".to_owned(),
                String::new(),
            ),
            (
                SkillSourceChoice::Anthropic,
                "Anthropic".to_owned(),
                String::new(),
            ),
            (
                SkillSourceChoice::CustomGithub,
                "Custom GitHub owner/org".to_owned(),
                String::new(),
            ),
            (SkillSourceChoice::Skip, "Skip".to_owned(), String::new()),
        ],
    )?;
    let source = match choice {
        None | Some(SkillSourceChoice::Skip) => {
            args.no_skills = true;
            return Ok(());
        }
        Some(SkillSourceChoice::OpenAi) => SOURCE_OPENAI.to_owned(),
        Some(SkillSourceChoice::Anthropic) => SOURCE_ANTHROPIC.to_owned(),
        Some(SkillSourceChoice::CustomGithub) => {
            match prompt::text(interactive, "GitHub owner/org for <owner>/skills", true)? {
                Some(owner) if !owner.trim().is_empty() => {
                    format!("{SOURCE_CUSTOM_GITHUB_PREFIX}{}", owner.trim())
                }
                _ => {
                    args.no_skills = true;
                    return Ok(());
                }
            }
        }
    };
    args.skills_source = Some(source);

    let skills = prompt::text(
        interactive,
        "skills (comma-separated dash-case, blank to skip)",
        false,
    )?
    .unwrap_or_default();
    let skills = skills.trim();
    if skills.is_empty() {
        args.no_skills = true;
        args.skills_source = None;
        return Ok(());
    }
    args.skills = vec![skills.to_owned()];
    Ok(())
}

pub(super) fn resolve_skill_install_plan(
    args: &InitArgs,
    home: &Path,
    config: &Config,
    registry: &RegistryCatalog,
    skill_catalog: &SkillCatalog,
) -> Result<Option<InitSkillInstallPlan>> {
    if args.no_skills {
        return Ok(None);
    }
    if args.skills_source.is_none() && args.skills.is_empty() {
        return Ok(None);
    }
    let source_arg = args
        .skills_source
        .as_deref()
        .ok_or(StackError::MissingField {
            field: "--skills-source",
        })?;
    if args.skills.is_empty() {
        return Err(StackError::MissingField { field: "--skills" });
    }

    let install_dir =
        agent_install_dir(config, registry).ok_or_else(|| StackError::SkillInstallFailed {
            reason: format!(
                "agent `{}` does not declare an Agent Skills install directory",
                config.agent.id
            ),
        })?;
    let destination_root = expand_agent_skills_install_dir(home, install_dir)?;
    let selection = parse_skill_source(source_arg)?;
    let source = resolve_source(&selection, skill_catalog)?;
    let skills = parse_skill_names(&args.skills)?;
    Ok(Some(InitSkillInstallPlan {
        source,
        destination_root,
        skills,
    }))
}

pub(super) fn skill_install_postcondition_holds(plan: &InitSkillInstallPlan) -> bool {
    all_skills_installed(&plan.destination_root, &plan.skills)
}

pub(super) fn install_init_skills(plan: &InitSkillInstallPlan) -> Result<SkillInstallReport> {
    install_from_github(&plan.source, &plan.destination_root, &plan.skills)
}

fn agent_install_dir<'a>(config: &Config, registry: &'a RegistryCatalog) -> Option<&'a str> {
    let entry = registry.lookup(&config.agent.id)?;
    if !entry.supports_agent_skills {
        return None;
    }
    entry.agent_skills_install_dir.as_deref()
}
