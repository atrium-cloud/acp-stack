//! Agent Skills source validation: alias syntax/uniqueness and `owner/repo`
//! shape for user-declared `[[skills.sources]]`.

use std::collections::HashSet;

use crate::config::schema::{SkillsConfig, UserSkillSource};
use crate::error::{Result, StackError};

pub(crate) fn validate_skills(skills: &SkillsConfig) -> Result<()> {
    let mut aliases = HashSet::new();
    for source in &skills.sources {
        validate_source_alias(&source.alias)?;
        if !aliases.insert(source.alias.clone()) {
            return Err(duplicate_alias_error(&source.alias));
        }
        validate_github_owner_repo(&source.alias, &source.github)?;
        validate_git_ref(&source.alias, &source.branch)?;
    }
    Ok(())
}

/// Startup/reload counterpart to [`validate_skills`]: an individually invalid
/// source must not fail startup, so drop-and-report it and keep the rest.
pub(crate) fn partition_valid_sources(
    sources: Vec<UserSkillSource>,
) -> (Vec<UserSkillSource>, Vec<(String, String)>) {
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for source in sources {
        let label = if source.alias.trim().is_empty() {
            "<empty>".to_owned()
        } else {
            source.alias.clone()
        };
        let problem = if seen.contains(&source.alias) {
            Some(duplicate_alias_error(&source.alias).to_string())
        } else {
            validate_source(&source)
                .err()
                .map(|error| error.to_string())
        };
        match problem {
            Some(reason) => dropped.push((label, reason)),
            None => {
                seen.insert(source.alias.clone());
                kept.push(source);
            }
        }
    }
    (kept, dropped)
}

fn validate_source(source: &UserSkillSource) -> Result<()> {
    validate_source_alias(&source.alias)?;
    validate_github_owner_repo(&source.alias, &source.github)?;
    validate_git_ref(&source.alias, &source.branch)?;
    Ok(())
}

fn duplicate_alias_error(alias: &str) -> StackError {
    StackError::InvalidParam {
        field: "skills.sources",
        reason: format!("duplicate skill source alias `{alias}`"),
    }
}

/// A branch/ref is interpolated raw into the archive URL, so it MUST stay
/// git-ref-safe: `?`/`#` would truncate the path and `..` would redirect the
/// fetch to a different resource than the one displayed.
fn validate_git_ref(alias: &str, branch: &str) -> Result<()> {
    let valid = !branch.is_empty()
        && branch.len() <= 255
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.contains("..")
        && branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'));
    if valid {
        Ok(())
    } else {
        Err(StackError::InvalidParam {
            field: "skills.sources",
            reason: format!(
                "skill source `{alias}` branch `{branch}` must be a valid git ref \
                 (letters, digits, `-`, `_`, `.`, `/`; no `..`)"
            ),
        })
    }
}

/// Alias syntax mirrors the catalog's: lowercase alphanumerics and single
/// interior dashes.
fn validate_source_alias(alias: &str) -> Result<()> {
    let valid = !alias.is_empty()
        && alias.len() <= 64
        && !alias.starts_with('-')
        && !alias.ends_with('-')
        && !alias.contains("--")
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(StackError::InvalidParam {
            field: "skills.sources",
            reason: format!(
                "skill source alias `{alias}` must be lowercase alphanumerics and single \
                 dashes, at most 64 characters"
            ),
        })
    }
}

/// GitHub account names: ASCII alphanumerics and single dashes, max 39 chars.
/// MUST match the installer's fetch path exactly, or config persists a source
/// every later `add`/`source get` rejects.
pub(crate) fn is_valid_github_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && !owner.ends_with('-')
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Repo names are looser than owners: GitHub also allows `_` and `.`.
pub(crate) fn is_valid_github_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo.len() <= 100
        && repo != "."
        && repo != ".."
        && !repo.starts_with('-')
        && !repo.ends_with('-')
        && repo
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// `github` must be exactly `owner/repo`, each segment a valid GitHub name.
fn validate_github_owner_repo(alias: &str, github: &str) -> Result<()> {
    let mut parts = github.splitn(3, '/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(StackError::InvalidParam {
            field: "skills.sources",
            reason: format!("skill source `{alias}` github must be `owner/repo`, got `{github}`"),
        });
    };
    if !is_valid_github_owner(owner) {
        return Err(StackError::InvalidParam {
            field: "skills.sources",
            reason: format!(
                "skill source `{alias}` github `{github}` has an invalid owner `{owner}`"
            ),
        });
    }
    if !is_valid_github_repo(repo) {
        return Err(StackError::InvalidParam {
            field: "skills.sources",
            reason: format!(
                "skill source `{alias}` github `{github}` has an invalid repository `{repo}`"
            ),
        });
    }
    Ok(())
}
