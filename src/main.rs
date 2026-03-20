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
    const ALL: &[Step] = &[Step::Discover, Step::Downstream, Step::Onchain, Step::Annotate];
}

/// CLI arguments.
#[derive(Parser)]
#[clap(name = "tracker", about = "PR Deployment Tracker for polkadot-sdk")]
struct Cli {
    /// Run without modifying state or GitHub project
    #[clap(long)]
    dry_run: bool,

    /// Run only a specific step
    #[clap(long)]
    step: Option<Step>,

    /// Path to state.json (default: ./state.json)
    #[clap(long)]
    state_path: Option<PathBuf>,

    /// Path to a local polkadot-sdk git checkout
    #[clap(long, env = "POLKADOT_SDK_DIR")]
    sdk_repo: PathBuf,
}

struct Runner {
    gh: github::GitHubClient,
    state: state::State,
    state_path: PathBuf,
    releases_json: releases::ReleasesJson,
    dry_run: bool,
    sdk_repo: PathBuf,
    /// PRs that need annotation this run. None means all PRs (bootstrap).
    dirty_prs: Option<HashSet<u64>>,
}

impl Runner {
    async fn run(mut self, steps: &[Step]) -> Result<()> {
        for &step in steps {
            self.run_step(step).await?;
            self.save_state()?;
        }
        Ok(())
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

    /// Collect all PRs from all releases.
    fn all_prs(&self) -> HashSet<u64> {
        self.state.releases.iter()
            .flat_map(|r| r.crates.iter().flat_map(|c| &c.prs))
            .copied()
            .collect()
    }

    async fn run_step(&mut self, step: Step) -> Result<()> {
        match step {
            Step::Discover => {
                let new_tags = releases::discover_and_resolve(
                    &mut self.state, &self.releases_json, &self.sdk_repo,
                )?;
                if !new_tags.is_empty() {
                    let new_prs = self.prs_from_tags(&new_tags);
                    log::info!("{} PRs from {} new release tag(s)", new_prs.len(), new_tags.len());
                    match &mut self.dirty_prs {
                        Some(set) => set.extend(new_prs),
                        None => {} // None means all — already covers these
                    }
                }
                Ok(())
            }
            Step::Downstream => {
                let changed = downstream::check_downstream(&mut self.state, &self.gh).await?;
                if changed {
                    // Downstream state changed — all PRs need status recomputation
                    self.dirty_prs = None;
                }
                Ok(())
            }
            Step::Onchain => onchain::check_onchain(&mut self.state.runtimes).await,
            Step::Annotate => {
                project::annotate(&self.state, &self.gh, self.dry_run, self.dirty_prs.as_ref()).await
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
    let state = state::State::load(&state_path)?;
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

    let runner = Runner { gh, state, state_path, releases_json, dry_run: cli.dry_run, sdk_repo: cli.sdk_repo, dirty_prs };
    runner.run(steps).await
}
