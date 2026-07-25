use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DirResolverEnv {
    pub platform: String,
    pub env: HashMap<String, String>,
    pub home: String,
}

impl Default for DirResolverEnv {
    fn default() -> Self {
        Self {
            platform: std::env::consts::OS.into(),
            env: std::env::vars().collect(),
            home: std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| "/".to_string()),
        }
    }
}

/// Accepts both the Rust (`std::env::consts::OS`) and the Node-style platform
/// names, because callers and tests use different spellings.
fn is_windows(platform: &str) -> bool {
    matches!(platform, "win32" | "windows")
}

fn is_macos(platform: &str) -> bool {
    matches!(platform, "darwin" | "macos")
}

pub fn resolve_config_dir(deps: &DirResolverEnv) -> PathBuf {
    if let Some(override_dir) = deps.env.get("CCP_CONFIG_DIR") {
        return Path::new(override_dir).to_path_buf();
    }

    if is_windows(&deps.platform) {
        let appdata = deps
            .env
            .get("APPDATA")
            .cloned()
            .unwrap_or_else(|| format!("{}\\AppData\\Roaming", deps.home));
        return join_with_sep(&appdata, &["claude-code-proxy"], true);
    }

    if is_macos(&deps.platform) {
        return join_with_sep(&deps.home, &[".config", "claude-code-proxy"], false);
    }

    let base = deps.env.get("XDG_CONFIG_HOME").cloned().unwrap_or_else(|| {
        join_with_sep(&deps.home, &[".config"], false)
            .to_string_lossy()
            .into_owned()
    });
    join_with_sep(&base, &["claude-code-proxy"], false)
}

pub fn resolve_state_dir(deps: &DirResolverEnv) -> PathBuf {
    // Un XDG_STATE_HOME explicito gana sobre el default de plataforma, en
    // cualquier plataforma: es como los tests y el servicio de Homebrew
    // redirigen el estado a un directorio propio.
    if let Some(base) = deps.env.get("XDG_STATE_HOME") {
        return join_with_sep(base, &["claude-code-proxy"], false);
    }

    if is_windows(&deps.platform) {
        let local = deps
            .env
            .get("LOCALAPPDATA")
            .cloned()
            .unwrap_or_else(|| format!("{}\\AppData\\Local", deps.home));
        return join_with_sep(&local, &["claude-code-proxy"], true);
    }

    let base = join_with_sep(&deps.home, &[".local", "state"], false)
        .to_string_lossy()
        .into_owned();
    join_with_sep(&base, &["claude-code-proxy"], false)
}

pub fn legacy_config_dir(deps: &DirResolverEnv) -> PathBuf {
    join_with_sep(&deps.home, &[".config", "claude-code-proxy"], false)
}

pub fn config_dir() -> PathBuf {
    resolve_config_dir(&DirResolverEnv::default())
}

pub fn state_dir() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default())
}

pub fn codex_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("codex").join("auth.json")
}

pub fn kimi_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("kimi").join("auth.json")
}

pub fn cursor_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("cursor").join("auth.json")
}

pub fn kimi_device_id_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("kimi").join("device_id")
}

pub fn log_file() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default()).join("proxy.log")
}

pub fn provider_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    resolve_config_dir(&deps).join(provider).join("auth.json")
}

pub fn provider_legacy_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    legacy_config_dir(&deps).join(provider).join("auth.json")
}

fn join_with_sep(base: &str, parts: &[&str], win32: bool) -> PathBuf {
    let sep = '/';
    let _ = win32;
    let mut out = String::new();
    for part in std::iter::once(base).chain(parts.iter().copied()) {
        if !out.is_empty() && !out.ends_with(sep) {
            out.push(sep);
        }
        out.push_str(part);
    }
    Path::new(&out).to_path_buf()
}

pub fn resolve_config_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_config_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: env.clone(),
        home: home.to_string(),
    })
}

pub fn resolve_state_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_state_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: env.clone(),
        home: home.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    /// `std::env::consts::OS` is "windows", not "win32", so the runtime name
    /// must resolve to the same directories the Node-style name does.
    #[test]
    fn rust_and_node_platform_names_agree_on_windows() {
        let env = env(&[
            ("APPDATA", "C:\\Users\\dev\\AppData\\Roaming"),
            ("LOCALAPPDATA", "C:\\Users\\dev\\AppData\\Local"),
        ]);
        let home = "C:\\Users\\dev";

        assert_eq!(
            resolve_config_dir_for_env("windows", home, &env),
            resolve_config_dir_for_env("win32", home, &env)
        );
        assert_eq!(
            resolve_state_dir_for_env("windows", home, &env),
            resolve_state_dir_for_env("win32", home, &env)
        );
        // Debe caer bajo %APPDATA%, no en el fallback XDG (~/.config), que es
        // adonde iba antes de aceptar el nombre "windows".
        let config = resolve_config_dir_for_env("windows", home, &env)
            .to_string_lossy()
            .into_owned();
        assert!(
            config.starts_with("C:\\Users\\dev\\AppData\\Roaming"),
            "{config}"
        );
        assert!(config.ends_with("claude-code-proxy"), "{config}");
        assert!(!config.contains(".config"), "{config}");

        let state = resolve_state_dir_for_env("windows", home, &env)
            .to_string_lossy()
            .into_owned();
        assert!(
            state.starts_with("C:\\Users\\dev\\AppData\\Local"),
            "{state}"
        );
    }

    /// Redirigir el estado con XDG_STATE_HOME tiene que funcionar tambien en
    /// Windows: es el mecanismo que usan los tests de integracion.
    #[test]
    fn explicit_xdg_state_home_overrides_the_platform_default() {
        let env = env(&[
            ("XDG_STATE_HOME", "/tmp/scratch"),
            ("LOCALAPPDATA", "C:\\Users\\dev\\AppData\\Local"),
        ]);
        for platform in ["windows", "win32", "linux", "macos"] {
            assert_eq!(
                resolve_state_dir_for_env(platform, "C:\\Users\\dev", &env),
                PathBuf::from("/tmp/scratch/claude-code-proxy"),
                "{platform}"
            );
        }
    }

    #[test]
    fn rust_and_node_platform_names_agree_on_macos() {
        let env = env(&[]);
        assert_eq!(
            resolve_config_dir_for_env("macos", "/Users/dev", &env),
            resolve_config_dir_for_env("darwin", "/Users/dev", &env)
        );
    }
}
