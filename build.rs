use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
    println!("cargo:rerun-if-changed=.git/HEAD");

    let version = std::env::var("GITHUB_REF_NAME")
        .ok()
        .filter(|tag| tag.starts_with('v') && !tag.trim().is_empty())
        .or_else(git_tag_at_head)
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));

    println!("cargo:rustc-env=GBANDIT_BUILD_VERSION={version}");
}

fn git_tag_at_head() -> Option<String> {
    let output = Command::new("git")
        .args(["tag", "--points-at", "HEAD", "--sort=-version:refname"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .map(str::trim)
        .find(|tag| tag.starts_with('v') && !tag.is_empty())
        .map(str::to_string)
}
