# Tracker

*Tracks when polkadot-sdk PRs reach downstream runtimes and go live on-chain.*

Tracker monitors [Polkadot SDK](https://github.com/paritytech/polkadot-sdk) releases, checks whether downstream runtimes have adopted the changes, verifies on-chain deployment, and annotates a GitHub Project with per-PR deployment status.

![PR fields before and after tracking](https://raw.githubusercontent.com/pgherveou/design-doc/gas-sponsoring/release-process/project-overview.png)

## How It Works

Tracker runs a four-step pipeline, each step building on the previous:

1. **Discover** -- reads [`releases-v1.json`](https://github.com/paritytech/release-registry/blob/main/releases-v1.json) to find new release tags, extracts crate version bumps and maps them to PRs via commit messages and prdoc files
2. **Onchain** -- connects to live chains via WebSocket RPC, binary-searches for runtime upgrade blocks, and records spec version, block number, and timestamp
3. **Downstream** -- fetches `Cargo.lock` / `Cargo.toml` from downstream runtime repos (e.g. [paseo-network/runtimes](https://github.com/paseo-network/runtimes)) to check which crate versions have been adopted
4. **Annotate** -- updates a GitHub Project V2, tagging each PR with its release and a per-runtime deployment status:

| Status | Meaning |
|--------|---------|
| *(empty)* | Crates not yet picked up downstream |
| `pending > v{spec}` | Picked up, spec version not yet bumped |
| `pending v{spec}` | Spec bumped in code, not yet enacted on-chain |
| `v{spec}` | Live on-chain |

Partial adoption is shown as a suffix, e.g. `v1002300 (2/3 crates)`.

![Status examples across the PR lifecycle](https://raw.githubusercontent.com/pgherveou/design-doc/gas-sponsoring/release-process/project-status-examples.png)

## Sample Output

Every run prints per-step summary tables:

```
┌───────────────────┬───────────────────────────┐
│ Release Discovery ┆                           │
╞═══════════════════╪═══════════════════════════╡
│ Latest known      ┆ polkadot-stable2512-2     │
│ New releases      ┆ none                      │
└───────────────────┴───────────────────────────┘

┌─────────────────────┬──────────┬──────────┬──────────┐
│ Onchain Discovery   ┆ Previous ┆ Current  ┆ Pending  │
╞═════════════════════╪══════════╪══════════╪══════════╡
│ AH Paseo            ┆ v2000006 ┆ v2000006 ┆ v2001000 │
│ AH Kusama           ┆ v2001000 ┆ v2001000 ┆ v2001001 │
│ AH Polkadot         ┆ v2000007 ┆ v2001001 ┆ -        │
└─────────────────────┴──────────┴──────────┴──────────┘

┌───────────────────┬──────────┬───────────┬───────────────┐
│ Runtime Discovery ┆ Current  ┆ Code Spec ┆ Crate Updates │
╞═══════════════════╪══════════╪═══════════╪═══════════════╡
│ AH Paseo          ┆ v2000006 ┆ v2001000  ┆ 126           │
│ AH Kusama         ┆ v2001000 ┆ v2001001  ┆ 118           │
│ AH Polkadot       ┆ v2001001 ┆ v2001001  ┆ -             │
└───────────────────┴──────────┴───────────┴───────────────┘

┌─────────────────┬──────────┬─────┐
│ PRs to Annotate ┆ Version  ┆ PRs │
╞═════════════════╪══════════╪═════╡
│ AH Paseo        ┆ v2001000 ┆ 420 │
│ AH Kusama       ┆ v2001001 ┆ 380 │
│ AH Polkadot     ┆ v2001001 ┆ 512 │
└─────────────────┴──────────┴─────┘
```

With `--verbose`, each annotated PR is listed with its per-runtime status:

```
  #9002 AH Paseo: pending v2001000, AH Kusama: pending v2001001, AH Polkadot: v2001001
  #9063 AH Paseo: pending v2001000 (1/2 crates), AH Kusama: pending v2001001 (1/2 crates)
  #9279 AH Paseo: pending v2001000 (3/4 crates), AH Polkadot: v2001001 (3/4 crates)
```

## Quick Start

<details>
<summary>Prerequisites</summary>

- Rust 1.70+
- A `GITHUB_TOKEN` with access to the target repos and project
- A local [polkadot-sdk](https://github.com/paritytech/polkadot-sdk) git checkout

</details>

```bash
cargo build --release
```

## Usage

```bash
# Run the full pipeline
GITHUB_TOKEN=xxx ./target/release/tracker --sdk-repo /path/to/polkadot-sdk

# Preview without modifying state or GitHub
GITHUB_TOKEN=xxx ./target/release/tracker --sdk-repo /path/to/polkadot-sdk --dry-run

# Show per-PR annotation details
GITHUB_TOKEN=xxx ./target/release/tracker --sdk-repo /path/to/polkadot-sdk --verbose

# Run a single step
GITHUB_TOKEN=xxx ./target/release/tracker --sdk-repo /path/to/polkadot-sdk --step discover

# Filter to a single runtime
GITHUB_TOKEN=xxx ./target/release/tracker --sdk-repo /path/to/polkadot-sdk --runtime paseo
```

You can set `POLKADOT_SDK_DIR` instead of passing `--sdk-repo` each time.

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `--sdk-repo <PATH>` | `$POLKADOT_SDK_DIR` | Path to a local polkadot-sdk git checkout (required) |
| `--dry-run` | `false` | Run without writing state or updating GitHub |
| `--verbose` | `false` | Print per-PR annotation details with clickable links |
| `--runtime <FILTER>` | all | Filter to runtimes matching this name (e.g. "paseo", "AH Kusama") |
| `--step <STEP>` | all | Run only one step: `discover`, `onchain`, `downstream`, `annotate` |
| `--state-path <PATH>` | `./state.json` | Path to the persistent state file |

## Configuration

All configuration lives in [`state.json`](./state.json). It defines which GitHub Project to annotate and which downstream runtimes to track:

```jsonc
{
  "project": { "org": "paritytech", "number": 274 },
  "runtimes": [
    {
      "runtime": "Asset Hub",
      "short": "AH",
      "repo": "paseo-network/runtimes",
      "branch": "main",
      "network": "Paseo",
      "ws": "wss://sys.ibp.network/asset-hub-paseo",
      "field_name": "AH Paseo"
      // ...
    }
  ]
}
```

## Testing

```bash
cargo test
```
