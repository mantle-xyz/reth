//! Build script that derives Mantle version metadata from the checked-out ref.

use std::{
    env, fs,
    path::{MAIN_SEPARATOR, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-env-changed=MANTLE_BUILD_REF");
    println!("cargo:rerun-if-env-changed=MANTLE_BUILD_SHA");
    watch_git_metadata();

    let git_sha = run_git(&["rev-parse", "HEAD"]);
    let supplied_sha = env::var("MANTLE_BUILD_SHA").ok().filter(|sha| !sha.is_empty());
    if let (Some(head), Some(supplied)) = (&git_sha, &supplied_sha) {
        assert!(head.starts_with(supplied), "MANTLE_BUILD_SHA does not match Git HEAD");
    }
    let sha = git_sha.as_deref().or(supplied_sha.as_deref()).expect("Git SHA is required");
    assert!(sha.len() >= 7 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()), "invalid Git SHA");
    let sha_short = &sha[..7];

    let supplied_ref_raw =
        env::var("MANTLE_BUILD_REF").ok().filter(|ref_name| !ref_name.is_empty());
    let supplied_ref = supplied_ref_raw.as_deref().map(strip_ref_prefix);
    if let Some(ref_name) = supplied_ref {
        assert!(!ref_name.contains(['\n', '\r']), "invalid Git ref");
    }
    let supplied_is_tag = supplied_ref_raw.as_deref().is_some_and(|raw| {
        raw.starts_with("refs/tags/") ||
            (!raw.starts_with("refs/heads/") &&
                run_git(&[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/tags/{}", strip_ref_prefix(raw)),
                ])
                .is_some())
    });

    let tags = run_git(&["tag", "--points-at", "HEAD", "--list", "mantle-v*"]).unwrap_or_default();
    let tags: Vec<_> = tags.lines().filter(|tag| !tag.is_empty()).collect();
    assert!(tags.len() <= 1, "multiple mantle-v tags point at HEAD");
    let exact_tag = tags.first().copied();

    if let Some(ref_name) = supplied_ref.filter(|ref_name| ref_name.starts_with("mantle-v")) {
        if let Some(tag_sha) = run_git(&["rev-parse", &format!("refs/tags/{ref_name}^{{commit}}")])
        {
            assert_eq!(git_sha.as_deref(), Some(tag_sha.as_str()), "Mantle tag is not at Git HEAD");
        }
        assert!(
            exact_tag.is_none() || exact_tag == Some(ref_name),
            "conflicting Mantle tag at Git HEAD"
        );
    }

    let version = if let Some(tag) = exact_tag {
        tag.to_string()
    } else if let Some(ref_name) = supplied_ref {
        if ref_name.starts_with("mantle-v") {
            ref_name.to_string()
        } else if supplied_is_tag || ref_name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            "dev".to_string()
        } else {
            normalize_branch(ref_name)
        }
    } else if let Some(branch) = run_git(&["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        normalize_branch(&branch)
    } else {
        "dev".to_string()
    };

    println!("cargo:rustc-env=MANTLE_VERSION={version}");
    println!("cargo:rustc-env=MANTLE_GIT_SHA_SHORT={sha_short}");
    println!("cargo:rustc-env=MANTLE_GIT_SHA={sha}");

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

fn watch_git_metadata() {
    let Some(git_dir) = run_git(&["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    let git_dir = PathBuf::from(git_dir);
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());

    if let Some(common_dir) =
        run_git(&["rev-parse", "--git-common-dir"]).and_then(|path| fs::canonicalize(path).ok())
    {
        for path in ["refs/heads", "refs/tags", "packed-refs"] {
            println!("cargo:rerun-if-changed={}", common_dir.join(path).display());
        }
    }
}

fn strip_ref_prefix(ref_name: &str) -> &str {
    ref_name
        .strip_prefix("refs/heads/")
        .or_else(|| ref_name.strip_prefix("refs/tags/"))
        .unwrap_or(ref_name)
}

fn normalize_branch(branch: &str) -> String {
    let name: String = branch
        .chars()
        .map(
            |ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') { ch } else { '-' }
            },
        )
        .collect();
    let name = name.trim_matches('-');
    if name.is_empty() { "dev".to_string() } else { name.to_string() }
}
