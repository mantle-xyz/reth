# Mantle op-reth Build System

GIT_SHA := `git rev-parse HEAD`
GIT_TAG := `git describe --tags --exact-match --match 'mantle-v*' 2>/dev/null || echo "dev-$(git rev-parse --short=7 HEAD)"`
BIN_DIR := "dist/bin"
CARGO_TARGET_DIR := env("CARGO_TARGET_DIR", "target")

# Default features: jemalloc + asm-keccak + min-debug-logs (no jemalloc on Windows)
FEATURES := env("FEATURES", if os() == "windows" { "asm-keccak min-debug-logs" } else { "jemalloc asm-keccak min-debug-logs" })

# Mantle debug features: adds state-export for root mismatch debugging
MANTLE_DEBUG_FEATURES := env("MANTLE_DEBUG_FEATURES", FEATURES + " state-export")

PROFILE := env("PROFILE", "release")

# Docker image name (override in CI)
DOCKER_IMAGE_NAME := env("DOCKER_IMAGE_NAME", "ghcr.io/mantle-xyz/op-reth")

# The proxy ships as its own image, not as a second binary inside the node's
DOCKER_WS_PROXY_IMAGE_NAME := env("DOCKER_WS_PROXY_IMAGE_NAME", "ghcr.io/mantle-xyz/mantle-ws-proxy")

# default recipe
default:
  @just --list

# ==================== Build ====================

# Build op-reth binary (release)
build:
  cargo build -p mantle-reth-cli --bin op-reth --features "{{FEATURES}}" --profile "{{PROFILE}}"

# Alias: backward-compatible with old Makefile `make build-op`
build-op: build

# `mantle-ws-proxy` is not in `default-members`, so a bare `cargo build` never
# produces it. Name the recipe rather than leaving the `-p` incantation as
# folklore: the devnet's flashblocks tests expect the binary at
# `target/release/mantle-ws-proxy`.

# Build the flashblocks relay binary (release)
build-ws-proxy:
  cargo build -p mantle-reth-ws-proxy --bin mantle-ws-proxy --profile "{{PROFILE}}"

# Build both binaries
build-all: build build-ws-proxy

# Build op-reth binary (debug, with Mantle state-export feature)
build-debug:
  cargo build -p mantle-reth-cli --bin op-reth --features "{{MANTLE_DEBUG_FEATURES}}"

# Build op-reth with maximum performance optimisations
build-maxperf:
  RUSTFLAGS="-C target-cpu=native" cargo build -p mantle-reth-cli --profile maxperf --features "jemalloc asm-keccak" --bin op-reth

# Build op-reth with profiling symbols
build-profiling:
  RUSTFLAGS="-C target-cpu=native" cargo build -p mantle-reth-cli --profile profiling --features "jemalloc asm-keccak" --bin op-reth

# Build and install op-reth to $CARGO_HOME/bin
install:
  cargo install --path mantle-reth/crates/cli --bin op-reth --force --locked \
    --features "{{FEATURES}}" \
    --profile "{{PROFILE}}"

# ==================== Cross-compilation ====================

# Build op-reth natively for a specific target
build-native target:
  cargo build -p mantle-reth-cli --bin op-reth --target {{target}} --features "{{FEATURES}}" --profile "{{PROFILE}}"

# Cross-compile op-reth for a specific target (requires `cross` and Docker)
build-cross target:
  #!/usr/bin/env bash
  set -euo pipefail
  features="{{FEATURES}}"
  env_args=()
  if [[ "{{target}}" == "aarch64-unknown-linux-gnu" ]]; then
    env_args+=(JEMALLOC_SYS_WITH_LG_PAGE=16)
  fi
  if [[ "{{target}}" == "x86_64-pc-windows-gnu" ]]; then
    features=$(echo "$features" | sed 's/jemalloc-prof//g; s/jemalloc//g' | xargs)
  fi
  # Ubuntu 24.04 containers need:
  # - LIBCLANG_PATH: use system clang-18, not cross's bundled libclang 3.8
  #   libclang-dev installs libclang-18.so in /usr/lib/x86_64-linux-gnu/
  # - BINDGEN_EXTRA_CLANG_ARGS: gcc-13 stdarg.h path (clang-18 package misses it)
  # CROSS_CONTAINER_OPTS injects -e flags into docker run
  export CROSS_CONTAINER_OPTS="${CROSS_CONTAINER_OPTS:--e LIBCLANG_PATH=/usr/lib/x86_64-linux-gnu -e BINDGEN_EXTRA_CLANG_ARGS=-I/usr/lib/gcc/x86_64-linux-gnu/13/include}"
  env "${env_args[@]}" \
    RUSTFLAGS="-C link-arg=-lgcc -Clink-arg=-static-libgcc" \
    cross build -p mantle-reth-cli --bin op-reth --target {{target}} --features "$features" --profile "{{PROFILE}}"

# Shorthand targets
build-x86_64-unknown-linux-gnu: (build-cross "x86_64-unknown-linux-gnu")
build-aarch64-unknown-linux-gnu: (build-cross "aarch64-unknown-linux-gnu")
build-x86_64-apple-darwin: (build-native "x86_64-apple-darwin")
build-aarch64-apple-darwin: (build-native "aarch64-apple-darwin")

# Build release tarballs for Linux targets
build-release-tarballs:
  #!/usr/bin/env bash
  set -euo pipefail
  mkdir -p {{BIN_DIR}}
  just build-x86_64-unknown-linux-gnu
  cp {{CARGO_TARGET_DIR}}/x86_64-unknown-linux-gnu/{{PROFILE}}/op-reth {{BIN_DIR}}/op-reth
  (cd {{BIN_DIR}} && tar -czf op-reth-{{GIT_TAG}}-x86_64-unknown-linux-gnu.tar.gz op-reth && rm op-reth)
  just build-aarch64-unknown-linux-gnu
  cp {{CARGO_TARGET_DIR}}/aarch64-unknown-linux-gnu/{{PROFILE}}/op-reth {{BIN_DIR}}/op-reth
  (cd {{BIN_DIR}} && tar -czf op-reth-{{GIT_TAG}}-aarch64-unknown-linux-gnu.tar.gz op-reth && rm op-reth)

# ==================== Quality ====================

# Type-check all workspace crates
check:
  cargo check --workspace

# Format code (requires nightly)
fmt:
  cargo +nightly fmt --all

# Run clippy lints (stable, matching upstream)
clippy:
  cargo clippy \
    --workspace \
    --lib --examples --tests --benches \
    --all-features \
    -- -D warnings

# Run all linters (fmt + clippy)
lint: fmt clippy

# PR-tier tests: every test that runs in well under a minute — workspace unit tests
# plus ALL Mantle integration suites (offline `replay`/`token_ratio_midblock` and the
# node-spawning `it` harness, incl. the estimate_total_fee token_ratio regression).
#
# `--lib --bins`, not `--lib`: `--lib` never compiles a crate's `main.rs`, so a bin
# crate's own `mod tests` (ws-proxy's startup-argument checks live there) would run
# nowhere and the gap would not show — the run still prints `N passed`.
#
# Separate invocations on purpose: `-p mantle-reth-integration-tests --tests` runs
# *every* test target of that crate, so newly-added suites are covered automatically
# without editing a name list — and it is scoped to the Mantle crate, so it does NOT
# pull in the ~24 upstream op-reth integration tests a bare `--workspace --tests`
# would. The price is paid in CI: the `-p` invocations resolve features differently
# from `--workspace`, so each recompiles a slice of alloy/revm/reth (measured on
# 2026-10-16, run 38862570322: 889 + 317 + 202 crates across the first three).
#
# Doctests run here too, but with DEFAULT features so they reuse the codegen from the
# `--lib` build above (~30s incremental). The exhaustive `--all-features` doctest pass
# is a full from-scratch build (~11min) and stays in nightly (`just test-doc`).
#
# The node-spawning integration tests run under nextest; its retries and JUnit
# report are set in `.config/nextest.toml`, so a pass on a retry is reported the
# same way here and in CI (`FLAKY` in the summary, a warning on the CI run page).
# Doctests stay on `cargo test` (nextest can't run them).
#
# CI wall clock, same run: 57 min, of which ~54 min is compilation and 2.6 min is
# nextest running its 267 tests. `ci.yml`'s `timeout-minutes` is sized against the
# compile, not the test count.
test-ci:
  cargo test --workspace --lib --bins
  cargo test -p mantle-reth-cli --test build_version
  cargo nextest run -p mantle-reth-integration-tests --tests
  cargo test --doc --workspace

# Run the full local test suite (examples + benches + all features)
test:
  cargo test --workspace --lib --examples --tests --benches --all-features

# Run documentation tests
test-doc:
  cargo test --doc --workspace --all-features

# Tests quarantined with `#[ignore]` for failing under parallel load only. Run
# serially (`--test-threads=1`) they are deterministic; nightly runs this.
test-quarantined:
  cargo test -p mantle-reth-integration-tests --test preconf -- --ignored --test-threads=1

# Full pre-PR check: lint + CI tests + doc tests (use `just test` for the exhaustive suite)
pr: lint test-ci test-doc

# ==================== Docker ====================

# Build and push multi-arch Docker image with given tags
docker-build-push-tags build_tag push_tag features=FEATURES:
  #!/usr/bin/env bash
  set -euo pipefail
  FEATURES="{{features}}" just build-x86_64-unknown-linux-gnu
  mkdir -p {{BIN_DIR}}/amd64
  cp {{CARGO_TARGET_DIR}}/x86_64-unknown-linux-gnu/{{PROFILE}}/op-reth {{BIN_DIR}}/amd64/op-reth
  FEATURES="{{features}}" just build-aarch64-unknown-linux-gnu
  mkdir -p {{BIN_DIR}}/arm64
  cp {{CARGO_TARGET_DIR}}/aarch64-unknown-linux-gnu/{{PROFILE}}/op-reth {{BIN_DIR}}/arm64/op-reth
  docker buildx build --file ./op-reth/DockerfileOp.cross . \
    --platform linux/amd64,linux/arm64 \
    --tag {{DOCKER_IMAGE_NAME}}:{{build_tag}} \
    --tag {{DOCKER_IMAGE_NAME}}:{{push_tag}} \
    --provenance=false \
    --push

# Docker: tag with git tag
docker-build-push: (docker-build-push-tags GIT_TAG GIT_TAG)

# Docker: tag with git SHA
docker-build-push-git-sha: (docker-build-push-tags GIT_SHA GIT_SHA)

# Docker: tag with latest
docker-build-push-latest: (docker-build-push-tags GIT_TAG "latest")

# `mantle-ws-proxy` is delivered as its own image: a stateless relay that is
# scaled and restarted independently of any node. It builds from source in the
# image (`DockerfileWsProxy`), the same way the deployment pipeline builds
# `DockerfileOp` — not from a pre-cross-compiled binary like the `.cross` path
# above, which exists for multi-arch pushes the proxy does not yet need.

# Build the standalone mantle-ws-proxy image (single arch, local)
docker-build-ws-proxy tag=GIT_TAG:
  docker build --file ./DockerfileWsProxy . \
    --build-arg VERSION={{tag}} \
    --build-arg GIT_COMMIT={{GIT_SHA}} \
    --tag {{DOCKER_WS_PROXY_IMAGE_NAME}}:{{tag}}

# ==================== Misc ====================

# Clean build artifacts
clean:
  cargo clean
  rm -rf {{BIN_DIR}}
