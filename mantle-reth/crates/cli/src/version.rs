//! Mantle version metadata injection.
//!
//! Overrides the default reth version strings with Mantle-specific values
//! derived from git tags at build time.  The version is fully automatic:
//!
//! - On a tag `op-reth-v2.2.1-mantle-arsia.1` → version is that tag
//! - Off tag → appends `-dev`
//! - No tag at all → falls back to short commit SHA + `-dev`
//!
//! Call [`init_mantle_version`] **before** `Cli::parse()` so the clap
//! `--version` flag and startup log use the Mantle values.

use reth_node_core::version::{
    RethCliVersionConsts, default_reth_version_metadata, try_init_version_metadata,
};
use std::borrow::Cow;

/// The human-readable client name used in `--version` and P2P handshake.
pub const MANTLE_CLIENT_NAME: &str = "Mantle-Reth";

/// Overrides the global reth version metadata with Mantle-specific values.
///
/// Must be called **before** `Cli::parse()` so the clap `--version` flag
/// and startup log use the Mantle version.
pub fn init_mantle_version() {
    let version = env!("MANTLE_VERSION");
    let sha = env!("MANTLE_GIT_SHA_SHORT");
    let profile = env!("MANTLE_BUILD_PROFILE");

    let defaults = default_reth_version_metadata();

    let _ = try_init_version_metadata(RethCliVersionConsts {
        name_client: Cow::Borrowed(MANTLE_CLIENT_NAME),
        cargo_pkg_version: Cow::Borrowed(version),
        short_version: Cow::Owned(format!("{version} ({sha})")),
        long_version: Cow::Owned(format!(
            "Version: {version}\n\
             Commit SHA: {sha}\n\
             Build Timestamp: {}\n\
             Build Features: {}\n\
             Build Profile: {profile}",
            defaults.vergen_build_timestamp, defaults.vergen_cargo_features,
        )),
        p2p_client_version: Cow::Owned(format!(
            "mantle-reth/{version}/{}",
            defaults.vergen_cargo_target_triple,
        )),
        extra_data: Cow::Owned(format!("mantle-reth/{version}/{}", std::env::consts::OS,)),
        vergen_git_sha: Cow::Borrowed(sha),
        ..defaults
    });
}
