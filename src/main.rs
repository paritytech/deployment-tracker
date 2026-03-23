//! PR Deployment Tracker for polkadot-sdk releases.
//!
//! Tracks when PRs merged in polkadot-sdk reach downstream runtimes and go live on-chain,
//! annotating a GitHub Project V2 with release tags and per-runtime deployment status.

/// Downstream runtime consumption checks.
mod downstream;
/// GitHub REST and GraphQL API client.
mod github;
/// On-chain spec version tracking via Substrate RPC.
mod onchain;
/// GitHub Project V2 annotation logic.
mod project;
/// Release discovery and PR resolution.
mod releases;
/// Persistent tracker state.
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Step {
    /// Discover new releases from releases-v1.json and resolve PRs via prdocs.
    Discover,
    /// Check downstream runtimes for crate consumption.
    Downstream,
    /// Query on-chain spec versions and detect runtime upgrades.
    Onchain,
    /// Annotate the GitHub Project V2 with release tags and deployment status.
    Annotate,
}

impl Step {
    const ALL: &[Step] = &[Step::Discover, Step::Onchain, Step::Downstream, Step::Annotate];
}

/// CLI arguments.
#[derive(Parser)]
#[clap(name = "tracker", about = "PR Deployment Tracker for polkadot-sdk")]
struct Cli {
    /// Run without modifying state or GitHub project
    #[clap(long)]
    dry_run: bool,

    /// Print detailed per-PR annotations
    #[clap(long)]
    verbose: bool,

    /// Run only a specific step
    #[clap(long)]
    step: Option<Step>,

    /// Filter to a single runtime by field name (e.g. "AH Paseo")
    #[clap(long)]
    runtime: Option<String>,

    /// Path to state.json (default: ./state.json)
    #[clap(long)]
    state_path: Option<PathBuf>,

    /// Path to a local polkadot-sdk git checkout
    #[clap(long, env = "POLKADOT_SDK_DIR")]
    sdk_repo: PathBuf,
}

struct DownstreamSummary {
    current_spec: Option<u64>,
    crate_updates: usize,
}

struct OnchainSummary {
    previous: Option<u64>,
    current: u64,
}

struct AnnotateSummary {
    version: Option<u64>,
    prs: usize,
}

enum StepSummary {
    Discover { new_tags: Vec<String> },
    Downstream { runtimes: Vec<DownstreamSummary> },
    Onchain { runtimes: Vec<OnchainSummary> },
    Annotate {
        runtimes: Vec<AnnotateSummary>,
        details: Vec<project::PrAnnotation>,
    },
}

struct Runner {
    gh: github::GitHubClient,
    state: state::State,
    state_path: PathBuf,
    releases_json: releases::ReleasesJson,
    dry_run: bool,
    verbose: bool,
    sdk_repo: PathBuf,
    /// PRs that need annotation this run. None means all PRs (bootstrap).
    dirty_prs: Option<HashSet<u64>>,
}

impl Runner {
    async fn run(mut self, steps: &[Step]) -> Result<()> {
        let mut summaries = Vec::new();
        for &step in steps {
            summaries.push(self.run_step(step).await?);
            self.save_state()?;
        }
        self.print_summary(&summaries);
        Ok(())
    }

    fn print_summary(&self, summaries: &[StepSummary]) {
        use comfy_table::{Table, presets::UTF8_FULL_CONDENSED};

        let make_table = || {
            let mut t = Table::new();
            t.load_preset(UTF8_FULL_CONDENSED);
            t
        };

        let fmt_spec = |v: u64| format!("v{v}");
        let fmt_opt_spec = |v: Option<u64>| v.map_or("-".into(), |v| fmt_spec(v));
        let hyperlink = |url: &str, label: &str| {
            format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\")
        };

        for s in summaries {
            match s {
                StepSummary::Discover { new_tags } => {
                    let mut t = make_table();
                    t.set_header(["Release Discovery", ""]);
                    let latest = self.state.last_processed_tag.as_deref().unwrap_or("-");
                    t.add_row(["Latest known", latest]);
                    if new_tags.is_empty() {
                        t.add_row(["New releases", "none"]);
                    } else {
                        for (i, tag) in new_tags.iter().enumerate() {
                            let label = if i == 0 { "New releases" } else { "" };
                            t.add_row([label, tag.as_str()]);
                        }
                    }
                    println!("\n{t}");
                }
                StepSummary::Downstream { runtimes } => {
                    let mut t = make_table();
                    t.set_header(["Runtime Discovery", "Current", "Code Spec", "Crate Updates"]);
                    for (ds, rt) in runtimes.iter().zip(&self.state.runtimes) {
                        let code_spec = fmt_opt_spec(rt.downstream.spec_version);
                        let is_new = rt.downstream.spec_version != ds.current_spec;
                        let updates = if is_new { ds.crate_updates.to_string() } else { "-".into() };
                        t.add_row([&rt.field_name, &fmt_opt_spec(ds.current_spec), &code_spec, &updates]);
                    }
                    println!("\n{t}");
                }
                StepSummary::Onchain { runtimes } => {
                    let mut t = make_table();
                    t.set_header(["Onchain Discovery", "Previous", "Current", "Pending"]);
                    for (oc, rt) in runtimes.iter().zip(&self.state.runtimes) {
                        let pending = match rt.downstream.spec_version {
                            Some(code) if code > oc.current => fmt_spec(code),
                            _ => "-".into(),
                        };
                        t.add_row([&rt.field_name, &fmt_opt_spec(oc.previous), &fmt_spec(oc.current), &pending]);
                    }
                    println!("\n{t}");
                }
                StepSummary::Annotate { runtimes, details } => {
                    let mut t = make_table();
                    t.set_header(["PRs to Annotate", "Version", "PRs"]);
                    for (an, rt) in runtimes.iter().zip(&self.state.runtimes) {
                        t.add_row([rt.field_name.as_str(), &fmt_opt_spec(an.version), &an.prs.to_string()]);
                    }
                    println!("\n{t}");

                    if !details.is_empty() {
                        println!();
                        for pr in details {
                            let url = format!(
                                "https://github.com/paritytech/polkadot-sdk/pull/{}",
                                pr.number,
                            );
                            let label = format!("#{}", pr.number);
                            let mut parts = Vec::new();
                            for (rt, status) in self.state.runtimes.iter().zip(&pr.statuses) {
                                if !status.is_empty() {
                                    parts.push(format!("{}: {status}", rt.field_name));
                                }
                            }
                            if parts.is_empty() {
                                println!("  {}", hyperlink(&url, &label));
                            } else {
                                println!("  {} {}", hyperlink(&url, &label), parts.join(", "));
                            }
                        }
                    } else if !self.verbose {
                        println!("\nRun with --verbose to display all annotation details.");
                    }
                }
            }
        }
    }

    fn save_state(&self) -> Result<()> {
        if !self.dry_run {
            log::info!("Saving state to {}", self.state_path.display());
            self.state.save(&self.state_path)?;
        }
        Ok(())
    }

    /// Collect PRs from the given release tags.
    fn prs_from_tags(&self, tags: &[String]) -> HashSet<u64> {
        let tag_set: HashSet<&str> = tags.iter().map(|t| t.as_str()).collect();
        self.state.releases.iter()
            .filter(|r| tag_set.contains(r.tag.as_str()))
            .flat_map(|r| r.crates.iter().flat_map(|c| &c.prs))
            .copied()
            .collect()
    }

    /// Find PRs associated with the given crate version updates.
    fn prs_from_crate_updates(&self, updates: &HashSet<downstream::CrateUpdate>) -> HashSet<u64> {
        self.state.releases.iter()
            .flat_map(|r| &r.crates)
            .filter(|c| updates.contains(&downstream::CrateUpdate {
                name: c.name.clone(),
                version: c.version.clone(),
            }))
            .flat_map(|c| &c.prs)
            .copied()
            .collect()
    }

    /// Find PRs whose crates are dependencies of the given runtimes.
    fn prs_from_runtimes(&self, runtime_indices: &[usize]) -> HashSet<u64> {
        let deps: HashSet<&str> = runtime_indices.iter()
            .flat_map(|&i| self.state.runtimes[i].downstream.deps.iter().map(|s| s.as_str()))
            .collect();

        self.state.releases.iter()
            .flat_map(|r| &r.crates)
            .filter(|c| deps.contains(c.name.as_str()))
            .flat_map(|c| &c.prs)
            .copied()
            .collect()
    }

    fn add_dirty(&mut self, prs: HashSet<u64>) {
        match &mut self.dirty_prs {
            Some(set) => set.extend(prs),
            None => {} // None means all — already covers these
        }
    }

    async fn run_step(&mut self, step: Step) -> Result<StepSummary> {
        match step {
            Step::Discover => {
                let new_tags = releases::discover_and_resolve(
                    &mut self.state, &self.releases_json, &self.sdk_repo,
                )?;
                if !new_tags.is_empty() {
                    let new_prs = self.prs_from_tags(&new_tags);
                    log::info!("{} dirty PRs from {} new release tag(s)", new_prs.len(), new_tags.len());
                    self.add_dirty(new_prs);
                }
                Ok(StepSummary::Discover { new_tags })
            }
            Step::Downstream => {
                let crate_updates = downstream::check_downstream(&mut self.state, &self.gh).await?;
                if !crate_updates.is_empty() {
                    let prs = self.prs_from_crate_updates(&crate_updates);
                    log::info!("{} dirty PRs from {} crate update(s)", prs.len(), crate_updates.len());
                    self.add_dirty(prs);
                }
                let runtimes = self.state.runtimes.iter()
                    .map(|rt| {
                        let count = crate_updates.iter()
                            .filter(|u| rt.downstream.deps.contains(&u.name))
                            .count();
                        DownstreamSummary { current_spec: rt.max_onchain_spec(), crate_updates: count }
                    })
                    .collect();
                Ok(StepSummary::Downstream { runtimes })
            }
            Step::Onchain => {
                let prev_specs: Vec<Option<u64>> = self.state.runtimes.iter()
                    .map(|rt| rt.max_onchain_spec())
                    .collect();
                let upgraded = onchain::check_onchain(&mut self.state.runtimes).await?;
                if !upgraded.is_empty() {
                    let prs = self.prs_from_runtimes(&upgraded);
                    log::info!("{} dirty PRs from {} runtime upgrade(s)", prs.len(), upgraded.len());
                    self.add_dirty(prs);
                }
                let runtimes = self.state.runtimes.iter().enumerate()
                    .map(|(i, rt)| OnchainSummary {
                        previous: prev_specs[i],
                        current: rt.max_onchain_spec().unwrap_or(0),
                    })
                    .collect();
                Ok(StepSummary::Onchain { runtimes })
            }
            Step::Annotate => {
                let stats = project::annotate(
                    &self.state, &self.gh, self.dry_run, self.verbose, self.dirty_prs.as_ref(),
                ).await?;
                let runtimes = self.state.runtimes.iter().enumerate()
                    .map(|(i, rt)| AnnotateSummary {
                        version: rt.downstream.spec_version,
                        prs: stats.per_runtime[i],
                    })
                    .collect();
                Ok(StepSummary::Annotate { runtimes, details: stats.details })
            }
        }
    }
}

fn resolve_state_path(cli_path: Option<PathBuf>) -> PathBuf {
    cli_path.unwrap_or_else(|| {
        std::env::current_dir().unwrap().join("state.json")
    })
}

const RELEASES_URL: &str =
    "https://raw.githubusercontent.com/paritytech/release-registry/main/releases-v1.json";

/// Fetch releases-v1.json from the release-registry GitHub repo (public, no auth needed).
async fn fetch_releases_json() -> Result<releases::ReleasesJson> {
    log::info!("Fetching releases-v1.json from release-registry");
    reqwest::get(RELEASES_URL)
        .await
        .context("failed to fetch releases-v1.json")?
        .json()
        .await
        .context("failed to parse releases-v1.json")
}

/// Entry point: parse CLI args, load state, run pipeline steps, save state.
#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    let token = std::env::var("GITHUB_TOKEN").context("GITHUB_TOKEN env var required")?;
    let state_path = resolve_state_path(cli.state_path);
    let gh = github::GitHubClient::new(token);

    log::info!("Loading state from {}", state_path.display());
    let mut state = state::State::load(&state_path)?;

    if let Some(ref filter) = cli.runtime {
        let filter_lower = filter.to_lowercase();
        let before = state.runtimes.len();
        state.runtimes.retain(|rt| rt.field_name.to_lowercase().contains(&filter_lower));
        anyhow::ensure!(
            !state.runtimes.is_empty(),
            "no runtime matching '{}' (had {} runtimes)", filter, before,
        );
        log::info!("Filtered to {} runtime(s) matching '{}'", state.runtimes.len(), filter);
    }

    let releases_json = fetch_releases_json().await?;

    let single;
    let steps: &[Step] = match cli.step {
        Some(Step::Annotate) => &[Step::Downstream, Step::Annotate],
        Some(step) => { single = [step]; &single },
        None => Step::ALL,
    };

    // Bootstrap (blank state) -> annotate all PRs; incremental -> only dirty ones
    let dirty_prs = if state.last_processed_tag.is_some() {
        Some(HashSet::new())
    } else {
        None
    };

    let runner = Runner { gh, state, state_path, releases_json, dry_run: cli.dry_run, verbose: cli.verbose, sdk_repo: cli.sdk_repo, dirty_prs };
    runner.run(steps).await
}
