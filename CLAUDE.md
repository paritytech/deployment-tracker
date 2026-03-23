# deployment-tracker

PR Deployment Tracker for polkadot-sdk releases. Tracks when PRs reach downstream runtimes and go live on-chain.

## Specs

`SPECS.md` is the authoritative design document. Keep it in sync with the code when making major changes to the pipeline, step ordering, state format, or annotation logic.

## Build & Test

```sh
cargo check
cargo test
cargo +nightly fmt
```

## Dry-run

```sh
cargo run -- --dry-run --sdk-repo /path/to/polkadot-sdk
```

Prints per-step summary tables (Release Discovery, Onchain Discovery, Runtime Discovery, PRs to Annotate). The annotation stats are computed from the same code path used for actual GitHub Project updates.
