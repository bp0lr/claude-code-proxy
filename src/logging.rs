use crate::{config, paths};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

static STDERR_SUPPRESSION_DEPTH: AtomicUsize = AtomicUsize::new(0);

pub const REDACT_KEYS: [&str; 15] = [
    "authorization",
    "proxy-authorization",
    "access",
    "access_token",
    "refresh",
    "refresh_token",
    "id_token",
    "code",
    "code_verifier",
    "chatgpt-account-id",
    "cookie",
    "set-cookie",
    "x-api-key",
    "apikey",
    "api_key",
];

pub fn log_file() -> std::path::PathBuf {
    paths::log_file()
}

#[must_use]
pub struct StderrSuppressionGuard;

impl Drop for StderrSuppressionGuard {
    fn drop(&mut self) {
        STDERR_SUPPRESSION_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn suppress_stderr() -> StderrSuppressionGuard {
    STDERR_SUPPRESSION_DEPTH.fetch_add(1, Ordering::Relaxed);
    StderrSuppressionGuard
}

fn stderr_suppressed() -> bool {
    STDERR_SUPPRESSION_DEPTH.load(Ordering::Relaxed) > 0
}

fn should_mirror_to_stderr(level: &str) -> bool {
    !stderr_suppressed() && (matches!(level, "warn" | "error") || config::log_stderr())
}

#[derive(Clone)]
pub struct Logger {
    service: String,
    base: serde_json::Map<String, Value>,
}

impl Logger {
    pub fn child(&self, bindings: serde_json::Map<String, Value>) -> Logger {
        let mut merged = self.base.clone();
        merged.extend(bindings);
        Logger {
            service: self.service.clone(),
            base: merged,
        }
    }

    pub fn debug(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("debug", msg, fields)
    }

    pub fn info(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("info", msg, fields)
    }

    pub fn warn(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("warn", msg, fields)
    }

    pub fn error(&self, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        self.emit("error", msg, fields)
    }

    fn emit(&self, level: &str, msg: &str, fields: Option<serde_json::Map<String, Value>>) {
        let mut body = serde_json::Map::new();
        body.insert("t".into(), Value::String(now_iso8601()));
        body.insert("level".into(), Value::String(level.to_string()));
        body.insert("service".into(), Value::String(self.service.clone()));
        body.insert("msg".into(), Value::String(msg.to_string()));

        let mut merged = self.base.clone();
        if let Some(fields) = fields {
            merged.extend(fields);
        }
        if !merged.is_empty() {
            body.insert("fields".into(), redact_value(Value::Object(merged)));
        }

        let line = Value::Object(body).to_string();

        let mirror_to_stderr = should_mirror_to_stderr(level);
        if mirror_to_stderr {
            let _ = writeln!(io::stderr(), "{line}");
        }

        if write_log_line(&line).is_err() && mirror_to_stderr {
            // swallow logging errors intentionally
        }
    }
}

pub fn create_logger(service: &str) -> Logger {
    Logger {
        service: service.to_string(),
        base: serde_json::Map::new(),
    }
}

fn write_log_line(line: &str) -> io::Result<()> {
    let file = log_file();
    if let Some(dir) = file.parent() {
        create_dir(dir, 0o700)?;
    }

    if fs::metadata(&file).is_ok_and(|meta| meta.len() > MAX_LOG_BYTES) {
        rotate_file(&file)?;
    }

    let mut out = OpenOptions::new().create(true).append(true).open(&file)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
}

/// How many rotated `proxy.<millis>` files survive a rotation.
const MAX_ROTATED_LOGS: usize = 1;

fn rotate_file(path: &Path) -> io::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let rotated = path.with_extension(format!("{ts}"));
    fs::rename(path, rotated)?;
    prune_rotated_logs(path);
    Ok(())
}

/// Rotated logs used to accumulate forever. Keeping a bounded number of them
/// caps the log directory at a predictable size.
fn prune_rotated_logs(path: &Path) {
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let prefix = format!("{}.", stem.to_string_lossy());
    let mut rotated: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| candidate != path)
        .filter(|candidate| {
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(&prefix))
                // The suffix is a millisecond timestamp, so lexicographic
                // order over equal-length digits is chronological order.
                .is_some_and(|suffix| suffix.chars().all(|ch| ch.is_ascii_digit()))
        })
        .collect();

    if rotated.len() <= MAX_ROTATED_LOGS {
        return;
    }
    rotated.sort();
    let excess = rotated.len() - MAX_ROTATED_LOGS;
    for stale in rotated.into_iter().take(excess) {
        let _ = fs::remove_file(stale);
    }
}

fn create_dir(path: &Path, mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    set_mode(path, mode);
    Ok(())
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(mode);
            let _ = fs::set_permissions(path, perm);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

fn now_iso8601() -> String {
    let now = time::OffsetDateTime::now_utc();
    let format = time::format_description::parse_borrowed::<3>(
        "[year]-[month]-[day]T[hour]:[minute]:[second]Z",
    )
    .unwrap();
    now.format(&format).unwrap_or_else(|_| String::new())
}

pub fn redact_value(value: Value) -> Value {
    redact_with_depth(value, 0)
}

fn redact_with_depth(value: Value, depth: u8) -> Value {
    if depth > 6 {
        return Value::String("[depth-limit]".into());
    }

    match value {
        Value::String(s) => {
            if config::log_verbose() {
                Value::String(s)
            } else if s.len() > 4000 {
                // Truncate on a char boundary: `&s[..4000]` panics when byte
                // 4000 lands inside a multi-byte character.
                let end = floor_char_boundary(&s, 4000);
                Value::String(format!("{}…[{} more]", &s[..end], s.len() - end))
            } else {
                Value::String(s)
            }
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| redact_with_depth(v, depth + 1))
                .collect(),
        ),
        Value::Object(fields) => {
            let mut out = serde_json::Map::new();
            for (key, value) in fields {
                if REDACT_KEYS.contains(&key.to_lowercase().as_str()) {
                    out.insert(key, redact_key_redaction(value));
                } else {
                    out.insert(key, redact_with_depth(value, depth + 1));
                }
            }
            Value::Object(out)
        }
        value => value,
    }
}

/// Largest index `<= max` that is a UTF-8 character boundary.
fn floor_char_boundary(value: &str, max: usize) -> usize {
    if max >= value.len() {
        return value.len();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn redact_key_redaction(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("[redacted len={}]", s.len())),
        _ => Value::String("[redacted]".to_string()),
    }
}

pub fn redacted_keys() -> HashSet<&'static str> {
    REDACT_KEYS.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static STDERR_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn rotation_keeps_only_the_newest_rotated_logs() {
        let temp = tempfile::TempDir::new().unwrap();
        let active = temp.path().join("proxy.log");

        for stamp in ["1700000000001", "1700000000002", "1700000000003"] {
            fs::write(temp.path().join(format!("proxy.{stamp}")), b"old").unwrap();
        }
        fs::write(&active, b"current").unwrap();
        // Unrelated files must survive.
        fs::write(temp.path().join("proxy.log.bak"), b"keep").unwrap();
        fs::write(temp.path().join("notes.txt"), b"keep").unwrap();

        prune_rotated_logs(&active);

        let mut left: Vec<String> = fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "notes.txt",
                "proxy.1700000000003",
                "proxy.log",
                "proxy.log.bak"
            ]
        );
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // 3999 ASCII bytes + 'ñ' puts byte 4000 inside the two-byte character,
        // which is exactly where `&s[..4000]` used to panic.
        let text = format!("{}ñ{}", "a".repeat(3999), "b".repeat(100));
        let end = floor_char_boundary(&text, 4000);
        assert_eq!(end, 3999);
        let _ = &text[..end];

        assert_eq!(floor_char_boundary("hola", 10), 4);
        assert_eq!(floor_char_boundary("ñ", 1), 0);
    }

    #[test]
    fn truncation_keeps_short_strings_intact() {
        let Value::String(redacted) = redact_value(Value::String("hola ñandú".into())) else {
            panic!("expected a string");
        };
        assert_eq!(redacted, "hola ñandú");
    }

    #[test]
    fn stderr_suppression_disables_level_mirroring() {
        let _lock = STDERR_TEST_LOCK.lock().unwrap();
        assert!(should_mirror_to_stderr("warn"));

        {
            let _guard = suppress_stderr();
            assert!(!should_mirror_to_stderr("warn"));
            assert!(!should_mirror_to_stderr("error"));
        }

        assert!(should_mirror_to_stderr("warn"));
    }

    #[test]
    fn stderr_suppression_supports_nested_guards() {
        let _lock = STDERR_TEST_LOCK.lock().unwrap();
        let outer = suppress_stderr();
        let inner = suppress_stderr();
        assert!(!should_mirror_to_stderr("warn"));

        drop(inner);
        assert!(!should_mirror_to_stderr("warn"));

        drop(outer);
        assert!(should_mirror_to_stderr("warn"));
    }

    #[test]
    fn redacts_proxy_authorization_case_insensitively() {
        let redacted = redact_value(serde_json::json!({
            "Proxy-Authorization": "Basic dXNlcjpwYXNz",
            "safe": "kept"
        }));

        assert_eq!(redacted["safe"], "kept");
        assert_eq!(redacted["Proxy-Authorization"], "[redacted len=18]");
    }
}
