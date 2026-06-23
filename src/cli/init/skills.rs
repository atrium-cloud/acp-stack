use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::init_runner::step_kind;
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_installer::{
    ANTHROPIC_SKILLS_SOURCE_ID, OPENAI_PLUGINS_SOURCE_ID, OPENAI_SKILLS_SOURCE_ID,
    ResolvedSkillSource, SOURCE_ANTHROPIC, SOURCE_CUSTOM_GITHUB_PREFIX, SOURCE_OPENAI,
    SkillInstallReport, all_skills_installed, expand_agent_skills_install_dir, install_from_github,
    install_plugins_from_github, parse_plugin_names, parse_plugin_source, parse_skill_names,
    parse_skill_source, resolve_plugin_source, resolve_source,
};
use crate::runtime::install::skill_registry::SkillCatalog;
use crate::state::InitStepRecord;

use super::{InitArgs, prompt, prompts_enabled};

#[derive(Debug, Clone)]
pub(super) struct InitSkillInstallPlan {
    pub(super) destination_root: PathBuf,
    pub(super) skills: Option<InitSkillSelectionPlan>,
    pub(super) plugins: Option<InitPluginSelectionPlan>,
}

#[derive(Debug, Clone)]
pub(super) struct InitSkillSelectionPlan {
    pub(super) source: ResolvedSkillSource,
    pub(super) skills: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct InitPluginSelectionPlan {
    pub(super) source: ResolvedSkillSource,
    pub(super) plugins: Vec<String>,
}

pub(super) fn prompt_init_skills_if_needed(
    args: &mut InitArgs,
    config: &Config,
    registry: &RegistryCatalog,
    skill_catalog: &SkillCatalog,
) -> Result<()> {
    if args.resume
        || args.no_skills
        || args.skills_source.is_some()
        || !args.skills.is_empty()
        || args.plugins_source.is_some()
        || !args.plugins.is_empty()
    {
        return Ok(());
    }
    let interactive = prompts_enabled(args);
    if !interactive || !args.prompt_skills || agent_install_dir(config, registry).is_none() {
        return Ok(());
    }

    #[derive(Clone, PartialEq, Eq)]
    enum SkillSourceChoice {
        OpenAiSkills,
        AnthropicSkills,
        OpenAiPlugins,
        CustomGithub,
        Skip,
    }
    let choice = prompt::select(
        interactive,
        "Select Agent Skills source",
        &[
            (
                SkillSourceChoice::OpenAiSkills,
                "OpenAI skills".to_owned(),
                String::new(),
            ),
            (
                SkillSourceChoice::AnthropicSkills,
                "Anthropic skills".to_owned(),
                String::new(),
            ),
            (
                SkillSourceChoice::OpenAiPlugins,
                "OpenAI plugin bundles".to_owned(),
                "install bundled skills".to_owned(),
            ),
            (
                SkillSourceChoice::CustomGithub,
                "Custom GitHub owner/org".to_owned(),
                String::new(),
            ),
            (SkillSourceChoice::Skip, "Skip".to_owned(), String::new()),
        ],
    )?;
    match choice {
        None | Some(SkillSourceChoice::Skip) => {
            args.no_skills = true;
            Ok(())
        }
        Some(SkillSourceChoice::OpenAiSkills) => prompt_indexed_skills(
            interactive,
            args,
            skill_catalog,
            SOURCE_OPENAI,
            OPENAI_SKILLS_SOURCE_ID,
        ),
        Some(SkillSourceChoice::AnthropicSkills) => prompt_indexed_skills(
            interactive,
            args,
            skill_catalog,
            SOURCE_ANTHROPIC,
            ANTHROPIC_SKILLS_SOURCE_ID,
        ),
        Some(SkillSourceChoice::OpenAiPlugins) => {
            let plugins = indexed_plugin_names(skill_catalog, OPENAI_PLUGINS_SOURCE_ID);
            let selected = prompt_indexed_names(interactive, "Select plugin bundle", &plugins)?;
            if selected.is_empty() {
                args.no_skills = true;
                return Ok(());
            }
            args.plugins_source = Some(SOURCE_OPENAI.to_owned());
            args.plugins = selected;
            Ok(())
        }
        Some(SkillSourceChoice::CustomGithub) => {
            let source =
                match prompt::text(interactive, "GitHub owner/org for <owner>/skills", true)? {
                    Some(owner) if !owner.trim().is_empty() => {
                        format!("{SOURCE_CUSTOM_GITHUB_PREFIX}{}", owner.trim())
                    }
                    _ => {
                        args.no_skills = true;
                        return Ok(());
                    }
                };
            args.skills_source = Some(source);
            prompt_manual_skill_names(interactive, args)
        }
    }
}

fn prompt_indexed_skills(
    interactive: bool,
    args: &mut InitArgs,
    skill_catalog: &SkillCatalog,
    source_flag: &str,
    source_id: &str,
) -> Result<()> {
    args.skills_source = Some(source_flag.to_owned());
    let skills = indexed_skill_names(skill_catalog, source_id);
    let selected = if skills.is_empty() {
        prompt_manual_skill_names(interactive, args)?;
        return Ok(());
    } else {
        prompt_indexed_names(interactive, "Select skill", &skills)?
    };
    if selected.is_empty() {
        args.no_skills = true;
        args.skills_source = None;
        return Ok(());
    }
    args.skills = selected;
    Ok(())
}

fn prompt_manual_skill_names(interactive: bool, args: &mut InitArgs) -> Result<()> {
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

#[derive(Clone, PartialEq, Eq)]
enum IndexedNameChoice {
    Name(String),
    Done,
}

fn prompt_indexed_names(interactive: bool, label: &str, names: &[String]) -> Result<Vec<String>> {
    let mut selected = Vec::new();
    let mut remaining = names.to_vec();
    loop {
        let mut items: Vec<(IndexedNameChoice, String, String)> = remaining
            .iter()
            .map(|name| {
                (
                    IndexedNameChoice::Name(name.clone()),
                    name.clone(),
                    String::new(),
                )
            })
            .collect();
        items.push((IndexedNameChoice::Done, "Done".to_owned(), String::new()));
        match prompt::searchable_select(interactive, label, &items)? {
            Some(IndexedNameChoice::Name(name)) => {
                remaining.retain(|candidate| candidate != &name);
                selected.push(name);
                if remaining.is_empty() {
                    return Ok(selected);
                }
            }
            Some(IndexedNameChoice::Done) | None => return Ok(selected),
        }
    }
}

fn indexed_skill_names(skill_catalog: &SkillCatalog, source_id: &str) -> Vec<String> {
    let Some(source) = skill_catalog.lookup(source_id) else {
        return Vec::new();
    };
    let mut names: Vec<String> = source
        .directories
        .iter()
        .filter(|directory| directory.installable)
        .flat_map(|directory| directory.indexed_names.iter().cloned())
        .collect();
    names.sort();
    names.dedup();
    names
}

fn indexed_plugin_names(skill_catalog: &SkillCatalog, source_id: &str) -> Vec<String> {
    let Some(source) = skill_catalog.lookup(source_id) else {
        return Vec::new();
    };
    let mut names: Vec<String> = source
        .plugin_bundles
        .iter()
        .flat_map(|plugin_bundle| plugin_bundle.installable_plugins.iter().cloned())
        .collect();
    names.sort();
    names.dedup();
    names
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
    let skill_requested = args.skills_source.is_some() || !args.skills.is_empty();
    let plugin_requested = args.plugins_source.is_some() || !args.plugins.is_empty();
    if !skill_requested && !plugin_requested {
        return Ok(None);
    }

    let install_dir =
        agent_install_dir(config, registry).ok_or_else(|| StackError::SkillInstallFailed {
            reason: format!(
                "agent `{}` does not declare an Agent Skills install directory",
                config.agent.id
            ),
        })?;
    let destination_root = expand_agent_skills_install_dir(home, install_dir)?;

    let skills = if skill_requested {
        let source_arg = args
            .skills_source
            .as_deref()
            .ok_or(StackError::MissingField {
                field: "--skills-source",
            })?;
        if args.skills.is_empty() {
            return Err(StackError::MissingField { field: "--skills" });
        }
        let selection = parse_skill_source(source_arg)?;
        let source = resolve_source(&selection, skill_catalog)?;
        let skills = parse_skill_names(&args.skills)?;
        Some(InitSkillSelectionPlan { source, skills })
    } else {
        None
    };

    let plugins = if plugin_requested {
        let source_arg = args
            .plugins_source
            .as_deref()
            .ok_or(StackError::MissingField {
                field: "--plugins-source",
            })?;
        if args.plugins.is_empty() {
            return Err(StackError::MissingField { field: "--plugins" });
        }
        let selection = parse_plugin_source(source_arg)?;
        let source = resolve_plugin_source(&selection, skill_catalog)?;
        let plugins = parse_plugin_names(&args.plugins)?;
        Some(InitPluginSelectionPlan { source, plugins })
    } else {
        None
    };

    Ok(Some(InitSkillInstallPlan {
        destination_root,
        skills,
        plugins,
    }))
}

pub(super) fn skill_install_postcondition_holds(
    plan: &InitSkillInstallPlan,
    prior_steps: &[InitStepRecord],
) -> bool {
    let skills_installed = plan
        .skills
        .as_ref()
        .is_none_or(|skills| all_skills_installed(&plan.destination_root, &skills.skills));
    let plugins_installed = plan.plugins.as_ref().is_none_or(|plugins| {
        plugin_skills_installed_from_prior_report(plan, plugins, prior_steps)
    });
    skills_installed && plugins_installed
}

pub(super) fn install_init_skills(plan: &InitSkillInstallPlan) -> Result<Vec<SkillInstallReport>> {
    let mut reports = Vec::new();
    if let Some(skills) = &plan.skills {
        reports.push(install_from_github(
            &skills.source,
            &plan.destination_root,
            &skills.skills,
        )?);
    }
    if let Some(plugins) = &plan.plugins {
        reports.push(install_plugins_from_github(
            &plugins.source,
            &plan.destination_root,
            &plugins.plugins,
        )?);
    }
    Ok(reports)
}

fn plugin_skills_installed_from_prior_report(
    plan: &InitSkillInstallPlan,
    plugins: &InitPluginSelectionPlan,
    prior_steps: &[InitStepRecord],
) -> bool {
    let Some(payload) = prior_skill_install_payload(prior_steps) else {
        return false;
    };
    if let Some(requested_plugins) = payload.requested_plugins
        && requested_plugins != plugins.plugins
    {
        return false;
    }
    let Some(report) = payload
        .reports
        .iter()
        .find(|report| report.source_id == plugins.source.id)
    else {
        return false;
    };
    if report.destination_root != plan.destination_root {
        return false;
    }
    let skill_names = report
        .installed
        .iter()
        .chain(report.skipped.iter())
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    !skill_names.is_empty() && all_skills_installed(&plan.destination_root, &skill_names)
}

fn prior_skill_install_payload(prior_steps: &[InitStepRecord]) -> Option<SkillInstallPayload> {
    let step = prior_steps
        .iter()
        .find(|step| step.kind == step_kind::AGENT_SKILLS_INSTALL)?;
    parse_skill_install_payload(&step.payload_json).ok()
}

struct SkillInstallPayload {
    reports: Vec<SkillInstallReport>,
    requested_plugins: Option<Vec<String>>,
}

fn parse_skill_install_payload(payload: &str) -> serde_json::Result<SkillInstallPayload> {
    let value: serde_json::Value = serde_json::from_str(payload)?;
    if let serde_json::Value::Object(object) = &value
        && let Some(reports) = object.get("reports")
    {
        let requested_plugins = object
            .get("request")
            .and_then(|request| request.get("plugins"))
            .map(|plugins| serde_json::from_value(plugins.clone()))
            .transpose()?;
        return Ok(SkillInstallPayload {
            reports: serde_json::from_value(reports.clone())?,
            requested_plugins,
        });
    }
    Ok(SkillInstallPayload {
        reports: serde_json::from_value(value)?,
        requested_plugins: None,
    })
}

fn agent_install_dir<'a>(config: &Config, registry: &'a RegistryCatalog) -> Option<&'a str> {
    let entry = registry.lookup(&config.agent.id)?;
    if !entry.supports_agent_skills {
        return None;
    }
    entry.agent_skills_install_dir.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::install::skill_installer::SkillInstallEntry;

    fn resolved_source(id: &str) -> ResolvedSkillSource {
        ResolvedSkillSource {
            id: id.to_owned(),
            name: id.to_owned(),
            owner: "openai".to_owned(),
            repo: "plugins".to_owned(),
            url: "https://github.com/openai/plugins".to_owned(),
            branch: "main".to_owned(),
            verified_commit: None,
            indexed_commit: None,
            descriptor: "SKILL.md".to_owned(),
            directories: Vec::new(),
            plugin_bundles: Vec::new(),
        }
    }

    fn plugin_plan(destination_root: std::path::PathBuf) -> InitSkillInstallPlan {
        InitSkillInstallPlan {
            destination_root,
            skills: None,
            plugins: Some(InitPluginSelectionPlan {
                source: resolved_source(OPENAI_PLUGINS_SOURCE_ID),
                plugins: vec!["github".to_owned()],
            }),
        }
    }

    fn direct_skill_plan(destination_root: std::path::PathBuf) -> InitSkillInstallPlan {
        InitSkillInstallPlan {
            destination_root,
            skills: Some(InitSkillSelectionPlan {
                source: resolved_source(ANTHROPIC_SKILLS_SOURCE_ID),
                skills: vec!["docx".to_owned()],
            }),
            plugins: None,
        }
    }

    fn prior_skill_step(payload_json: String) -> InitStepRecord {
        InitStepRecord {
            id: "step_1".to_owned(),
            run_id: "run_1".to_owned(),
            ordinal: 9,
            kind: step_kind::AGENT_SKILLS_INSTALL.to_owned(),
            status: "succeeded".to_owned(),
            started_at: None,
            finished_at: None,
            log_dir: None,
            error_kind: None,
            error_detail: None,
            payload_json,
        }
    }

    fn install_skill_dir(root: &std::path::Path, name: &str) {
        let skill_dir = root.join(name);
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(skill_dir.join("SKILL.md"), "# Skill\n").expect("descriptor");
    }

    fn plugin_report(destination_root: &std::path::Path) -> SkillInstallReport {
        SkillInstallReport {
            source_id: OPENAI_PLUGINS_SOURCE_ID.to_owned(),
            destination_root: destination_root.to_path_buf(),
            installed: vec![SkillInstallEntry {
                name: "github".to_owned(),
                path: destination_root.join("github"),
            }],
            skipped: Vec::new(),
        }
    }

    #[test]
    fn plugin_postcondition_uses_prior_report_object_payload() {
        let home = tempfile::tempdir().expect("home");
        let destination = home
            .path()
            .canonicalize()
            .expect("canonical home")
            .join("skills");
        install_skill_dir(&destination, "github");
        let payload = serde_json::json!({
            "request": {
                "skills": [],
                "plugins": ["github"]
            },
            "reports": [plugin_report(&destination)],
            "resume": { "verified": true }
        })
        .to_string();

        assert!(skill_install_postcondition_holds(
            &plugin_plan(destination),
            &[prior_skill_step(payload)]
        ));
    }

    #[test]
    fn plugin_postcondition_accepts_legacy_report_array_payload() {
        let home = tempfile::tempdir().expect("home");
        let destination = home
            .path()
            .canonicalize()
            .expect("canonical home")
            .join("skills");
        install_skill_dir(&destination, "github");
        let payload =
            serde_json::to_string(&vec![plugin_report(&destination)]).expect("legacy payload");

        assert!(skill_install_postcondition_holds(
            &plugin_plan(destination),
            &[prior_skill_step(payload)]
        ));
    }

    #[test]
    fn plugin_postcondition_rejects_mismatched_plugin_request() {
        let home = tempfile::tempdir().expect("home");
        let destination = home
            .path()
            .canonicalize()
            .expect("canonical home")
            .join("skills");
        install_skill_dir(&destination, "github");
        let payload = serde_json::json!({
            "request": {
                "skills": [],
                "plugins": ["cloudflare"]
            },
            "reports": [plugin_report(&destination)]
        })
        .to_string();

        assert!(!skill_install_postcondition_holds(
            &plugin_plan(destination),
            &[prior_skill_step(payload)]
        ));
    }

    #[test]
    fn plugin_postcondition_rejects_missing_expanded_skill() {
        let home = tempfile::tempdir().expect("home");
        let destination = home
            .path()
            .canonicalize()
            .expect("canonical home")
            .join("skills");
        let payload = serde_json::json!({ "reports": [plugin_report(&destination)] }).to_string();

        assert!(!skill_install_postcondition_holds(
            &plugin_plan(destination),
            &[prior_skill_step(payload)]
        ));
    }

    #[test]
    fn direct_skill_postcondition_does_not_require_prior_payload() {
        let home = tempfile::tempdir().expect("home");
        let destination = home
            .path()
            .canonicalize()
            .expect("canonical home")
            .join("skills");
        install_skill_dir(&destination, "docx");

        assert!(skill_install_postcondition_holds(
            &direct_skill_plan(destination),
            &[]
        ));
    }
}
