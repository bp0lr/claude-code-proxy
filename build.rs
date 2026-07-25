//! Embeds the Windows icon and version metadata into the executable.
//!
//! Only runs on a Windows host: `winresource` needs the resource compiler, and
//! the whole notion of an embedded icon resource is Windows-specific. Builds on
//! every other platform are untouched.

#[cfg(windows)]
fn main() {
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

#[cfg(not(windows))]
fn main() {}
