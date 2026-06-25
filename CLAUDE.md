# Contributing to Mantle op-reth

## Before opening a PR

Run the full pre-PR check locally and make sure it is green:

```bash
just pr
```

`just pr` runs the same gates CI enforces, in order:

1. `just lint` — `cargo +nightly fmt --all` + `cargo clippy --workspace --all-features -D warnings`
2. `just test-ci` — hermetic unit + integration + replay tests (`cargo test --workspace --lib --tests`)
3. `just test-doc` — documentation tests

CI (`.github/workflows/ci.yml`) runs `lint` and `test` (`just test-ci`) in
parallel on every PR to `mantle-elysium` and `main`. Running `just pr` first
avoids round-trips waiting on CI.

## Useful recipes

| Recipe | What it does |
|--------|--------------|
| `just check` | `cargo check --workspace` (fast type-check) |
| `just test-replay` | Offline mainnet-replay fixtures only (sub-second) |
| `just test` | Exhaustive local suite (examples + benches + all features) |
| `just build` | Build the `op-reth` release binary |

Run `just --list` to see all available recipes.
