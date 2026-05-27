use std::io::{self, IsTerminal, Write};
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

use super::InitArgs;

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
    if !io::stdin().is_terminal() || agent_install_dir(config, registry).is_none() {
        return Ok(());
    }

    println!("Select Agent Skills source:");
    println!("  1. OpenAI");
    println!("  2. Anthropic");
    println!("  3. Custom GitHub owner/org");
    println!("  4. Skip");
    print!("skills source [1-4, blank to skip]: ");
    flush_stdout()?;
    let source_answer = read_stdin_line()?;
    let source_answer = source_answer.trim();
    if source_answer.is_empty() || source_answer == "4" {
        args.no_skills = true;
        return Ok(());
    }

    args.skills_source = Some(match source_answer {
        "1" => SOURCE_OPENAI.to_owned(),
        "2" => SOURCE_ANTHROPIC.to_owned(),
        "3" => {
            print!("GitHub owner/org for <owner>/skills: ");
            flush_stdout()?;
            let owner = read_stdin_line()?;
            format!("{SOURCE_CUSTOM_GITHUB_PREFIX}{}", owner.trim())
        }
        other => {
            return Err(StackError::InvalidParam {
                field: "skills-source",
                reason: format!("invalid selection `{other}`"),
            });
        }
    });

    print!("skills (comma-separated dash-case, blank to skip): ");
    flush_stdout()?;
    let skills = read_stdin_line()?;
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

fn flush_stdout() -> Result<()> {
    io::stdout()
        .flush()
        .map_err(|source| StackError::ServeIo { source })
}

fn read_stdin_line() -> Result<String> {
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| StackError::ServeIo { source })?;
    Ok(answer)
}
