//! Build script that derives Mantle version metadata from git tags.

use std::{env, path::MAIN_SEPARATOR, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/tags/");

    let sha_short = run_git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=MANTLE_GIT_SHA_SHORT={sha_short}");

    // git describe --always --tags --match 'op-reth-v*'
    //   On tag:     "op-reth-v2.2.1-mantle-arsia.1"
    //   Off tag:    "op-reth-v2.2.1-mantle-arsia.1-5-gd158cf04"
    //   No tag yet: "d158cf04"
    let describe =
        run_git(&["describe", "--always", "--tags", "--match", "op-reth-v*"]).unwrap_or_default();

    let is_dirty = Command::new("git")
        .args(["diff-index", "--quiet", "HEAD"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false);

    // If describe contains "-g" followed by a hex suffix, we're not exactly on a tag.
    let not_on_tag = describe.contains("-g") && describe.len() > sha_short.len();
    let suffix = if not_on_tag || is_dirty { "-dev" } else { "" };

    // Extract the base tag (strip trailing "-N-gSHA" if present)
    let base_tag = if not_on_tag {
        // "op-reth-v2.2.1-mantle-arsia.1-5-gd158cf04" → "op-reth-v2.2.1-mantle-arsia.1"
        // Find the last "-g" followed by hex digits
        describe
            .rfind("-g")
            .and_then(|g_pos| {
                // Also strip the commit count before "-g": find the dash before the count
                describe[..g_pos].rfind('-').map(|dash_pos| &describe[..dash_pos])
            })
            .unwrap_or(&describe)
    } else {
        &describe
    };

    let version = format!("{base_tag}{suffix}");
    println!("cargo:rustc-env=MANTLE_VERSION={version}");

    // Build profile from OUT_DIR (same trick as reth-node-core)
    let out_dir = env::var("OUT_DIR").unwrap_or_default();
    let profile = out_dir.rsplit(MAIN_SEPARATOR).nth(3).unwrap_or("unknown");
    println!("cargo:rustc-env=MANTLE_BUILD_PROFILE={profile}");
}

fn run_git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}
