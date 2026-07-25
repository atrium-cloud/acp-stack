use acp_stack::runtime::install::skill_registry::{CatalogSkill, SkillDiscovery, SkillSource};

pub(crate) fn render_catalog(sources: &[SkillSource]) -> String {
    let mut output = String::from(
        "# Reviewed Agent Skills Catalog\n\
         # Generated indexes are maintained by `sync-skills-catalog`.\n",
    );
    for source in sources {
        output.push_str("\n[[sources]]\n");
        push_field(&mut output, "id", &source.id);
        push_field(&mut output, "alias", &source.alias);
        push_field(&mut output, "name", &source.name);
        push_field(&mut output, "owner", &source.owner);
        push_field(&mut output, "repo", &source.repo);
        push_field(&mut output, "url", &source.url);
        push_array(&mut output, "docs", &source.docs);
        output.push_str(&format!("official = {}\n", source.official));
        output.push_str(&format!("trusted = {}\n", source.trusted));
        push_field(&mut output, "reviewed_at", &source.reviewed_at);
        push_field(&mut output, "branch", &source.branch);
        if let Some(commit) = source.verified_commit.as_deref() {
            push_field(&mut output, "verified_commit", commit);
        }
        if let Some(commit) = source.indexed_commit.as_deref() {
            push_field(&mut output, "indexed_commit", commit);
        }
        push_field(&mut output, "descriptor", &source.descriptor);
        push_field(
            &mut output,
            "discovery",
            match source.discovery {
                SkillDiscovery::Direct => "direct",
                SkillDiscovery::Recursive => "recursive",
            },
        );
        push_array(&mut output, "preferred_paths", &source.preferred_paths);
        push_array(&mut output, "excluded_skills", &source.excluded_skills);
        push_array(&mut output, "essential_skills", &source.essential_skills);
        push_skills(&mut output, &source.indexed_skills);

        for directory in &source.directories {
            output.push_str("\n[[sources.directories]]\n");
            push_field(&mut output, "path", &directory.path);
            push_field(&mut output, "source_url", &directory.source_url);
            output.push_str(&format!("verified = {}\n", directory.verified));
            output.push_str(&format!("installable = {}\n", directory.installable));
        }
    }
    output
}

fn push_field(output: &mut String, key: &str, value: &str) {
    output.push_str(&format!("{key} = \"{}\"\n", toml_escape(value)));
}

fn push_array(output: &mut String, key: &str, values: &[String]) {
    if values.is_empty() {
        output.push_str(&format!("{key} = []\n"));
        return;
    }
    output.push_str(&format!("{key} = [\n"));
    for value in values {
        output.push_str(&format!("  \"{}\",\n", toml_escape(value)));
    }
    output.push_str("]\n");
}

fn push_skills(output: &mut String, skills: &[CatalogSkill]) {
    if skills.is_empty() {
        output.push_str("indexed_skills = []\n");
        return;
    }
    output.push_str("indexed_skills = [\n");
    for skill in skills {
        output.push_str(&format!(
            "  {{ selector = \"{}\", name = \"{}\", path = \"{}\" }},\n",
            toml_escape(&skill.selector),
            toml_escape(&skill.name),
            toml_escape(&skill.path)
        ));
    }
    output.push_str("]\n");
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
