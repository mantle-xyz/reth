# Contributing to Mantle op-reth

## Before opening a PR

Run the full pre-PR check locally and make sure it is green:

```bash
just pr
```

`just pr` runs the same gates CI enforces on every PR, in order:

1. `just lint` — `cargo +nightly fmt --all` + `cargo clippy --workspace --all-features -D warnings`
2. `just test-ci` — workspace unit tests + all Mantle integration tests (offline `replay`/`token_ratio_midblock` + the node-spawning `it` harness) + default-feature doctests
3. `just test-doc` — exhaustive `--all-features` doctests (full from-scratch build; PR CI relies on the lighter default-feature doctests inside `test-ci` instead)

CI (`.github/workflows/ci.yml`) runs `lint` and `test` (`just test-ci`) in
parallel on every PR to `mantle-stage2` and `main`. Running `just pr` first
avoids round-trips waiting on CI.

## Test tiers

The suite is split by **runtime**, not by "is it a node test": anything that runs
in well under a minute belongs in per-PR CI; only genuinely heavy work waits for
nightly.

- **PR tier** (`just test-ci`, every PR): workspace unit tests + **all** Mantle
  integration suites — the offline `replay`/`token_ratio_midblock` targets **and**
  the node-spawning `it` harness (`fill_transaction`, `gas_estimation`, `gas_limit`,
  `txpool`, `estimate_total_fee_token_ratio`). The `it` group runs in ~20s; spawning
  a node is cheap, so it is gated per-PR. `test-ci` uses
  `-p mantle-reth-integration-tests --tests`, so new suites are covered automatically.
  Doctests also run here with **default features** (~30s, reusing the `--lib` codegen).
- **Nightly tier** (`just test`, `.github/workflows/nightly.yml`, daily + manual
  `workflow_dispatch`): only the genuinely heavy matrix — `--all-features` (full
  feature-set rebuild), `--benches`, `--examples`, the exhaustive `--all-features`
  doctests (`just test-doc`, a ~11min from-scratch build), and the upstream op-reth
  integration tests.

If a node/integration test ever becomes flaky in PR CI, quarantine that single
test (`#[ignore]`) rather than moving the whole tier back to nightly.

## Useful recipes

| Recipe | What it does |
|--------|--------------|
| `just check` | `cargo check --workspace` (fast type-check) |
| `just test` | Exhaustive local suite (examples + benches + all features) |
| `just build` | Build the `op-reth` release binary |

Run `just --list` to see all available recipes.
