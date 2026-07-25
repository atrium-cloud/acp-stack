use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use acp_stack::runtime::install::skill_registry::{CatalogSkill, SkillDiscovery, SkillSource};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::SKILL_DESCRIPTOR;

#[derive(Debug, Clone)]
struct CandidateSkill {
    name: String,
    path: String,
    digest: String,
}

pub(crate) fn discover_source_skills(
    repository_root: &Path,
    source: &SkillSource,
) -> Result<Vec<CatalogSkill>, Box<dyn std::error::Error>> {
    let mut candidates = BTreeMap::<String, CandidateSkill>::new();
    for directory in source
        .directories
        .iter()
        .filter(|directory| directory.installable)
    {
        let discovery_root = repository_root.join(&directory.path);
        match source.discovery {
            SkillDiscovery::Direct => {
                discover_direct_skills(repository_root, &discovery_root, &mut candidates)?
            }
            SkillDiscovery::Recursive => {
                discover_recursive_skills(repository_root, &discovery_root, &mut candidates)?
            }
        }
    }

    for excluded in &source.excluded_skills {
        if !candidates.contains_key(excluded) {
            return Err(format!(
                "skill source `{}` has stale excluded path `{excluded}`",
                source.id
            )
            .into());
        }
    }
    for excluded in &source.excluded_skills {
        candidates.remove(excluded);
    }

    let mut by_name = BTreeMap::<String, Vec<CandidateSkill>>::new();
    for candidate in candidates.into_values() {
        by_name
            .entry(candidate.name.clone())
            .or_default()
            .push(candidate);
    }

    let mut indexed = Vec::new();
    for (name, candidates) in by_name {
        let mut by_digest = BTreeMap::<String, Vec<CandidateSkill>>::new();
        for candidate in candidates {
            by_digest
                .entry(candidate.digest.clone())
                .or_default()
                .push(candidate);
        }
        let mut variants = Vec::new();
        for copies in by_digest.into_values() {
            variants.push(select_canonical_copy(source, &name, copies)?);
        }
        variants.sort_by(|left, right| left.path.cmp(&right.path));
        if variants.len() == 1 {
            let candidate = variants.remove(0);
            indexed.push(CatalogSkill {
                selector: normalized_install_name_selector(&name),
                name: candidate.name,
                path: candidate.path,
            });
        } else {
            for candidate in variants {
                indexed.push(CatalogSkill {
                    selector: contextual_selector(&candidate.path),
                    name: candidate.name,
                    path: candidate.path,
                });
            }
        }
    }
    disambiguate_contextual_selectors(&mut indexed)?;
    indexed.sort_by(|left, right| left.selector.cmp(&right.selector));
    Ok(indexed)
}

fn discover_direct_skills(
    repository_root: &Path,
    discovery_root: &Path,
    candidates: &mut BTreeMap<String, CandidateSkill>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = read_directory_sorted(discovery_root)?;
    for entry in entries.drain(..) {
        let metadata = std::fs::symlink_metadata(&entry)?;
        if metadata.file_type().is_symlink() {
            return Err(format!("refusing symlink in skill source `{}`", entry.display()).into());
        }
        if !metadata.is_dir() || !entry.join(SKILL_DESCRIPTOR).exists() {
            continue;
        }
        insert_candidate(repository_root, &entry, candidates)?;
    }
    Ok(())
}

fn discover_recursive_skills(
    repository_root: &Path,
    discovery_root: &Path,
    candidates: &mut BTreeMap<String, CandidateSkill>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut pending = vec![discovery_root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in read_directory_sorted(&directory)? {
            let metadata = std::fs::symlink_metadata(&entry)?;
            if metadata.file_type().is_symlink() {
                return Err(
                    format!("refusing symlink in skill source `{}`", entry.display()).into(),
                );
            }
            if metadata.is_dir() {
                pending.push(entry);
                continue;
            }
            if !metadata.is_file()
                || entry.file_name().and_then(|name| name.to_str()) != Some(SKILL_DESCRIPTOR)
            {
                continue;
            }
            let Some(skill_directory) = entry.parent() else {
                continue;
            };
            let relative = skill_directory.strip_prefix(discovery_root)?;
            if !relative
                .components()
                .any(|component| matches!(component, Component::Normal(value) if value == "skills"))
            {
                continue;
            }
            insert_candidate(repository_root, skill_directory, candidates)?;
        }
    }
    Ok(())
}

fn insert_candidate(
    repository_root: &Path,
    skill_directory: &Path,
    candidates: &mut BTreeMap<String, CandidateSkill>,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = normalized_relative_path(repository_root, skill_directory)?;
    let descriptor = skill_directory.join(SKILL_DESCRIPTOR);
    let name = parse_skill_name(&descriptor)?;
    validate_install_name(&name)?;
    let digest = hash_skill_tree(skill_directory)?;
    let candidate = CandidateSkill {
        name,
        path: path.clone(),
        digest,
    };
    if candidates.insert(path.clone(), candidate).is_some() {
        return Err(format!("duplicate discovered skill path `{path}`").into());
    }
    Ok(())
}

fn parse_skill_name(descriptor: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let metadata = std::fs::symlink_metadata(descriptor)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "skill descriptor `{}` must be a regular file",
            descriptor.display()
        )
        .into());
    }
    let body = std::fs::read_to_string(descriptor)?;
    let mut lines = body.lines();
    if lines.next() != Some("---") {
        return Err(format!(
            "skill descriptor `{}` is missing YAML frontmatter",
            descriptor.display()
        )
        .into());
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        return Err(format!(
            "skill descriptor `{}` has unterminated YAML frontmatter",
            descriptor.display()
        )
        .into());
    }
    let frontmatter: SkillFrontmatter = serde_norway::from_str(&yaml)?;
    Ok(frontmatter.name)
}

#[derive(Deserialize)]
struct SkillFrontmatter {
    name: String,
}

fn validate_install_name(name: &str) -> Result<(), Box<dyn std::error::Error>> {
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
        Ok(())
    } else {
        Err(format!("upstream skill frontmatter name `{name}` is not a safe install path").into())
    }
}

fn normalized_install_name_selector(name: &str) -> String {
    name.split('/')
        .map(normalize_selector_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn hash_skill_tree(root: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    collect_tree_entries(root, root, &mut entries)?;
    entries.sort();
    let mut hasher = Sha256::new();
    for entry in entries {
        let relative = normalized_relative_path(root, &entry)?;
        let metadata = std::fs::symlink_metadata(&entry)?;
        if metadata.is_dir() {
            hasher.update(b"directory\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            continue;
        }
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(format!("skill tree contains unsafe entry `{}`", entry.display()).into());
        }
        hasher.update(b"file\0");
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        let mut file = File::open(&entry)?;
        let mut buffer = [0_u8; 32 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        hasher.update(b"\0");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_tree_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in read_directory_sorted(directory)? {
        let metadata = std::fs::symlink_metadata(&entry)?;
        if metadata.file_type().is_symlink() {
            return Err(format!("skill tree contains symlink `{}`", entry.display()).into());
        }
        entry.strip_prefix(root)?;
        entries.push(entry.clone());
        if metadata.is_dir() {
            collect_tree_entries(root, &entry, entries)?;
        } else if !metadata.is_file() {
            return Err(format!("skill tree contains special entry `{}`", entry.display()).into());
        }
    }
    Ok(())
}

fn select_canonical_copy(
    source: &SkillSource,
    name: &str,
    mut copies: Vec<CandidateSkill>,
) -> Result<CandidateSkill, Box<dyn std::error::Error>> {
    if copies.len() == 1 {
        return Ok(copies.remove(0));
    }
    for preferred in &source.preferred_paths {
        let mut matching = copies
            .iter()
            .filter(|candidate| path_is_within(&candidate.path, preferred));
        let Some(first) = matching.next() else {
            continue;
        };
        if matching.next().is_some() {
            return Err(format!(
                "skill source `{}` has multiple identical `{name}` copies under preferred path `{preferred}`",
                source.id
            )
            .into());
        }
        return Ok(first.clone());
    }
    let paths = copies
        .iter()
        .map(|candidate| candidate.path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "skill source `{}` has identical `{name}` copies without a unique preferred path: {paths}",
        source.id
    )
    .into())
}

fn contextual_selector(path: &str) -> String {
    let components = path.split('/').collect::<Vec<_>>();
    let skills_index = components
        .iter()
        .rposition(|component| *component == "skills");
    let selected = match skills_index {
        Some(index) if index > 0 => components[index - 1..]
            .iter()
            .filter(|component| **component != "skills")
            .copied()
            .collect::<Vec<_>>(),
        _ => components
            .iter()
            .filter(|component| **component != "skills")
            .copied()
            .collect::<Vec<_>>(),
    };
    selected
        .into_iter()
        .map(normalize_selector_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn full_path_selector(path: &str) -> String {
    path.split('/')
        .filter(|component| *component != "skills")
        .map(normalize_selector_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn normalize_selector_segment(segment: &str) -> String {
    let mut normalized = String::new();
    let mut separator_pending = false;
    for character in segment.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if separator_pending && !normalized.is_empty() {
                normalized.push('-');
            }
            normalized.push(character);
            separator_pending = false;
        } else {
            separator_pending = true;
        }
    }
    normalized.trim_matches('-').to_owned()
}

fn disambiguate_contextual_selectors(
    indexed: &mut [CatalogSkill],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut counts = BTreeMap::<String, usize>::new();
    for skill in indexed.iter() {
        *counts.entry(skill.selector.clone()).or_default() += 1;
    }
    for skill in indexed.iter_mut() {
        if counts.get(&skill.selector).copied().unwrap_or_default() > 1 {
            skill.selector = full_path_selector(&skill.path);
        }
    }
    let mut seen = BTreeSet::new();
    for skill in indexed.iter() {
        if skill.selector.is_empty() || !seen.insert(skill.selector.as_str()) {
            return Err(format!(
                "could not derive a unique selector for indexed path `{}`",
                skill.path
            )
            .into());
        }
    }
    Ok(())
}

fn read_directory_sorted(directory: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut entries = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn normalized_relative_path(
    root: &Path,
    path: &Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let relative = path.strip_prefix(root)?;
    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(format!("unsafe relative source path `{}`", relative.display()).into());
        };
        let value = value
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 source path `{}`", relative.display()))?;
        components.push(value);
    }
    Ok(components.join("/"))
}

fn path_is_within(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{root}/"))
}
