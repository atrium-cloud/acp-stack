use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Result, StackError};
use crate::runtime::install::agent_registry::RegistryCatalog;
use crate::runtime::install::skill_installer::{
    ResolvedSkillSource, SOURCE_CUSTOM_GITHUB_PREFIX, SkillInstallReport, SkillSourceSelection,
    all_skills_installed, expand_agent_skills_install_dir, install_from_github,
    install_target_names_overlap, parse_skill_names, parse_skill_source, resolve_source,
};
use crate::runtime::install::skill_registry::SkillCatalog;
use crate::state::InitStepRecord;

use super::{InitArgs, prompt, prompts_enabled};

#[derive(Debug, Clone)]
pub(super) struct InitSkillInstallPlan {
    pub(super) destination_root: PathBuf,
    pub(super) selections: Vec<InitSkillSelectionPlan>,
}

#[derive(Debug, Clone)]
pub(super) struct InitSkillSelectionPlan {
    pub(super) source: ResolvedSkillSource,
    pub(super) skills: Vec<String>,
}

pub(super) fn prompt_init_skills_if_needed(
    args: &mut InitArgs,
    config: &Config,
    registry: &RegistryCatalog,
    skill_catalog: &SkillCatalog,
) -> Result<()> {
    if args.resume
        || args.no_skills
        || args.essential_skills
        || args.skills_source.is_some()
        || !args.skills.is_empty()
    {
        return Ok(());
    }
    let interactive = prompts_enabled(args);
    if !interactive || !args.prompt_skills || agent_install_dir(config, registry).is_none() {
        return Ok(());
    }

    #[derive(Clone, PartialEq, Eq)]
    enum SkillSourceChoice {
        Catalog(String),
        CustomGithub,
        Skip,
    }

    let mut choices = skill_catalog
        .sources()
        .iter()
        .filter(|source| !source.indexed_skills.is_empty())
        .map(|source| {
            (
                SkillSourceChoice::Catalog(source.id.clone()),
                source.name.clone(),
                format!("{}/{}", source.owner, source.repo),
            )
        })
        .collect::<Vec<_>>();
    choices.push((
        SkillSourceChoice::CustomGithub,
        "Custom GitHub owner/org".to_owned(),
        String::new(),
    ));
    choices.push((SkillSourceChoice::Skip, "Skip".to_owned(), String::new()));

    match prompt::select(interactive, "Select Agent Skills source", &choices)? {
        None | Some(SkillSourceChoice::Skip) => {
            args.no_skills = true;
            Ok(())
        }
        Some(SkillSourceChoice::Catalog(source_id)) => {
            let source = skill_catalog.lookup(&source_id).ok_or_else(|| {
                StackError::SkillInstallSourceMissing {
                    source_id: source_id.clone(),
                }
            })?;
            args.skills_source = Some(source.alias.clone());
            let selectors = source
                .indexed_skills
                .iter()
                .map(|skill| skill.selector.clone())
                .collect::<Vec<_>>();
            let selected = prompt_indexed_names(interactive, "Select skill", &selectors)?;
            if selected.is_empty() {
                args.no_skills = true;
                args.skills_source = None;
            } else {
                args.skills = selected;
            }
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
        let mut items = remaining
            .iter()
            .map(|name| {
                (
                    IndexedNameChoice::Name(name.clone()),
                    name.clone(),
                    String::new(),
                )
            })
            .collect::<Vec<_>>();
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
    let explicit_requested = args.skills_source.is_some() || !args.skills.is_empty();
    if !explicit_requested && !args.essential_skills {
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

    let mut selections = Vec::new();
    if args.essential_skills {
        selections = essential_skill_selections(skill_catalog)?;
    } else {
        let source_argument = args
            .skills_source
            .as_deref()
            .ok_or(StackError::MissingField {
                field: "--skills-source",
            })?;
        if args.skills.is_empty() {
            return Err(StackError::MissingField { field: "--skills" });
        }
        let selection = parse_skill_source(source_argument, skill_catalog)?;
        let source = resolve_source(&selection, skill_catalog)?;
        let skills = parse_skill_names(&args.skills)?;
        selections.push(InitSkillSelectionPlan { source, skills });
    }
    validate_unique_install_targets(&selections)?;
    Ok(Some(InitSkillInstallPlan {
        destination_root,
        selections,
    }))
}

fn essential_skill_selections(skill_catalog: &SkillCatalog) -> Result<Vec<InitSkillSelectionPlan>> {
    skill_catalog
        .sources()
        .iter()
        .filter(|source| !source.essential_skills.is_empty())
        .map(|source| {
            let resolved = resolve_source(
                &SkillSourceSelection::Official {
                    id: source.id.clone(),
                },
                skill_catalog,
            )?;
            Ok(InitSkillSelectionPlan {
                source: resolved,
                skills: source.essential_skills.clone(),
            })
        })
        .collect()
}

fn validate_unique_install_targets(selections: &[InitSkillSelectionPlan]) -> Result<()> {
    let mut targets = HashSet::<String>::new();
    for selection in selections {
        for selector in &selection.skills {
            let name = if selection.source.catalog_managed {
                selection
                    .source
                    .indexed_skills
                    .iter()
                    .find(|skill| skill.selector == *selector)
                    .map(|skill| skill.name.as_str())
                    .ok_or_else(|| StackError::SkillInstallSkillMissing {
                        source_id: selection.source.id.clone(),
                        skill: selector.clone(),
                    })?
            } else {
                selector.as_str()
            };
            if let Some(existing) = targets
                .iter()
                .find(|existing| install_target_names_overlap(existing, name))
            {
                return Err(StackError::SkillInstallFailed {
                    reason: format!(
                        "selected skills resolve to overlapping install paths `{existing}` and `{name}`"
                    ),
                });
            }
            targets.insert(name.to_owned());
        }
    }
    Ok(())
}

pub(super) fn skill_install_postcondition_holds(
    plan: &InitSkillInstallPlan,
    _prior_steps: &[InitStepRecord],
) -> bool {
    plan.selections.iter().all(|selection| {
        all_skills_installed(&selection.source, &plan.destination_root, &selection.skills)
    })
}

pub(super) fn install_init_skills(plan: &InitSkillInstallPlan) -> Result<Vec<SkillInstallReport>> {
    plan.selections
        .iter()
        .map(|selection| {
            install_from_github(&selection.source, &plan.destination_root, &selection.skills)
        })
        .collect()
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
    use crate::runtime::install::skill_registry::CatalogSkill;

    fn resolved_source(id: &str, selector: &str, name: &str) -> ResolvedSkillSource {
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
            catalog_managed: true,
            directories: Vec::new(),
            indexed_skills: vec![CatalogSkill {
                selector: selector.to_owned(),
                name: name.to_owned(),
                path: format!("plugins/test/skills/{name}"),
            }],
        }
    }

    fn install_skill_dir(root: &Path, name: &str) {
        let skill_dir = root.join(name);
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(skill_dir.join("SKILL.md"), "# Skill\n").expect("descriptor");
    }

    #[test]
    fn postcondition_resolves_qualified_selector_to_install_name() {
        let home = tempfile::tempdir().expect("home");
        let destination = home.path().canonicalize().expect("home").join("skills");
        install_skill_dir(&destination, "android");
        let plan = InitSkillInstallPlan {
            destination_root: destination,
            selections: vec![InitSkillSelectionPlan {
                source: resolved_source("openai-plugins", "zoom/android", "android"),
                skills: vec!["zoom/android".to_owned()],
            }],
        };

        assert!(skill_install_postcondition_holds(&plan, &[]));
    }

    #[test]
    fn duplicate_install_targets_across_sources_are_rejected() {
        let selections = vec![
            InitSkillSelectionPlan {
                source: resolved_source("one", "one/android", "android"),
                skills: vec!["one/android".to_owned()],
            },
            InitSkillSelectionPlan {
                source: resolved_source("two", "two/android", "android"),
                skills: vec!["two/android".to_owned()],
            },
        ];

        let error = validate_unique_install_targets(&selections).expect_err("collision");

        assert!(matches!(error, StackError::SkillInstallFailed { .. }));
    }

    #[test]
    fn nested_install_targets_across_sources_are_rejected() {
        let selections = vec![
            InitSkillSelectionPlan {
                source: resolved_source("one", "zoom-mcp", "zoom-mcp"),
                skills: vec!["zoom-mcp".to_owned()],
            },
            InitSkillSelectionPlan {
                source: resolved_source("two", "zoom-mcp/whiteboard", "zoom-mcp/whiteboard"),
                skills: vec!["zoom-mcp/whiteboard".to_owned()],
            },
        ];

        let error = validate_unique_install_targets(&selections).expect_err("nested collision");

        assert!(matches!(error, StackError::SkillInstallFailed { .. }));
    }

    #[test]
    fn standard_essentials_resolve_as_two_individual_skill_sources() {
        let catalog = SkillCatalog::load_embedded().expect("catalog");

        let selections = essential_skill_selections(&catalog).expect("essential selections");

        assert_eq!(selections.len(), 2);
        assert_eq!(selections[0].source.id, "anthropic-skills");
        assert_eq!(selections[0].skills, ["docx", "pptx", "xlsx", "pdf"]);
        assert_eq!(selections[1].source.id, "openai-plugins");
        assert_eq!(
            selections[1].skills,
            ["gh-address-comments", "gh-fix-ci", "github", "yeet"]
        );
    }
}
