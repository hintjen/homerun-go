fn main() {
    napi_build::setup();
    println!("cargo:rerun-if-env-changed=HOMERUN_CORE_BUILD_ID");
    // A source identity, not a digest of the final signed binary. Embedding
    // a binary's own digest is recursive; the manifest carries that digest.
    let source = std::env::var("HOMERUN_CORE_BUILD_ID")
        .ok()
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    assert!(
        !source.is_empty()
            && source
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
        "HOMERUN_CORE_BUILD_ID must be a nonempty plain build identifier"
    );
    println!("cargo:rustc-env=HOMERUN_CORE_BUILD_ID={source}");
    let mut refs = vec!["HEAD".to_owned()];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        refs.push(branch);
    }
    for name in refs {
        if let Some(path) = git(&["rev-parse", "--git-path", &name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
