# Mantle op-reth Build System

GIT_SHA := `git rev-parse HEAD`
GIT_TAG := `git describe --tags --abbrev=0 2>/dev/null || echo "unknown"`
BIN_DIR := "dist/bin"
CARGO_TARGET_DIR := env("CARGO_TARGET_DIR", "target")

# Default features: jemalloc + asm-keccak + min-debug-logs (no jemalloc on Windows)
FEATURES := env("FEATURES", if os() == "windows" { "asm-keccak min-debug-logs" } else { "jemalloc asm-keccak min-debug-logs" })

# Mantle debug features: adds state-export for root mismatch debugging
MANTLE_DEBUG_FEATURES := env("MANTLE_DEBUG_FEATURES", FEATURES + " state-export")

PROFILE := env("PROFILE", "release")

# Docker image name (override in CI)
DOCKER_IMAGE_NAME := env("DOCKER_IMAGE_NAME", "ghcr.io/mantle-xyz/op-reth")

# default recipe
default:
  @just --list

# ==================== Build ====================

# Build op-reth binary (release)
build:
  cargo build --bin op-reth --features "{{FEATURES}}" --profile "{{PROFILE}}"

# Build op-reth binary (debug, with Mantle state-export feature)
build-debug:
  cargo build --bin op-reth --features "{{MANTLE_DEBUG_FEATURES}}"

# Build op-reth with maximum performance optimisations
build-maxperf:
  RUSTFLAGS="-C target-cpu=native" cargo build --profile maxperf --features "jemalloc asm-keccak" --bin op-reth

# Build op-reth with profiling symbols
build-profiling:
  RUSTFLAGS="-C target-cpu=native" cargo build --profile profiling --features "jemalloc asm-keccak" --bin op-reth

# Build and install op-reth to $CARGO_HOME/bin
install:
  cargo install --path op-reth/bin --bin op-reth --force --locked \
    --features "{{FEATURES}}" \
    --profile "{{PROFILE}}"

# ==================== Cross-compilation ====================

# Build op-reth natively for a specific target
build-native target:
  cargo build --bin op-reth --target {{target}} --features "{{FEATURES}}" --profile "{{PROFILE}}"

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
  env "${env_args[@]}" \
    RUSTFLAGS="-C link-arg=-lgcc -Clink-arg=-static-libgcc" \
    cross build --bin op-reth --target {{target}} --features "$features" --profile "{{PROFILE}}"

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

# Run workspace unit tests
test:
  cargo test --workspace --lib --examples --tests --benches --all-features

# Run documentation tests
test-doc:
  cargo test --doc --workspace --all-features

# Full pre-PR check: lint + test
pr: lint test test-doc

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

# ==================== Misc ====================

# Clean build artifacts
clean:
  cargo clean
  rm -rf {{BIN_DIR}}
