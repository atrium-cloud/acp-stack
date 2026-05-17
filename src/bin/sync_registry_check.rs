//! Dev tool: compare our embedded `data/agents.toml` against upstream ACP
//! `registry.json`. Not shipped in release artifacts — `cargo install` the
//! daemon and this binary stays in the workspace.
//!
//! Usage:
//!
//! ```sh
//! cargo run --bin sync-registry-check
//! ```
//!
//! The embedded registry is intentionally a small curated subset. Every
//! embedded sync id (`adapter.id` for adapter-backed entries, otherwise `id`)
//! must still exist upstream; upstream entries that are not embedded are reported for
//! awareness but do not fail the check.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::time::Duration;

use serde::Deserialize;

use acp_stack::agent_registry::RegistryCatalog;

const UPSTREAM_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct UpstreamIndex {
    #[serde(default)]
    agents: Vec<UpstreamAgent>,
}

#[derive(Debug, Deserialize)]
struct UpstreamAgent {
    id: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let upstream = fetch_upstream()?;
    let embedded = RegistryCatalog::load_embedded()?;

    let upstream_by_id: BTreeMap<&str, &UpstreamAgent> = upstream
        .agents
        .iter()
        .map(|agent| (agent.id.as_str(), agent))
        .collect();
    let embedded_sync_ids = embedded_sync_ids(&embedded);

    let drift = compare_registry_ids(upstream_by_id.keys().copied(), embedded_sync_ids);

    println!("acp-stack registry curation report");
    println!("==================================");
    println!("upstream entries: {}", upstream.agents.len());
    println!("embedded entries: {}", embedded.entries().len());
    println!();

    if drift.upstream_not_embedded.is_empty() {
        println!("[upstream entries not embedded] none");
    } else {
        println!(
            "[upstream entries not embedded] {} entries:",
            drift.upstream_not_embedded.len()
        );
        for id in &drift.upstream_not_embedded {
            println!("  - {id}");
        }
    }
    println!();

    if drift.embedded_not_upstream.is_empty() {
        println!("[embedded entries missing upstream] none");
    } else {
        println!(
            "[embedded entries missing upstream] {} entries:",
            drift.embedded_not_upstream.len()
        );
        for id in &drift.embedded_not_upstream {
            println!("  - {id}");
        }
    }
    println!();

    if drift.has_embedded_unknown_ids() {
        return Err("embedded registry contains ids missing from upstream registry".into());
    }
    println!();
    println!("embedded registry sync ids are present upstream");
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct RegistryIdDrift {
    upstream_not_embedded: Vec<String>,
    embedded_not_upstream: Vec<String>,
}

impl RegistryIdDrift {
    fn has_embedded_unknown_ids(&self) -> bool {
        !self.embedded_not_upstream.is_empty()
    }
}

fn compare_registry_ids<'a>(
    upstream_ids: impl IntoIterator<Item = &'a str>,
    embedded_ids: impl IntoIterator<Item = &'a str>,
) -> RegistryIdDrift {
    let upstream: BTreeSet<&str> = upstream_ids.into_iter().collect();
    let embedded: BTreeSet<&str> = embedded_ids.into_iter().collect();
    RegistryIdDrift {
        upstream_not_embedded: upstream
            .difference(&embedded)
            .map(|id| (*id).to_owned())
            .collect(),
        embedded_not_upstream: embedded
            .difference(&upstream)
            .map(|id| (*id).to_owned())
            .collect(),
    }
}

fn embedded_sync_ids(catalog: &RegistryCatalog) -> Vec<&str> {
    catalog
        .entries()
        .iter()
        .map(|entry| {
            entry
                .adapter
                .as_ref()
                .map(|adapter| adapter.id.as_str())
                .unwrap_or(entry.id.as_str())
        })
        .collect()
}

fn fetch_upstream() -> Result<UpstreamIndex, Box<dyn std::error::Error>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("acp-stack-sync-check/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let body = client
        .get(UPSTREAM_URL)
        .send()?
        .error_for_status()?
        .text()?;
    let parsed: UpstreamIndex = serde_json::from_str(&body)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_has_no_drift() {
        let drift = compare_registry_ids(["a", "b"], ["b", "a"]);
        assert_eq!(
            drift,
            RegistryIdDrift {
                upstream_not_embedded: Vec::new(),
                embedded_not_upstream: Vec::new(),
            }
        );
        assert!(!drift.has_embedded_unknown_ids());
    }

    #[test]
    fn missing_and_extra_ids_are_reported_in_order() {
        let drift = compare_registry_ids(["a", "b", "d"], ["a", "c"]);
        assert_eq!(
            drift,
            RegistryIdDrift {
                upstream_not_embedded: vec!["b".to_owned(), "d".to_owned()],
                embedded_not_upstream: vec!["c".to_owned()],
            }
        );
        assert!(drift.has_embedded_unknown_ids());
    }

    #[test]
    fn upstream_entries_missing_from_embedded_do_not_fail_subset_check() {
        let drift = compare_registry_ids(["a", "b"], ["a"]);
        assert_eq!(drift.upstream_not_embedded, vec!["b"]);
        assert!(drift.embedded_not_upstream.is_empty());
        assert!(!drift.has_embedded_unknown_ids());
    }

    #[test]
    fn embedded_sync_ids_use_upstream_aliases() {
        let catalog = RegistryCatalog::load_embedded().expect("embedded registry");
        assert!(embedded_sync_ids(&catalog).contains(&"amp-acp"));
        assert!(!embedded_sync_ids(&catalog).contains(&"amp"));
    }
}
