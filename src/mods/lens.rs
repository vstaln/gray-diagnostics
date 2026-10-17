//! gray-lens — post-edit diagnostics appended to file-tool results.
//!
//! After a write/edit-shaped file tool
//! succeeds, run the right checker for the file's language and append the
//! findings to the tool result. `tool/after` carries no args, so
//! `tool/before` stashes the target path per (session, tool) in memory.
//!
//! Checker selection by extension (all overridable per
//! `~/.gray/lens/config.json`):
//!   .ts/.tsx/.js/.jsx → `tsc --noEmit` when a tsconfig.json is found walking
//!     up from the file, else `npx eslint <file>` when an eslint config exists
//!   .py  → `ruff check <file>`, else `python3|python -m pyflakes <file>`
//!   .rs  → skipped by default; `cargo clippy` only when config opts in
//!   .go  → `gofmt -l <file>` + `go vet` on the package
//!   .sh  → `shellcheck -f gcc <file>`
//!   else → `ast-grep scan <file>` only when the fallback is enabled
//!
//! Findings are appended as "\n--- lens: <tool> ---\n<findings>"; the
//! original content is kept intact. Missing tools, timeouts and zero-issue
//! runs are silent. Every failure path keeps the original result — lens
//! never eats a tool result.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// File-shaped tools whose path arg selects the checker.
const WRITE_TOOLS: &[&str] = &[
    "write", "edit", "patch", "write_file", "apply_patch", "multi_edit", "notebook_edit",
];

const ESLINT_CONFIGS: &[&str] = &[
    "eslint.config.js", "eslint.config.mjs", "eslint.config.cjs", "eslint.config.ts",
    ".eslintrc", ".eslintrc.js", ".eslintrc.cjs", ".eslintrc.yaml", ".eslintrc.yml",
    ".eslintrc.json",
];

const DEFAULT_TIMEOUT_MS: u64 = 8000;
const DEFAULT_MAX_ISSUES: usize = 20;

// ---------- state / config ---------------------------------------------------

fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_path() -> PathBuf {
    gray_home().join("lens").join("config.json")
}

#[derive(Debug, Clone)]
struct Config {
    enabled: bool,
    timeout_ms: u64,
    max_issues: usize,
    /// `ast-grep scan` for extensions with no dedicated checker.
    fallback_ast_grep: bool,
    /// `cargo clippy` for .rs — off by default (heavy).
    rust_clippy: bool,
    /// extension (leading dot, lowercase) → command line. `{file}` inside is
    /// replaced by the target path; otherwise the path is appended.
    lang_overrides: HashMap<String, String>,
    /// extensions with per_lang entries {"enabled": false}.
    disabled_langs: HashSet<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_issues: DEFAULT_MAX_ISSUES,
            fallback_ast_grep: false,
            rust_clippy: false,
            lang_overrides: HashMap::new(),
            disabled_langs: HashSet::new(),
        }
    }
}

fn norm_ext(s: &str) -> String {
    let s = s.trim().to_lowercase();
    if s.starts_with('.') { s } else { format!(".{s}") }
}

fn load_config_from(path: &Path) -> Config {
    let mut cfg = Config::default();
    let Ok(raw) = std::fs::read_to_string(path) else { return cfg };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else { return cfg };
    if let Some(b) = v.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = b;
    }
    if let Some(n) = v.get("timeout_ms").and_then(Value::as_u64) {
        cfg.timeout_ms = n.clamp(250, 120_000);
    }
    if let Some(n) = v.get("max_issues").and_then(Value::as_u64) {
        cfg.max_issues = (n.max(1) as usize).min(500);
    }
    for key in ["fallback_ast_grep", "ast_grep", "generic_fallback"] {
        if let Some(b) = v.get(key).and_then(Value::as_bool) {
            cfg.fallback_ast_grep = b;
        }
    }
    for key in ["rust_clippy", "clippy"] {
        if let Some(b) = v.get(key).and_then(Value::as_bool) {
            cfg.rust_clippy = b;
        }
    }
    if let Some(map) = v.get("per_lang").and_then(Value::as_object) {
        for (k, val) in map {
            let ext = norm_ext(k);
            match val {
                Value::String(cmd) => {
                    cfg.lang_overrides.insert(ext, cmd.clone());
                }
                Value::Object(_) => {
                    let enabled = val.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                    if !enabled {
                        cfg.disabled_langs.insert(ext);
                    } else if let Some(cmd) = val.get("cmd").and_then(Value::as_str) {
                        cfg.lang_overrides.insert(ext, cmd.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    cfg
}

fn load_config() -> Config {
    load_config_from(&config_path())
}

/// Merge `per_lang` back into the raw config object and write it.
fn write_per_lang(per_lang: &Value) -> Result<(), String> {
    let dir = gray_home().join("lens");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = config_path();
    let mut v = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({}));
    v["per_lang"] = per_lang.clone();
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).map_err(|e| e.to_string())
}

fn write_enabled(enabled: bool) -> Result<(), String> {
    let dir = gray_home().join("lens");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = config_path();
    let mut v = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({}));
    v["enabled"] = json!(enabled);
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).map_err(|e| e.to_string())
}

// ---------- process helpers ---------------------------------------------------

fn on_path(prog: &str) -> bool {
    if prog.contains('/') || prog.contains('\\') {
        return Path::new(prog).exists();
    }
    let Some(paths) = std::env::var_os("PATH") else { return false };
    for dir in std::env::split_paths(&paths) {
        if dir.join(prog).is_file() {
            return true;
        }
        #[cfg(windows)]
        for ext in ["exe", "cmd", "bat"] {
            if dir.join(format!("{prog}.{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}

/// Run argv in `cwd`, collecting stdout+stderr. Returns None when the
/// program is missing, can't spawn, or runs past `timeout` (killed).
fn run_timed(argv: &[String], cwd: &Path, timeout: Duration) -> Option<String> {
    let (prog, args) = argv.split_first()?;
    if !on_path(prog) {
        return None;
    }
    let mut child = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let drain = |mut p: std::process::ChildStdout| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = p.read_to_end(&mut b);
            b
        })
    };
    let out_reader = child.stdout.take().map(drain);
    let err_reader = child.stderr.take().map(|p| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let mut p: std::process::ChildStderr = p;
            let _ = p.read_to_end(&mut b);
            b
        })
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => return None,
        }
    }
    let mut text = String::new();
    for h in [out_reader, err_reader].into_iter().flatten() {
        if let Ok(bytes) = h.join() {
            let chunk = String::from_utf8_lossy(&bytes);
            if !chunk.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(chunk.trim_end());
            }
        }
    }
    Some(text)
}

fn find_upward(start: &Path, names: &[&str]) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        for n in names {
            let cand = dir.join(n);
            if cand.exists() {
                return Some(cand);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ---------- checker selection ---------------------------------------------------

struct Check {
    label: String,
    argv: Vec<String>,
    cwd: PathBuf,
}

fn check(label: &str, argv: Vec<String>, cwd: PathBuf) -> Check {
    Check { label: label.into(), argv, cwd }
}

fn checks_for(file: &Path, cfg: &Config) -> Vec<Check> {
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .map(norm_ext)
        .unwrap_or_default();
    let dir = file.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let file_s = file.to_string_lossy().to_string();
    if cfg.disabled_langs.contains(&ext) {
        return Vec::new();
    }
    if let Some(cmdline) = cfg.lang_overrides.get(&ext) {
        let mut argv: Vec<String> = cmdline.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            return Vec::new();
        }
        let mut used = false;
        for a in argv.iter_mut() {
            if a == "{file}" {
                *a = file_s.clone();
                used = true;
            }
        }
        if !used {
            argv.push(file_s);
        }
        return vec![check("custom", argv, dir)];
    }
    match ext.as_str() {
        ".ts" | ".tsx" | ".js" | ".jsx" => {
            if let Some(ts) = find_upward(&dir, &["tsconfig.json"]) {
                vec![check(
                    "tsc",
                    vec!["tsc".into(), "--noEmit".into(), "--pretty".into(), "false".into()],
                    ts.parent().unwrap_or(&dir).to_path_buf(),
                )]
            } else if let Some(es) = find_upward(&dir, ESLINT_CONFIGS) {
                vec![check(
                    "eslint",
                    vec!["npx".into(), "--no-install".into(), "eslint".into(), file_s],
                    es.parent().unwrap_or(&dir).to_path_buf(),
                )]
            } else {
                Vec::new()
            }
        }
        ".py" => {
            if on_path("ruff") {
                vec![check(
                    "ruff",
                    vec!["ruff".into(), "check".into(), "--output-format".into(), "concise".into(), file_s],
                    dir,
                )]
            } else {
                ["python3", "python"]
                    .iter()
                    .find(|p| on_path(p))
                    .map(|py| {
                        vec![check(
                            "pyflakes",
                            vec![py.to_string(), "-m".into(), "pyflakes".into(), file_s],
                            dir,
                        )]
                    })
                    .unwrap_or_default()
            }
        }
        ".rs" => {
            if cfg.rust_clippy && on_path("cargo") {
                find_upward(&dir, &["Cargo.toml"])
                    .map(|ct| {
                        vec![check(
                            "clippy",
                            vec!["cargo".into(), "clippy".into(), "--quiet".into(), "--message-format".into(), "short".into()],
                            ct.parent().unwrap_or(&dir).to_path_buf(),
                        )]
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        }
        ".go" => {
            let mut v = Vec::new();
            v.push(check("gofmt", vec!["gofmt".into(), "-l".into(), file_s], dir.clone()));
            v.push(check("go vet", vec!["go".into(), "vet".into(), ".".into()], dir));
            v
        }
        ".sh" | ".bash" => {
            vec![check("shellcheck", vec!["shellcheck".into(), "-f".into(), "gcc".into(), file_s], dir)]
        }
        _ => {
            if cfg.fallback_ast_grep {
                ["sg", "ast-grep"]
                    .iter()
                    .find(|p| on_path(p))
                    .map(|p| {
                        vec![check("ast-grep", vec![p.to_string(), "scan".into(), file_s], dir)]
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        }
    }
}

/// Trim checker output to at most `max` non-empty, non-noise lines.
fn filter_findings(label: &str, out: &str, max: usize) -> String {
    let mut lines: Vec<String> = out
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !l.contains("No module named")) // missing pyflakes: tool absent, not findings
        .map(str::to_string)
        .collect();
    if label == "gofmt" && !lines.is_empty() {
        lines = vec![format!("not gofmt-clean: {}", lines.join(", "))];
    }
    lines.truncate(max);
    lines.join("\n")
}

// ---------- hooks ---------------------------------------------------

/// (session, tool) → path, stashed by tool/before for tool/after.
pub type Stash = Mutex<HashMap<(String, String), String>>;

pub struct State {
    pub stash: Stash,
}

fn session_key(params: &Value) -> (String, String) {
    let sid = params
        .get("session")
        .and_then(|s| s.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    (sid, name)
}

fn extract_path(args: &Value) -> Option<String> {
    for key in ["path", "file_path"] {
        if let Some(p) = args.get(key).and_then(Value::as_str) {
            return Some(p.to_string());
        }
    }
    args.as_object().and_then(|m| {
        m.iter()
            .find(|(k, v)| k.contains("path") && v.is_string())
            .and_then(|(_, v)| v.as_str().map(str::to_string))
    })
}

fn tool_before(state: &State, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if !WRITE_TOOLS.contains(&name) {
        return json!({"decision": "allow"});
    }
    let args = params.get("args").cloned().unwrap_or(json!({}));
    if let Some(path) = extract_path(&args) {
        state.stash.lock().expect("stash").insert(session_key(params), path);
    }
    json!({"decision": "allow"})
}

fn after_with_cfg(state: &State, params: &Value, cfg: &Config) -> Value {
    let key = session_key(params);
    let Some(path) = state.stash.lock().expect("stash").remove(&key) else {
        return json!({});
    };
    if !cfg.enabled || params.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        return json!({});
    }
    let session_cwd = params
        .get("session")
        .and_then(|s| s.get("cwd"))
        .and_then(Value::as_str)
        .unwrap_or(".");
    let file = {
        let p = PathBuf::from(&path);
        if p.is_absolute() { p } else { PathBuf::from(session_cwd).join(p) }
    };
    let mut blocks = Vec::new();
    for c in checks_for(&file, cfg) {
        let Some(out) = run_timed(&c.argv, &c.cwd, Duration::from_millis(cfg.timeout_ms)) else {
            continue;
        };
        let findings = filter_findings(&c.label, &out, cfg.max_issues);
        if findings.is_empty() {
            continue;
        }
        blocks.push(format!("--- lens: {} ---\n{findings}", c.label));
    }
    if blocks.is_empty() {
        return json!({});
    }
    let content = params.get("content").and_then(Value::as_str).unwrap_or("");
    json!({ "content": format!("{content}\n{}", blocks.join("\n")) })
}

fn tool_after(state: &State, params: &Value) -> Value {
    after_with_cfg(state, params, &load_config())
}

// ---------- /lens --------------------------------------------------------------

fn status_text() -> String {
    let cfg = load_config();
    let tools: Vec<String> = [
        ("tsc", on_path("tsc")),
        ("npx", on_path("npx")),
        ("ruff", on_path("ruff")),
        ("python3", on_path("python3")),
        ("cargo", on_path("cargo")),
        ("gofmt", on_path("gofmt")),
        ("go", on_path("go")),
        ("shellcheck", on_path("shellcheck")),
        ("sg", on_path("sg") || on_path("ast-grep")),
    ]
    .iter()
    .map(|(n, ok)| format!("{n}{}", if *ok { "+" } else { "-" }))
    .collect();
    let mut lines = vec![format!(
        "gray-lens {} — {} · timeout {}ms · max {} issues · ast-grep fallback {} · rust clippy {}\nconfig: {}\ntools: {}",
        env!("CARGO_PKG_VERSION"),
        if cfg.enabled { "on" } else { "off" },
        cfg.timeout_ms,
        cfg.max_issues,
        if cfg.fallback_ast_grep { "on" } else { "off" },
        if cfg.rust_clippy { "on" } else { "off" },
        config_path().display(),
        tools.join(" "),
    )];
    if !cfg.lang_overrides.is_empty() || !cfg.disabled_langs.is_empty() {
        lines.push("per_lang:".into());
        for (e, c) in &cfg.lang_overrides {
            lines.push(format!("  {e} → {c}"));
        }
        for e in &cfg.disabled_langs {
            lines.push(format!("  {e} → (disabled)"));
        }
    }
    lines.join("\n")
}

fn run_command(argv: &[&str]) -> String {
    match argv.first().copied() {
        Some("on") => match write_enabled(true) {
            Ok(()) => "lens on".into(),
            Err(e) => format!("couldn't write {}: {e}", config_path().display()),
        },
        Some("off") => match write_enabled(false) {
            Ok(()) => "lens off".into(),
            Err(e) => format!("couldn't write {}: {e}", config_path().display()),
        },
        Some("lang") => {
            let path = config_path();
            let mut raw = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .unwrap_or_else(|| json!({}));
            if !raw.is_object() {
                raw = json!({});
            }
            let mut per_lang = raw.get("per_lang").cloned().unwrap_or_else(|| json!({}));
            if !per_lang.is_object() {
                per_lang = json!({});
            }
            match argv.get(1).copied() {
                None => {
                    let cfg = load_config();
                    if cfg.lang_overrides.is_empty() && cfg.disabled_langs.is_empty() {
                        "no per-language overrides — /lens lang <ext> <cmd…>".into()
                    } else {
                        let mut l = Vec::new();
                        for (e, c) in &cfg.lang_overrides {
                            l.push(format!("{e} → {c}"));
                        }
                        for e in &cfg.disabled_langs {
                            l.push(format!("{e} → (disabled)"));
                        }
                        l.join("\n")
                    }
                }
                Some(ext) => {
                    let ext = norm_ext(ext);
                    let cmd = argv[2..].join(" ");
                    if cmd.is_empty() {
                        return "usage: /lens lang <ext> <cmd…> (use `off` to disable, `default` to clear an override)".into();
                    }
                    match cmd.as_str() {
                        "off" | "disable" => {
                            per_lang[&ext] = json!({"enabled": false});
                            match write_per_lang(&per_lang) {
                                Ok(()) => format!("{ext} checks disabled"),
                                Err(e) => format!("couldn't write config: {e}"),
                            }
                        }
                        "default" | "clear" => {
                            if let Some(m) = per_lang.as_object_mut() {
                                m.remove(&ext);
                            }
                            match write_per_lang(&per_lang) {
                                Ok(()) => format!("{ext} back to the built-in checker"),
                                Err(e) => format!("couldn't write config: {e}"),
                            }
                        }
                        _ => {
                            per_lang[&ext] = json!(cmd);
                            match write_per_lang(&per_lang) {
                                Ok(()) => format!("{ext} → {cmd} (file path appended, or use {{file}} in the cmd)"),
                                Err(e) => format!("couldn't write config: {e}"),
                            }
                        }
                    }
                }
            }
        }
        Some("status") | None => status_text(),
        _ => "usage: /lens on|off|status|lang <ext> <cmd-override|off|default>".into(),
    }
}

// ---------- wire --------------------------------------------------------------

fn manifest_lens() -> Value {
    json!({
        "name": "lens",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/lens"],
        "hooks": ["tool/before", "tool/after"],
    })
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_lens(state: &State, req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_lens(),
        "tool/before" => tool_before(state, &params),
        "tool/after" => tool_after(state, &params),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({ "text": run_command(&argv) })
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main_lens() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_lens());
        return Ok(());
    }
    let state = State { stash: Mutex::new(HashMap::new()) };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (reply, exit) = handle_lens(&state, &req);
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> State {
        State { stash: Mutex::new(HashMap::new()) }
    }

    fn call(state: &State, method: &str, params: Value) -> Value {
        handle_lens(state, &json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    fn sess() -> Value {
        json!({"id": "s1", "cwd": "/tmp"})
    }

    #[test]
    fn manifest_claims_after_hook_and_command() {
        let m = call(&st(), "plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "lens");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["tool/before", "tool/after"]));
        assert_eq!(m["commands"], json!(["/lens"]));
    }

    #[test]
    fn before_stashes_only_write_tools() {
        let s = st();
        let r = call(&s, "tool/before", json!({"name":"read","args":{"path":"/tmp/x.py"},"session":sess()}));
        assert_eq!(r["result"]["decision"], "allow");
        assert!(s.stash.lock().unwrap().is_empty());
        let r = call(&s, "tool/before", json!({"name":"write","args":{"path":"/tmp/x.py"},"session":sess()}));
        assert_eq!(r["result"]["decision"], "allow");
        assert_eq!(s.stash.lock().unwrap().len(), 1);
    }

    #[test]
    fn after_without_stash_keeps_result() {
        let s = st();
        let r = call(&s, "tool/after", json!({"name":"write","content":"ok","session":sess()}));
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn after_on_error_result_keeps_result() {
        let s = st();
        call(&s, "tool/before", json!({"name":"write","args":{"path":"/tmp/x.py"},"session":sess()}));
        let r = call(&s, "tool/after", json!({"name":"write","content":"nope","is_error":true,"session":sess()}));
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn override_checker_appends_block() {
        let s = st();
        let dir = std::env::temp_dir().join(format!("gray-lens-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("thing.xyz");
        std::fs::write(&f, "xyz-findings-body").unwrap();
        let mut cfg = Config::default();
        cfg.lang_overrides.insert(".xyz".into(), "cat {file}".into());
        call(&s, "tool/before", json!({"name":"write","args":{"path":f},"session":sess()}));
        let r = after_with_cfg(&s, &json!({"name":"write","content":"written","session":sess()}), &cfg);
        let c = r["content"].as_str().unwrap_or("");
        assert!(c.starts_with("written\n--- lens: custom ---\n"), "{c}");
        assert!(c.contains("xyz-findings-body"), "{c}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_run_appends_nothing() {
        let s = st();
        let dir = std::env::temp_dir().join(format!("gray-lens-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("empty.xyz");
        std::fs::write(&f, "").unwrap();
        let mut cfg = Config::default();
        cfg.lang_overrides.insert(".xyz".into(), "cat {file}".into());
        call(&s, "tool/before", json!({"name":"write","args":{"path":f},"session":sess()}));
        let r = after_with_cfg(&s, &json!({"name":"write","content":"ok","session":sess()}), &cfg);
        assert_eq!(r, json!({}));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_lang_skips_checker() {
        let mut cfg = Config::default();
        cfg.lang_overrides.insert(".xyz".into(), "cat {file}".into());
        cfg.disabled_langs.insert(".xyz".into());
        assert!(checks_for(Path::new("/tmp/a.xyz"), &cfg).is_empty());
    }

    #[test]
    fn findings_are_capped_and_noise_filtered() {
        let out = "a\n\nNo module named pyflakes\nb\nc";
        assert_eq!(filter_findings("ruff", out, 2), "a\nb");
    }

    #[test]
    fn gofmt_rewrites_output() {
        assert_eq!(filter_findings("gofmt", "main.go\n", 20), "not gofmt-clean: main.go");
        assert_eq!(filter_findings("gofmt", "", 20), "");
    }

    #[test]
    fn norm_ext_adds_dot_and_lowercases() {
        assert_eq!(norm_ext("PY"), ".py");
        assert_eq!(norm_ext(".Ts"), ".ts");
    }

    #[test]
    fn config_parses_per_lang_shapes() {
        let dir = std::env::temp_dir().join(format!("gray-lens-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        std::fs::write(
            &p,
            r#"{"enabled": false, "timeout_ms": 100, "per_lang": {".py": {"enabled": false}, "rs": "cargo fmt --check"}}"#,
        )
        .unwrap();
        let cfg = load_config_from(&p);
        assert!(!cfg.enabled);
        assert_eq!(cfg.timeout_ms, 250); // clamped floor
        assert!(cfg.disabled_langs.contains(".py"));
        assert_eq!(cfg.lang_overrides.get(".rs").map(String::as_str), Some("cargo fmt --check"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_method_errors() {
        let r = call(&st(), "bogus/method", Value::Null);
        assert_eq!(r["error"]["code"], -32601);
    }
}
