//! Name, selector, and relative-path validation for skill installs: the gate
//! between untrusted catalog/archive strings and any path the installer joins
//! onto a real directory, so these rules reject rather than sanitize.

use super::*;

pub(super) fn validate_skill_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
    if valid {
        Ok(())
    } else {
        Err(StackError::SkillInstallInvalidName {
            name: name.to_owned(),
        })
    }
}

pub(super) fn validate_skill_selector(selector: &str) -> Result<()> {
    if !selector.is_empty()
        && selector
            .split('/')
            .all(|segment| validate_skill_name(segment).is_ok())
    {
        return Ok(());
    }
    Err(StackError::SkillInstallInvalidName {
        name: selector.to_owned(),
    })
}

// Mirrors the catalog's install-name rules, including `:` for frontmatter
// names such as `cocounsel-legal:deep-research`.
pub(crate) fn validate_install_target_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':'))
                && segment
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                && segment
                    .bytes()
                    .next_back()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
        });
    if valid {
        return Ok(());
    }
    Err(StackError::SkillInstallFailed {
        reason: format!("skill install name `{name}` is not a safe relative path"),
    })
}

pub(super) fn validate_github_owner(owner: &str) -> Result<()> {
    if crate::config::is_valid_github_owner(owner) {
        Ok(())
    } else {
        Err(StackError::SkillInstallInvalidSource {
            source_id: format!("{SOURCE_CUSTOM_GITHUB_PREFIX}{owner}"),
        })
    }
}

pub(super) fn validate_registry_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(StackError::SkillInstallFailed {
            reason: format!("skill directory `{value}` must be relative"),
        });
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(StackError::SkillInstallFailed {
                    reason: format!("skill directory `{value}` contains an unsafe path segment"),
                });
            }
        }
    }
    Ok(())
}
