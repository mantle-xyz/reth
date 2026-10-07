//! Integration checks for Mantle's build metadata.

use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(repo).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn repository() -> TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.name", "Version Test"]);
    git(repo.path(), &["config", "user.email", "version@example.test"]);

    fs::write(repo.path().join("source.txt"), "first").unwrap();
    git(repo.path(), &["add", "source.txt"]);
    git(repo.path(), &["commit", "-qm", "first"]);
    git(repo.path(), &["tag", "op-reth-v2.2.1-mantle-arsia.2"]);

    fs::write(repo.path().join("source.txt"), "second").unwrap();
    git(repo.path(), &["add", "source.txt"]);
    git(repo.path(), &["commit", "-qm", "second"]);
    repo
}

fn run_build_script(repo: &Path, extra_env: &[(&str, &str)]) -> (bool, String) {
    let build = tempfile::tempdir().unwrap();
    let executable = build.path().join("version-build-script");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs");
    let compile = Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&executable)
        .arg(script)
        .output()
        .unwrap();
    assert!(compile.status.success(), "{}", String::from_utf8_lossy(&compile.stderr));

    let output = Command::new(executable)
        .current_dir(repo)
        .env("OUT_DIR", "target/release/build/mantle-reth-cli/out")
        .envs(extra_env.iter().copied())
        .output()
        .unwrap();
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn value<'a>(output: &'a str, key: &str) -> &'a str {
    let prefix = format!("cargo:rustc-env={key}=");
    output.lines().find_map(|line| line.strip_prefix(&prefix)).unwrap()
}

#[test]
fn exact_mantle_tag_wins_over_old_tag_and_branch() {
    let repo = repository();
    git(repo.path(), &["switch", "-qc", "dev/mantle-v1.6.3"]);
    git(repo.path(), &["tag", "mantle-v1.6.3"]);
    let sha = git(repo.path(), &["rev-parse", "HEAD"]);

    let (success, output) = run_build_script(repo.path(), &[]);
    assert!(success, "{output}");
    assert_eq!(value(&output, "MANTLE_VERSION"), "mantle-v1.6.3");
    assert_eq!(value(&output, "MANTLE_GIT_SHA_SHORT"), &sha[..7]);
    assert!(
        output.contains(&format!(
            "cargo:rerun-if-changed={}",
            repo.path().canonicalize().unwrap().join(".git/HEAD").display()
        )),
        "{output}"
    );
}

#[test]
fn untagged_branch_and_detached_head_have_distinct_names() {
    let repo = repository();
    git(repo.path(), &["switch", "-qc", "dev/mantle-v1.6.3"]);
    let (success, output) = run_build_script(repo.path(), &[]);
    assert!(success, "{output}");
    assert_eq!(value(&output, "MANTLE_VERSION"), "dev-mantle-v1.6.3");

    git(repo.path(), &["checkout", "-q", "--detach", "HEAD"]);
    let (success, output) = run_build_script(repo.path(), &[]);
    assert!(success, "{output}");
    assert_eq!(value(&output, "MANTLE_VERSION"), "dev");
}

#[test]
fn explicit_ref_and_sha_work_without_git() {
    let directory = tempfile::tempdir().unwrap();
    let (success, output) = run_build_script(
        directory.path(),
        &[("MANTLE_BUILD_REF", "dev/mantle-v1.6.3"), ("MANTLE_BUILD_SHA", "abcdef1234567890")],
    );
    assert!(success, "{output}");
    assert_eq!(value(&output, "MANTLE_VERSION"), "dev-mantle-v1.6.3");
    assert_eq!(value(&output, "MANTLE_GIT_SHA_SHORT"), "abcdef1");
}

#[test]
fn explicit_tag_and_sha_work_without_local_tag_metadata() {
    let repo = repository();
    let sha = git(repo.path(), &["rev-parse", "HEAD"]);
    let (success, output) = run_build_script(
        repo.path(),
        &[("MANTLE_BUILD_REF", "mantle-v1.6.3"), ("MANTLE_BUILD_SHA", &sha)],
    );
    assert!(success, "{output}");
    assert_eq!(value(&output, "MANTLE_VERSION"), "mantle-v1.6.3");
}

#[test]
fn exact_tag_is_used_when_ci_modifies_a_tracked_dockerfile() {
    let repo = repository();
    git(repo.path(), &["tag", "mantle-v1.6.3"]);
    fs::write(repo.path().join("source.txt"), "modified by build pipeline").unwrap();

    let (success, output) = run_build_script(repo.path(), &[]);
    assert!(success, "{output}");
    assert_eq!(value(&output, "MANTLE_VERSION"), "mantle-v1.6.3");
}

#[test]
fn old_op_reth_tag_does_not_become_the_mantle_version() {
    let repo = repository();
    git(repo.path(), &["tag", "op-reth-v9"]);
    git(repo.path(), &["checkout", "-q", "--detach", "HEAD"]);

    for ref_name in ["op-reth-v9", "refs/tags/op-reth-v9"] {
        let (success, output) = run_build_script(repo.path(), &[("MANTLE_BUILD_REF", ref_name)]);
        assert!(success, "{output}");
        assert_eq!(value(&output, "MANTLE_VERSION"), "dev");
    }
}

#[test]
fn mismatched_explicit_sha_is_rejected() {
    let repo = repository();
    let (success, output) = run_build_script(repo.path(), &[("MANTLE_BUILD_SHA", "deadbeef")]);
    assert!(!success, "{output}");
}

#[test]
fn default_rpc_identity_contains_name_sha_and_arch_os() {
    mantle_reth_cli::version::init_mantle_version();
    let metadata = reth_node_core::version::version_metadata();
    let expected = format!(
        "mantle-reth/{}-{}/{}-{}",
        env!("MANTLE_VERSION"),
        env!("MANTLE_GIT_SHA_SHORT"),
        std::env::consts::ARCH,
        std::env::consts::OS,
    );
    assert_eq!(metadata.p2p_client_version, expected);
}
