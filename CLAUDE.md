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
parallel on every PR to `main`, `dev/**` or `release/**`, and on every push to
those branches. Running `just pr` first avoids round-trips waiting on CI.

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

## Building the binary — always name the package

**Use `just build`, or pass `-p mantle-reth-cli` / `--manifest-path
mantle-reth/crates/cli/Cargo.toml`. Never build `op-reth` from the workspace root
without naming the package.**

Two workspace members declare a `[[bin]]` called `op-reth`:

| Package | Path | Ships? |
|---------|------|--------|
| `mantle-reth-cli` | `mantle-reth/crates/cli/` | **yes** — this is the node we deploy |
| `op-reth` | `op-reth/bin/` | no — upstream's binary, vendored for parity |

Both write to `target/<profile>/op-reth`. Cargo does **not** treat this as an
error: it emits `warning: output filename collision` and lets the two link steps
race, so `cargo build --workspace` and `cargo build --bin op-reth` produce
**whichever finished last** (measured on cargo 1.95.0: five clean rebuilds
alternated between the two). See rust-lang/cargo#6313 — it may become a hard
error some day, but today it is only a warning that is easy to scroll past.

Picking up the wrong one fails loudly rather than quietly: upstream's parser has
no `mantle` chain, so `--chain mantle` is rejected at startup. `MantleChainSpecParser`
(`mantle`, `mantle-mainnet`, `mantle-sepolia`) lives in `mantle-reth/crates/cli`.

Both Dockerfiles already disambiguate via `--manifest-path`; the root
`DockerfileOp` is the one that builds the shipped binary.

`op-reth/bin/` is deliberately kept and deliberately left in `members`: deleting
it would be a permanent deviation to re-resolve on every upstream sync, and
keeping it compiled proves our edits to `op-reth/crates/*` still satisfy
upstream's own consumer. Renaming its bin target would be a deviation too — hence
this rule instead.

## Useful recipes

| Recipe | What it does |
|--------|--------------|
| `just check` | `cargo check --workspace` (fast type-check) |
| `just test` | Exhaustive local suite (examples + benches + all features) |
| `just build` | Build the shipped `op-reth` binary (`-p mantle-reth-cli`) |

Run `just --list` to see all available recipes.
