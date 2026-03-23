use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

use crate::github::GitHubClient;
use crate::releases::{SDK_OWNER, SDK_REPO};
use crate::state::State;

/// Fetched GitHub Project V2 metadata.
struct ProjectInfo {
    /// Global node ID of the project.
    project_id: String,
    /// Field name -> field node ID.
    fields: HashMap<String, String>,
}


/// Build a lookup from PR number to the crates it touches and all known release
/// versions containing that PR. We collect all versions rather than just the
/// earliest because polkadot-sdk publishes crate versions from independent
/// release branches, and a higher version number does not guarantee it contains
/// all changes from a lower one.
pub fn build_pr_crate_map(state: &State) -> HashMap<u64, HashMap<String, Vec<String>>> {
    let mut map: HashMap<u64, HashMap<String, Vec<String>>> = HashMap::new();
    for release in &state.releases {
        for crate_rel in &release.crates {
            for &pr in &crate_rel.prs {
                let versions = map.entry(pr)
                    .or_default()
                    .entry(crate_rel.name.clone())
                    .or_default();
                if !versions.contains(&crate_rel.version) {
                    versions.push(crate_rel.version.clone());
                }
            }
        }
    }
    map
}

/// Per-PR annotation detail.
pub struct PrAnnotation {
    /// PR number.
    pub number: u64,
    /// Per-runtime status (indexed by runtime, empty string if not relevant).
    pub statuses: Vec<String>,
}

/// Annotation results returned by `annotate`.
pub struct AnnotationStats {
    /// Number of PRs with non-empty status, per runtime index.
    pub per_runtime: Vec<usize>,
    /// Per-PR details (only populated when verbose=true).
    pub details: Vec<PrAnnotation>,
}

/// Annotate PRs in the GitHub Project V2.
/// When `dirty_prs` is Some, only those PRs are annotated. When None, all PRs are annotated.
/// Returns per-runtime counts of PRs with non-empty status, and per-PR details when `verbose`.
pub async fn annotate(state: &State, gh: &GitHubClient, dry_run: bool, verbose: bool, dirty_prs: Option<&HashSet<u64>>) -> Result<AnnotationStats> {
    log::info!("Annotate GitHub Project");
    let project = fetch_project_info(gh, &state.project.org, state.project.number).await?;
    log::debug!("Project ID: {}", project.project_id);
    log::debug!("Fields: {:?}", project.fields.keys().collect::<Vec<_>>());

    // Build PR -> release tags mapping
    let mut pr_tags: HashMap<u64, Vec<String>> = HashMap::new();
    for release in &state.releases {
        for crate_rel in &release.crates {
            for &pr in &crate_rel.prs {
                // Only include PRs in the dirty set (or all if no dirty set)
                if dirty_prs.map_or(true, |d| d.contains(&pr)) {
                    pr_tags.entry(pr).or_default().push(release.tag.clone());
                }
            }
        }
    }

    // Deduplicate tags per PR
    for tags in pr_tags.values_mut() {
        tags.sort();
        tags.dedup();
    }

    log::info!("{} PRs to annotate", pr_tags.len());

    let pr_crates = build_pr_crate_map(state);
    let prs: Vec<u64> = pr_tags.keys().copied().collect();

    // Compute per-runtime counts from the same data used for annotation
    let per_runtime = state.runtimes.iter()
        .map(|rt| {
            prs.iter()
                .filter(|&&pr| !compute_runtime_status(rt, pr_crates.get(&pr)).is_empty())
                .count()
        })
        .collect();
    let details = if verbose {
        let mut d: Vec<PrAnnotation> = pr_tags.iter()
            .map(|(&pr, _tags)| {
                let statuses = state.runtimes.iter()
                    .map(|rt| compute_runtime_status(rt, pr_crates.get(&pr)))
                    .collect();
                PrAnnotation { number: pr, statuses }
            })
            .collect();
        d.sort_by_key(|a| a.number);
        d
    } else {
        Vec::new()
    };
    let stats = AnnotationStats { per_runtime, details };

    if dry_run {
        return Ok(stats);
    }

    // Ensure "Release Tags" field exists
    let release_tags_field = match project.fields.get("Release Tags") {
        Some(f) => f.clone(),
        None => {
            log::info!("Creating 'Release Tags' field...");
            create_text_field(gh, &project.project_id, "Release Tags").await?
        }
    };

    // Ensure per-runtime fields exist
    let mut runtime_field_ids: HashMap<String, String> = HashMap::new();
    for runtime in &state.runtimes {
        let field_id = match project.fields.get(&runtime.field_name) {
            Some(f) => f.clone(),
            None => {
                log::info!("Creating '{}' field...", runtime.field_name);
                create_text_field(gh, &project.project_id, &runtime.field_name).await?
            }
        };
        runtime_field_ids.insert(runtime.field_name.clone(), field_id);
    }

    // Collect all field IDs for batch updates: "Release Tags" + per-runtime fields
    let mut all_field_ids: Vec<(&str, &str)> = vec![("Release Tags", &release_tags_field)];
    for runtime in &state.runtimes {
        if let Some(fid) = runtime_field_ids.get(&runtime.field_name) {
            all_field_ids.push((&runtime.field_name, fid));
        }
    }

    // Process PRs in batches
    let pr_list: Vec<_> = pr_tags.iter().collect();
    let total_prs = pr_list.len();
    let batch_size = 20;

    for (batch_idx, batch) in pr_list.chunks(batch_size).enumerate() {
        let start = batch_idx * batch_size + 1;
        let end = start + batch.len() - 1;
        log::info!("[{start}-{end}/{total_prs}] Fetching PR node IDs...");

        // Step 1: Batch fetch PR node IDs
        let pr_numbers: Vec<u64> = batch.iter().map(|(&n, _)| n).collect();
        let node_ids = batch_get_pr_node_ids(gh, SDK_OWNER, SDK_REPO, &pr_numbers).await?;

        // Filter to PRs we got node IDs for
        let resolved: Vec<_> = batch.iter()
            .filter_map(|(&pr_num, tags)| {
                node_ids.get(&pr_num).map(|node_id| (pr_num, tags, node_id.as_str()))
            })
            .collect();

        if resolved.is_empty() {
            continue;
        }

        // Step 2: Batch add items to project
        log::info!("[{start}-{end}/{total_prs}] Adding {} PRs to project...", resolved.len());
        let content_ids: Vec<_> = resolved.iter().map(|(_, _, nid)| *nid).collect();
        let item_ids = batch_add_items_to_project(gh, &project.project_id, &content_ids).await?;

        // Step 3: Batch set all field values
        let mut field_updates = Vec::new();
        for (i, &(pr_num, tags, _)) in resolved.iter().enumerate() {
            let item_id = match item_ids.get(i) {
                Some(id) => id.as_str(),
                None => continue,
            };

            // Release Tags field
            let tags_value = tags.join(", ");
            field_updates.push((item_id.to_string(), release_tags_field.clone(), tags_value));

            // Per-runtime status fields
            let crates = pr_crates.get(&pr_num);
            for runtime in &state.runtimes {
                if let Some(field_id) = runtime_field_ids.get(&runtime.field_name) {
                    let status = compute_runtime_status(runtime, crates);
                    field_updates.push((item_id.to_string(), field_id.clone(), status));
                }
            }
        }

        log::info!("[{start}-{end}/{total_prs}] Setting {} field values...", field_updates.len());
        batch_set_field_values(gh, &project.project_id, &field_updates).await?;
    }

    Ok(stats)
}

/// Compute the per-runtime status for a PR following the state machine:
///   (empty)                        - crates not picked up by downstream
///   pending > v{onchain_spec}      - picked up, spec not bumped
///   pending v{new_spec}            - picked up, spec bumped, not enacted on-chain
///   v{spec}                        - enacted on-chain
/// Partial adoption appends ` (N/M crates)`.
pub fn compute_runtime_status(
    runtime: &crate::state::Runtime,
    pr_release_crates: Option<&HashMap<String, Vec<String>>>,
) -> String {
    let pr_release_crates = match pr_release_crates {
        Some(c) if !c.is_empty() => c,
        _ => return String::new(),
    };

    // Filter to crates that are actual dependencies of this runtime
    let relevant: Vec<_> = pr_release_crates
        .keys()
        .filter(|name| runtime.downstream.deps.contains(name.as_str()))
        .cloned()
        .collect();

    if relevant.is_empty() {
        return String::new();
    }

    // Count how many relevant crates have a downstream version that matches one
    // of the known release versions containing this PR. We use exact matching
    // rather than >= because polkadot-sdk publishes from independent release
    // branches, and a higher version does not imply it contains the same backports.
    let adopted = relevant
        .iter()
        .filter(|name| {
            let release_versions = pr_release_crates.get(name.as_str());
            let lock_ver = runtime.downstream.versions.get(name.as_str());
            matches!((release_versions, lock_ver), (Some(versions), Some(l)) if versions.iter().any(|v| v == l))
        })
        .count();

    let total = relevant.len();

    if adopted == 0 {
        return String::new();
    }

    let partial_suffix = if adopted < total {
        format!(" ({adopted}/{total} crates)")
    } else {
        String::new()
    };

    let onchain_spec = runtime
        .upgrades
        .iter()
        .map(|u| u.spec_version)
        .max()
        .unwrap_or(0);
    let code_spec = runtime.downstream.spec_version.unwrap_or(0);

    if code_spec > onchain_spec {
        format!("pending v{code_spec}{partial_suffix}")
    } else if onchain_spec > 0 {
        format!("v{onchain_spec}{partial_suffix}")
    } else {
        format!("pending > v{code_spec}{partial_suffix}")
    }
}

/// Fetch project ID and field definitions via GraphQL.
async fn fetch_project_info(gh: &GitHubClient, org: &str, number: u64) -> Result<ProjectInfo> {
    let query = r#"
        query($org: String!, $number: Int!) {
            organization(login: $org) {
                projectV2(number: $number) {
                    id
                    fields(first: 50) {
                        nodes {
                            ... on ProjectV2Field {
                                id
                                name
                            }
                            ... on ProjectV2SingleSelectField {
                                id
                                name
                            }
                            ... on ProjectV2IterationField {
                                id
                                name
                            }
                        }
                    }
                }
            }
        }
    "#;

    let vars = serde_json::json!({
        "org": org,
        "number": number as i64,
    });

    let resp = gh.graphql_query(query, vars).await?;
    let project = &resp["data"]["organization"]["projectV2"];

    let project_id = project["id"]
        .as_str()
        .context("no project ID")?
        .to_string();

    let mut fields = HashMap::new();
    if let Some(nodes) = project["fields"]["nodes"].as_array() {
        for node in nodes {
            if let (Some(id), Some(name)) = (node["id"].as_str(), node["name"].as_str()) {
                fields.insert(name.to_string(), id.to_string());
            }
        }
    }

    Ok(ProjectInfo { project_id, fields })
}

/// Create a TEXT field on a Project V2, returning its node ID.
async fn create_text_field(gh: &GitHubClient, project_id: &str, name: &str) -> Result<String> {
    let query = r#"
        mutation($projectId: ID!, $name: String!) {
            createProjectV2Field(input: {
                projectId: $projectId,
                dataType: TEXT,
                name: $name
            }) {
                projectV2Field {
                    ... on ProjectV2Field {
                        id
                    }
                }
            }
        }
    "#;

    let vars = serde_json::json!({
        "projectId": project_id,
        "name": name,
    });

    let resp = gh.graphql_query(query, vars).await?;
    resp["data"]["createProjectV2Field"]["projectV2Field"]["id"]
        .as_str()
        .map(String::from)
        .context("no field ID in response")
}

/// Batch fetch PR node IDs using GraphQL aliases.
async fn batch_get_pr_node_ids(
    gh: &GitHubClient,
    owner: &str,
    repo: &str,
    numbers: &[u64],
) -> Result<HashMap<u64, String>> {
    if numbers.is_empty() {
        return Ok(HashMap::new());
    }

    let fragments: Vec<String> = numbers
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            format!(
                "pr{i}: repository(owner: {owner:?}, name: {repo:?}) {{ pullRequest(number: {n}) {{ id }} }}"
            )
        })
        .collect();

    let query = format!("query {{ {} }}", fragments.join("\n"));
    let resp = gh.graphql_query(&query, serde_json::json!({})).await?;

    let mut result = HashMap::new();
    for (i, &n) in numbers.iter().enumerate() {
        if let Some(id) = resp["data"][format!("pr{i}")]["pullRequest"]["id"].as_str() {
            result.insert(n, id.to_string());
        } else {
            log::warn!("PR #{n}: could not fetch node ID");
        }
    }
    Ok(result)
}

/// Batch add content nodes to a Project V2, returning item IDs in order.
async fn batch_add_items_to_project(
    gh: &GitHubClient,
    project_id: &str,
    content_ids: &[&str],
) -> Result<Vec<String>> {
    if content_ids.is_empty() {
        return Ok(Vec::new());
    }

    let fragments: Vec<String> = content_ids
        .iter()
        .enumerate()
        .map(|(i, cid)| {
            format!(
                "a{i}: addProjectV2ItemById(input: {{ projectId: {project_id:?}, contentId: {cid:?} }}) {{ item {{ id }} }}"
            )
        })
        .collect();

    let query = format!("mutation {{ {} }}", fragments.join("\n"));
    let resp = gh.graphql_query(&query, serde_json::json!({})).await?;

    let mut result = Vec::new();
    for i in 0..content_ids.len() {
        let id = resp["data"][format!("a{i}")]["item"]["id"]
            .as_str()
            .context(format!("no item ID for index {i}"))?;
        result.push(id.to_string());
    }
    Ok(result)
}

/// Batch set text field values on project items.
/// Each entry is (item_id, field_id, value).
async fn batch_set_field_values(
    gh: &GitHubClient,
    project_id: &str,
    updates: &[(String, String, String)],
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }

    // GitHub GraphQL has limits on mutation complexity, chunk to ~50 updates per call
    for chunk in updates.chunks(50) {
        let fragments: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(i, (item_id, field_id, value))| {
                let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                format!(
                    "f{i}: updateProjectV2ItemFieldValue(input: {{ projectId: {project_id:?}, itemId: {item_id:?}, fieldId: {field_id:?}, value: {{ text: \"{escaped}\" }} }}) {{ projectV2Item {{ id }} }}"
                )
            })
            .collect();

        let query = format!("mutation {{ {} }}", fragments.join("\n"));
        gh.graphql_query(&query, serde_json::json!({})).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::*;
    use std::collections::{HashMap, HashSet};

    fn make_state(releases: Vec<Release>, runtimes: Vec<Runtime>) -> State {
        State {
            project: Project { org: "test".into(), number: 1 },
            runtimes,
            last_processed_tag: None,
            releases,
        }
    }

    fn make_upgrade(spec_version: u64) -> Upgrade {
        Upgrade {
            spec_version,
            block_number: 100,
            block_hash: "0x00".into(),
            date: "2025-01-01".into(),
            block_url: "https://explorer/100".into(),
        }
    }

    fn make_runtime(
        versions: HashMap<String, String>,
        deps: HashSet<String>,
        spec_version: Option<u64>,
        upgrades: Vec<Upgrade>,
    ) -> Runtime {
        Runtime {
            runtime: "test-runtime".into(),
            short: "TR".into(),
            repo: "org/repo".into(),
            branch: "main".into(),
            cargo_lock_path: "Cargo.lock".into(),
            cargo_toml_path: "Cargo.toml".into(),
            spec_version_path: "lib.rs".into(),
            network: "testnet".into(),
            rpc: "https://rpc".into(),
            ws: "wss://ws".into(),
            field_name: "TR Test".into(),
            block_explorer_url: "https://explorer".into(),
            last_seen_commit: None,
            upgrades,
            downstream: DownstreamInfo { versions, deps, spec_version },
        }
    }

    #[test]
    fn build_pr_crate_map_basic() {
        let state = make_state(vec![
            Release {
                tag: "v1".into(),
                prev_tag: "v0".into(),
                crates: vec![
                    CrateRelease { name: "crate-a".into(), version: "1.0.0".into(), published: "2025-01-01".into(), prs: vec![10, 20] },
                ],
            },
        ], vec![]);

        let map = build_pr_crate_map(&state);
        assert_eq!(map[&10]["crate-a"], vec!["1.0.0"]);
        assert_eq!(map[&20]["crate-a"], vec!["1.0.0"]);
    }

    #[test]
    fn build_pr_crate_map_collects_all_versions() {
        let state = make_state(vec![
            Release {
                tag: "v1".into(),
                prev_tag: "v0".into(),
                crates: vec![
                    CrateRelease { name: "crate-a".into(), version: "1.0.0".into(), published: "2025-01-01".into(), prs: vec![10] },
                ],
            },
            Release {
                tag: "v2".into(),
                prev_tag: "v1".into(),
                crates: vec![
                    CrateRelease { name: "crate-a".into(), version: "2.0.0".into(), published: "2025-02-01".into(), prs: vec![10] },
                ],
            },
        ], vec![]);

        let map = build_pr_crate_map(&state);
        assert_eq!(map[&10]["crate-a"], vec!["1.0.0", "2.0.0"]);
    }

    #[test]
    fn compute_runtime_status_no_crates() {
        let rt = make_runtime(HashMap::new(), HashSet::new(), None, vec![]);
        assert_eq!(compute_runtime_status(&rt, None), "");
    }

    #[test]
    fn compute_runtime_status_not_in_deps() {
        let rt = make_runtime(HashMap::new(), HashSet::new(), None, vec![]);
        let crates = HashMap::from([("crate-a".into(), vec!["1.0.0".into()])]);
        assert_eq!(compute_runtime_status(&rt, Some(&crates)), "");
    }

    #[test]
    fn compute_runtime_status_adopted_enacted() {
        let rt = make_runtime(
            HashMap::from([("crate-a".into(), "1.0.0".into())]),
            HashSet::from(["crate-a".into()]),
            Some(2000006),
            vec![make_upgrade(2000006)],
        );
        let crates = HashMap::from([("crate-a".into(), vec!["1.0.0".into()])]);
        assert_eq!(compute_runtime_status(&rt, Some(&crates)), "v2000006");
    }

    #[test]
    fn compute_runtime_status_adopted_pending() {
        let rt = make_runtime(
            HashMap::from([("crate-a".into(), "1.0.0".into())]),
            HashSet::from(["crate-a".into()]),
            Some(3000000),
            vec![make_upgrade(2000006)],
        );
        let crates = HashMap::from([("crate-a".into(), vec!["1.0.0".into()])]);
        assert_eq!(compute_runtime_status(&rt, Some(&crates)), "pending v3000000");
    }

    #[test]
    fn compute_runtime_status_partial_adoption() {
        let rt = make_runtime(
            HashMap::from([("crate-a".into(), "2.0.0".into())]),
            HashSet::from(["crate-a".into(), "crate-b".into()]),
            Some(2000006),
            vec![make_upgrade(2000006)],
        );
        let crates = HashMap::from([
            ("crate-a".into(), vec!["1.0.0".into(), "2.0.0".into()]),
            ("crate-b".into(), vec!["1.0.0".into()]),
        ]);
        assert_eq!(
            compute_runtime_status(&rt, Some(&crates)),
            "v2000006 (1/2 crates)"
        );
    }

    #[test]
    fn compute_runtime_status_version_from_different_branch_not_adopted() {
        let rt = make_runtime(
            // Downstream has version 0.24.1 (from a branch without the backport)
            HashMap::from([("crate-a".into(), "0.24.1".into())]),
            HashSet::from(["crate-a".into()]),
            Some(2000006),
            vec![make_upgrade(2000006)],
        );
        // PR was backported to branches producing 0.21.1, 0.23.1, and 0.25.0
        let crates = HashMap::from([("crate-a".into(), vec![
            "0.21.1".into(), "0.23.1".into(), "0.25.0".into(),
        ])]);
        // 0.24.1 is not in the known versions, so not adopted
        assert_eq!(compute_runtime_status(&rt, Some(&crates)), "");
    }

}
