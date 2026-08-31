use super::super::*;
use crate::config::UserSkillSource;

pub(crate) fn user_source(alias: &str, github: &str, branch: &str) -> UserSkillSource {
    UserSkillSource {
        alias: alias.to_owned(),
        github: github.to_owned(),
        branch: branch.to_owned(),
        trusted: false,
    }
}

pub(crate) fn source() -> ResolvedSkillSource {
    ResolvedSkillSource {
        id: "openai-skills".to_owned(),
        name: "OpenAI Agent Skills".to_owned(),
        owner: "openai".to_owned(),
        repo: "skills".to_owned(),
        url: "https://github.com/openai/skills".to_owned(),
        branch: "main".to_owned(),
        verified_commit: None,
        indexed_commit: None,
        descriptor: SKILL_DESCRIPTOR.to_owned(),
        catalog_managed: false,
        directories: vec![
            ResolvedSkillDirectory {
                path: "skills/.system".to_owned(),
                installable: false,
            },
            ResolvedSkillDirectory {
                path: "skills/.curated".to_owned(),
                installable: true,
            },
        ],
        indexed_skills: Vec::new(),
    }
}

pub(crate) fn catalog_source(skills: Vec<CatalogSkill>) -> ResolvedSkillSource {
    ResolvedSkillSource {
        id: "openai-plugins".to_owned(),
        name: "OpenAI Plugin Skills".to_owned(),
        owner: "openai".to_owned(),
        repo: "plugins".to_owned(),
        url: "https://github.com/openai/plugins".to_owned(),
        branch: "main".to_owned(),
        verified_commit: None,
        indexed_commit: None,
        descriptor: SKILL_DESCRIPTOR.to_owned(),
        catalog_managed: true,
        directories: Vec::new(),
        indexed_skills: skills,
    }
}

pub(crate) fn write_skill(root: &Path, directory: &str, name: &str) {
    let skill_dir = root.join(directory).join(name);
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join(SKILL_DESCRIPTOR), "# Skill\n").expect("descriptor");
    std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
}

pub(crate) fn write_installed_skill(root: &Path, name: &str, descriptor: &str) {
    let skill_dir = root.join(name);
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(skill_dir.join(SKILL_DESCRIPTOR), descriptor).expect("descriptor");
    std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
    // Mirrors the install-time marker; removal refuses directories without it.
    std::fs::write(skill_dir.join(MANAGED_SKILL_MARKER), "test-source\n").expect("marker");
}

pub(crate) fn write_catalog_skill(root: &Path, path: &str, name: &str) {
    let skill_dir = root.join(path);
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join(SKILL_DESCRIPTOR),
        format!("---\nname: {name}\ndescription: test\n---\n# Skill\n"),
    )
    .expect("descriptor");
    std::fs::write(skill_dir.join("script.sh"), "true\n").expect("script");
}

pub(crate) fn canonical_temp_home(tempdir: &tempfile::TempDir) -> PathBuf {
    tempdir.path().canonicalize().expect("canonical temp home")
}

pub(crate) fn claude_code_entry(
    catalog: &RegistryCatalog,
) -> &crate::runtime::install::agent_registry::RegistryEntry {
    catalog.lookup("claude").expect("claude entry")
}

pub(crate) fn opencode_entry(
    catalog: &RegistryCatalog,
) -> &crate::runtime::install::agent_registry::RegistryEntry {
    catalog.lookup("opencode").expect("opencode entry")
}
