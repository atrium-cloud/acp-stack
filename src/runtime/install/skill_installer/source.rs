//! Skill source resolution: catalog aliases, configured `[[skills.sources]]`
//! entries, and ad-hoc `github:<owner>[/<repo>]` references, all normalized
//! into a [`ResolvedSkillSource`].

use super::*;

pub fn parse_skill_source(value: &str, catalog: &SkillCatalog) -> Result<SkillSourceSelection> {
    let trimmed = value.trim();
    if let Some(source) = catalog.lookup_alias(trimmed) {
        return Ok(SkillSourceSelection::Official {
            id: source.id.clone(),
        });
    }
    let Some(owner) = trimmed.strip_prefix(SOURCE_CUSTOM_GITHUB_PREFIX) else {
        return Err(StackError::SkillInstallInvalidSource {
            source_id: trimmed.to_owned(),
        });
    };
    validate_github_owner(owner)?;
    Ok(SkillSourceSelection::CustomGithubOwner {
        owner: owner.to_owned(),
    })
}

pub fn resolve_source(
    selection: &SkillSourceSelection,
    catalog: &SkillCatalog,
) -> Result<ResolvedSkillSource> {
    match selection {
        SkillSourceSelection::Official { id } => {
            let source =
                catalog
                    .lookup(id)
                    .ok_or_else(|| StackError::SkillInstallSourceMissing {
                        source_id: id.clone(),
                    })?;
            Ok(resolve_official_source(source))
        }
        SkillSourceSelection::CustomGithubOwner { owner } => {
            validate_github_owner(owner)?;
            Ok(ResolvedSkillSource {
                id: format!("{owner}-skills"),
                name: format!("{owner} Agent Skills"),
                owner: owner.clone(),
                repo: CUSTOM_SKILLS_REPO.to_owned(),
                url: format!("https://github.com/{owner}/{CUSTOM_SKILLS_REPO}"),
                branch: DEFAULT_SKILL_SOURCE_BRANCH.to_owned(),
                verified_commit: None,
                indexed_commit: None,
                descriptor: SKILL_DESCRIPTOR.to_owned(),
                catalog_managed: false,
                directories: vec![ResolvedSkillDirectory {
                    path: CUSTOM_SKILLS_DIRECTORY.to_owned(),
                    installable: true,
                }],
                indexed_skills: Vec::new(),
            })
        }
    }
}

/// Resolve a day-2 source reference. ORDER MATTERS: the embedded catalog wins
/// first so a hand-edited `[[skills.sources]]` alias cannot hijack a curated
/// one, then user sources, then ad-hoc `github:<owner>[/<repo>]`.
pub fn resolve_source_ref(
    source_ref: &str,
    user_sources: &[UserSkillSource],
    catalog: &SkillCatalog,
) -> Result<ResolvedSkillSource> {
    let trimmed = source_ref.trim();
    if let Some(catalog_source) = catalog.lookup_alias(trimmed) {
        return Ok(resolve_official_source(catalog_source));
    }
    if let Some(user) = user_sources.iter().find(|source| source.alias == trimmed) {
        return resolved_user_source(user);
    }
    if let Some(rest) = trimmed.strip_prefix(SOURCE_CUSTOM_GITHUB_PREFIX) {
        return resolve_ad_hoc_github(rest);
    }
    Err(StackError::SkillInstallInvalidSource {
        source_id: trimmed.to_owned(),
    })
}

fn resolved_user_source(user: &UserSkillSource) -> Result<ResolvedSkillSource> {
    let (owner, repo) = split_owner_repo(&user.github)?;
    Ok(github_repo_source(
        user.alias.clone(),
        format!("{} (user source)", user.alias),
        owner,
        repo,
        user.branch.clone(),
    ))
}

fn resolve_ad_hoc_github(rest: &str) -> Result<ResolvedSkillSource> {
    if rest.contains('/') {
        let (owner, repo) = split_owner_repo(rest)?;
        Ok(github_repo_source(
            format!("{owner}-{repo}-skills"),
            format!("{owner}/{repo} Agent Skills"),
            owner,
            repo,
            DEFAULT_SKILL_SOURCE_BRANCH.to_owned(),
        ))
    } else {
        validate_github_owner(rest)?;
        Ok(github_repo_source(
            format!("{rest}-skills"),
            format!("{rest} Agent Skills"),
            rest.to_owned(),
            CUSTOM_SKILLS_REPO.to_owned(),
            DEFAULT_SKILL_SOURCE_BRANCH.to_owned(),
        ))
    }
}

/// Build a non-catalog `ResolvedSkillSource` for a whole GitHub repo, expecting
/// skills flat under `skills/`.
fn github_repo_source(
    id: String,
    name: String,
    owner: String,
    repo: String,
    branch: String,
) -> ResolvedSkillSource {
    ResolvedSkillSource {
        id,
        name,
        url: format!("https://github.com/{owner}/{repo}"),
        owner,
        repo,
        branch,
        verified_commit: None,
        indexed_commit: None,
        descriptor: SKILL_DESCRIPTOR.to_owned(),
        catalog_managed: false,
        directories: vec![ResolvedSkillDirectory {
            path: CUSTOM_SKILLS_DIRECTORY.to_owned(),
            installable: true,
        }],
        indexed_skills: Vec::new(),
    }
}

fn split_owner_repo(github: &str) -> Result<(String, String)> {
    let mut parts = github.splitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) => {
            validate_github_owner(owner)?;
            validate_github_repo(repo)?;
            Ok((owner.to_owned(), repo.to_owned()))
        }
        _ => Err(StackError::SkillInstallInvalidSource {
            source_id: github.to_owned(),
        }),
    }
}

fn validate_github_repo(repo: &str) -> Result<()> {
    if crate::config::is_valid_github_repo(repo) {
        Ok(())
    } else {
        Err(StackError::SkillInstallInvalidSource {
            source_id: repo.to_owned(),
        })
    }
}

pub(super) fn resolve_official_source(source: &SkillSource) -> ResolvedSkillSource {
    ResolvedSkillSource {
        id: source.id.clone(),
        name: source.name.clone(),
        owner: source.owner.clone(),
        repo: source.repo.clone(),
        url: source.url.clone(),
        branch: source.branch.clone(),
        verified_commit: source.verified_commit.clone(),
        indexed_commit: source.indexed_commit.clone(),
        descriptor: source.descriptor.clone(),
        catalog_managed: true,
        directories: source.directories.iter().map(resolve_directory).collect(),
        indexed_skills: source.indexed_skills.clone(),
    }
}

fn resolve_directory(directory: &SkillDirectory) -> ResolvedSkillDirectory {
    ResolvedSkillDirectory {
        path: directory.path.clone(),
        installable: directory.installable,
    }
}
