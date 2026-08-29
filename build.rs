//! Embeds the build's git revision, plus the Windows icon and version
//! metadata.

use std::process::Command;

fn main() {
    embed_git_revision();

    #[cfg(windows)]
    embed_windows_resources();
}

/// Stamps the commit the binary was built from into `CCP_GIT_SHA`.
///
/// The crate version only moves on a release, so it cannot answer "am I
/// running the build you just handed me". The revision can.
///
/// A checkout with uncommitted changes is marked, because the SHA alone would
/// otherwise name a commit whose code is not what is running. When git is
/// unavailable — a source tarball, a vendored build — the value is `unknown`
/// rather than a build failure.
fn embed_git_revision() {
    // Without these, a rebuild after a commit would keep the previous stamp:
    // cargo has no other reason to think this script's output changed.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    let revision = git(&["rev-parse", "--short", "HEAD"])
        .map(|sha| {
            let dirty = git(&["status", "--porcelain"]).is_some_and(|out| !out.is_empty());
            if dirty { format!("{sha}-dirty") } else { sha }
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=CCP_GIT_SHA={revision}");
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

#[cfg(windows)]
fn embed_windows_resources() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/icon.ico");
    resource.set("ProductName", "claude-code-proxy");
    resource.set(
        "FileDescription",
        "Anthropic-compatible proxy for Claude Code provider backends",
    );

    if let Err(error) = resource.compile() {
        // A missing resource compiler must not break the build; the binary is
        // perfectly usable without an icon.
        println!("cargo:warning=skipping Windows resources: {error}");
    }
}
