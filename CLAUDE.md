# Contributing to Mantle op-reth

## Before opening a PR

Run the full pre-PR check locally and make sure it is green:

```bash
just pr
```

`just pr` runs the same gates CI enforces on every PR, in order:

1. `just lint` — `cargo +nightly fmt --all` + `cargo clippy --workspace --all-features -D warnings`
2. `just test-ci` — workspace unit tests + offline integration targets (`replay`, `token_ratio_midblock`)
3. `just test-doc` — documentation tests

CI (`.github/workflows/ci.yml`) runs `lint` and `test` (`just test-ci`) in
parallel on every PR to `mantle-elysium` and `main`. Running `just pr` first
avoids round-trips waiting on CI.

## Test tiers

To keep per-PR CI fast, the test suite is split into two tiers:

- **PR tier** (`just test-ci`, every PR): workspace unit tests + the offline
  integration targets (`replay`, `token_ratio_midblock`). No node spawning.
- **Nightly tier** (`just test`, `.github/workflows/nightly.yml`, daily +
  manual `workflow_dispatch`): the exhaustive suite, including the
  node-spawning `it` integration harness (`fill_transaction`, `gas_estimation`,
  `gas_limit`, `txpool`), benches, and `--all-features`.

Before a release — or whenever you touch node/RPC/txpool behavior — run the
full suite locally with `just test`, since those paths are only covered by the
nightly tier in CI.

## Useful recipes

| Recipe | What it does |
|--------|--------------|
| `just check` | `cargo check --workspace` (fast type-check) |
| `just test-replay` | Offline mainnet-replay fixtures only (sub-second) |
| `just test` | Exhaustive local suite (examples + benches + all features) |
| `just build` | Build the `op-reth` release binary |

Run `just --list` to see all available recipes.
