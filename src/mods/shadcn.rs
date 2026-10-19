//! gray-shadcn-lint — agent-verifiable Tailwind/shadcn rules for gray.
//!
//! Implements the agent-facing lint contract for shadcn/Tailwind projects.
//! The `shadcn_lint` tool prefers the official `npx @shadcn/ui lint` command
//! when the project declares shadcn deps, and falls back to a built-in regex
//! pass over *.tsx/*.css: arbitrary values (`w-[375px]`), hardcoded hex,
//! hardcoded `dark:` overrides, and duplicate class tokens. `tool/after`
//! re-lints just-written .tsx/.css files in shadcn/tailwind projects and
//! appends findings (cap 15). Fail open: a broken pass never eats a tool
//! result. `/shadcn on|off|status`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

const FILE_CAP: usize = 500;
const HOOK_FINDING_CAP: usize = 15;
const TOOL_FINDING_CAP: usize = 50;

fn manifest_shadcn() -> Value {
    json!({
        "name": "shadcn-lint",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [{
            "name": "shadcn_lint",
            "description": "Lint .tsx/.css files for shadcn/Tailwind rule violations: \
                arbitrary numeric values (w-[375px]) that should be design tokens, hardcoded \
                hex colors that should be CSS variables, hardcoded dark: palette overrides \
                that should be semantic tokens, and duplicate class tokens. Uses the official \
                `npx @shadcn/ui lint` when the project declares shadcn deps; otherwise runs a \
                built-in rule pass. `path` may be a file or directory (default: cwd).",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File or dir to lint; default: current dir." }
                }
            }
        }],
        "commands": ["/shadcn"],
        "hooks": ["tool/after"],
    })
}

fn state_dir() -> PathBuf {
    let home = std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("shadcn-lint")
}

fn enabled() -> bool {
    !state_dir().join("disabled").exists()
}

// --- built-in rule pass -----------------------------------------------------

/// Arbitrary numeric utilities: w-[375px], mt-[13rem], text-[15px], ...
/// Deliberately requires a digit+unit so var()/calc()/named tokens pass.
static RE_ARBITRARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:w|h|min-w|max-w|min-h|max-h|size|p|px|py|pt|pb|pl|pr|m|mx|my|mt|mb|ml|mr|gap|gap-x|gap-y|space-x|space-y|top|bottom|left|right|inset|inset-x|inset-y|rounded(?:-[a-z]+)?|text|leading|tracking|z|basis|grow|shrink|order|cols|rows)-\[(-?\d+(?:\.\d+)?(?:px|rem|em|vh|vw|ch|ex|%))\]").unwrap()
});

/// Hardcoded hex inside a class arbitrary value: bg-[#1a1a1a], text-[#fff].
static RE_HEX_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\b[a-z-]+-\[?"?#([0-9a-fA-F]{3,8})\b"#).unwrap());

/// dark: variants pinned to a palette color instead of a semantic token.
static RE_DARK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bdark:(?:bg|text|border|ring|fill|stroke|divide|outline|shadow|from|via|to)-(?:slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose|black|white)(?:-\d+)?\b|\bdark:[a-z-]+-\[#").unwrap()
});

/// Hex literal in a .css declaration that isn't a custom property definition.
static RE_CSS_HEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#[0-9a-fA-F]{6}\b|#[0-9a-fA-F]{3}\b").unwrap());
static RE_CSS_VAR_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"--[a-zA-Z0-9-]+\s*:\s*$").unwrap());

/// className="..." / className='...' / className={`...`} / class="..." bodies.
static RE_CLASS_ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:className|class)\s*=\s*(?:\{\s*)?["'`]([^"'`{}]+)["'`]"#).unwrap()
});

#[derive(Debug)]
struct Finding {
    line: usize,
    rule: &'static str,
    detail: String,
}

fn lint_tsx(content: &str, out: &mut Vec<Finding>) {
    for (i, line) in content.lines().enumerate() {
        let ln = i + 1;
        for m in RE_ARBITRARY.find_iter(line) {
            let cls = m.as_str().split('-').next().unwrap_or("");
            out.push(Finding {
                line: ln,
                rule: "arbitrary-value",
                detail: format!(
                    "`{}` — prefer a spacing/size token or CSS var (`{cls}-(--token)`)",
                    m.as_str()
                ),
            });
        }
        for m in RE_HEX_CLASS.find_iter(line) {
            out.push(Finding {
                line: ln,
                rule: "hardcoded-hex",
                detail: format!(
                    "`{}` — prefer a theme token (bg-primary, text-muted-foreground) or CSS var",
                    m.as_str()
                ),
            });
        }
        for m in RE_DARK.find_iter(line) {
            out.push(Finding {
                line: ln,
                rule: "dark-override",
                detail: format!("`{}` — hardcoded dark palette; prefer semantic tokens driven by `.dark` CSS vars", m.as_str()),
            });
        }
        for cap in RE_CLASS_ATTR.captures_iter(line) {
            let body = &cap[1];
            let mut seen = std::collections::HashSet::new();
            let mut dupes = Vec::new();
            for tok in body.split_whitespace() {
                if !seen.insert(tok) && !dupes.contains(&tok) {
                    dupes.push(tok);
                }
            }
            if !dupes.is_empty() {
                out.push(Finding {
                    line: ln,
                    rule: "class-noise",
                    detail: format!("duplicate class(es) {}", dupes.join(", ")),
                });
            }
        }
    }
}

fn lint_css(content: &str, out: &mut Vec<Finding>) {
    for (i, line) in content.lines().enumerate() {
        for m in RE_CSS_HEX.find_iter(line) {
            if RE_CSS_VAR_PREFIX.is_match(&line[..m.start()]) {
                continue;
            }
            out.push(Finding {
                line: i + 1,
                rule: "hardcoded-hex",
                detail: format!(
                    "`{}` — prefer a CSS variable so themes/dark mode can override",
                    m.as_str()
                ),
            });
        }
    }
}

fn lint_file(path: &Path, out: &mut Vec<(PathBuf, Finding)>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let mut file_findings = Vec::new();
    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") | Some("jsx") => lint_tsx(&content, &mut file_findings),
        Some("css") => lint_css(&content, &mut file_findings),
        _ => return,
    }
    for f in file_findings {
        out.push((path.to_path_buf(), f));
    }
}

fn collect_files(path: &Path, out: &mut Vec<PathBuf>) {
    if out.len() >= FILE_CAP {
        return;
    }
    if path.is_file() {
        if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("tsx" | "jsx" | "css")
        ) {
            out.push(path.to_path_buf());
        }
        return;
    }
    const SKIP: &[&str] = &[
        "node_modules",
        ".git",
        "dist",
        "build",
        ".next",
        "target",
        "coverage",
        ".gray",
    ];
    let Ok(rd) = std::fs::read_dir(path) else {
        return;
    };
    for e in rd.flatten() {
        if out.len() >= FILE_CAP {
            return;
        }
        let p = e.path();
        if p.is_dir() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.') && !SKIP.contains(&name.as_ref()) {
                collect_files(&p, out);
            }
        } else if matches!(
            p.extension().and_then(|x| x.to_str()),
            Some("tsx" | "jsx" | "css")
        ) {
            out.push(p);
        }
    }
}

fn builtin_pass(path: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files(path, &mut files);
    let mut findings = Vec::new();
    for f in &files {
        lint_file(f, &mut findings);
    }
    findings
        .iter()
        .map(|(p, f)| format!("{}:{}: [{}] {}", p.display(), f.line, f.rule, f.detail))
        .collect()
}

// --- project detection ------------------------------------------------------

fn find_up(start: &Path, name: &str) -> Option<PathBuf> {
    let mut dir = Some(if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    });
    for _ in 0..10 {
        let d = dir?;
        let cand = d.join(name);
        if cand.exists() {
            return Some(cand);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

fn pkg_has(path: &Path, needles: &[&str]) -> bool {
    find_up(path, "package.json")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|c| needles.iter().any(|n| c.contains(n)))
        .unwrap_or(false)
}

/// Project declares shadcn → the official CLI presumably applies.
fn has_shadcn_deps(path: &Path) -> bool {
    pkg_has(path, &["shadcn", "@shadcn"]) || find_up(path, "components.json").is_some()
}

/// Hook gate: shadcn or plain tailwind projects.
fn has_ui_deps(path: &Path) -> bool {
    has_shadcn_deps(path) || pkg_has(path, &["tailwindcss", "@tailwindcss"])
}

// --- official CLI -----------------------------------------------------------

/// `npx` resolved against PATH — on Windows the npm shim is `npx.cmd`,
/// which `CreateProcess` won't find by bare name, so spawn the file.
fn find_npx() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for name in ["npx", "npx.exe", "npx.cmd", "npx.bat"] {
            let c = dir.join(name);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

/// `npx args` in `dir`, captured output, bounded by a deadline — the
/// `timeout(1)` wrapper ported to spawn+try_wait so it works on
/// Windows/macOS. Err on missing npx, spawn failure, timeout, or nonzero
/// exit.
fn run_npx(args: &[&str], dir: &Path, secs: u64) -> Result<(bool, String), String> {
    use std::io::Read;
    let Some(npx) = find_npx() else {
        return Err("npx not found on PATH".into());
    };
    let mut child = std::process::Command::new(npx)
        .args(args)
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't spawn npx: {e}"))?;
    // Drain both pipes on threads so a chatty child can't deadlock.
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut b = String::new();
            let _ = r.read_to_string(&mut b);
            b
        })
    };
    let out_t = child.stdout.take().map(|s| drain(Box::new(s)));
    let err_t = child.stderr.take().map(|s| drain(Box::new(s)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("npx timed out after {secs}s"));
            }
            Err(e) => return Err(format!("couldn't wait on npx: {e}")),
        }
    };
    let stdout = out_t.and_then(|t| t.join().ok()).unwrap_or_default();
    let stderr = err_t.and_then(|t| t.join().ok()).unwrap_or_default();
    Ok((
        status.success(),
        if status.success() { stdout } else { stderr },
    ))
}

fn official_lint(path: &Path) -> Result<String, String> {
    let (ok, text) = run_npx(&["--yes", "@shadcn/ui", "lint"], path, 45)?;
    if ok {
        let text = text.trim().to_string();
        Ok(if text.is_empty() {
            "no findings".into()
        } else {
            text
        })
    } else {
        Err(format!(
            "npx @shadcn/ui lint failed: {}",
            text.chars().take(200).collect::<String>()
        ))
    }
}

fn call_tool(name: &str, args: &Value) -> Result<String, String> {
    match name {
        "shadcn_lint" => {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            if !path.exists() {
                return Err(format!("no such path: {}", path.display()));
            }
            if has_shadcn_deps(&path) {
                match official_lint(&path) {
                    Ok(text) => return Ok(text),
                    Err(e) => {
                        let mut findings = builtin_pass(&path);
                        findings.truncate(TOOL_FINDING_CAP);
                        let mut s = format!("(official lint unavailable: {e}; built-in pass)\n");
                        let body = if findings.is_empty() {
                            "no findings".to_string()
                        } else {
                            findings.join("\n")
                        };
                        s.push_str(&body);
                        return Ok(s);
                    }
                }
            }
            let mut findings = builtin_pass(&path);
            findings.truncate(TOOL_FINDING_CAP);
            Ok(if findings.is_empty() {
                "no findings".into()
            } else {
                findings.join("\n")
            })
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// --- tool/after -------------------------------------------------------------

static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[/\w.\-~]+/[\w.\-/]+\.(?:tsx|css)\b"#).unwrap());

fn is_write_tool(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("write") || n.contains("edit") || n.contains("create") || n.contains("replace")
}

/// Locate the just-written file: tool args if the host sent them, else the
/// first .tsx/.css path in the result content that exists on disk.
fn find_file(params: &Value) -> Option<PathBuf> {
    if let Some(args) = params.get("args") {
        for key in ["path", "file_path", "filename", "notebook_path"] {
            if let Some(p) = args.get(key).and_then(Value::as_str) {
                let p = PathBuf::from(p);
                if matches!(p.extension().and_then(|e| e.to_str()), Some("tsx" | "css")) {
                    return Some(p);
                }
            }
        }
    }
    let content = params.get("content").and_then(Value::as_str)?;
    let cwd = params
        .pointer("/session/cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    for m in RE_PATH.find_iter(content) {
        let raw = m.as_str();
        let p = PathBuf::from(raw);
        let p = if p.is_absolute() { p } else { cwd.join(p) };
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// `tool/after` params {name, content, is_error, session}. Replacement
/// `{content}` or None (keep). Always fail-open.
fn tool_after(params: &Value) -> Option<Value> {
    if !enabled() {
        return None;
    }
    if params
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let name = params.get("name").and_then(Value::as_str)?;
    if !is_write_tool(name) {
        return None;
    }
    let file = find_file(params)?;
    if !has_ui_deps(&file) {
        return None;
    }
    let mut findings = Vec::new();
    lint_file(&file, &mut findings);
    if findings.is_empty() {
        return None;
    }
    findings.truncate(HOOK_FINDING_CAP);
    let mut s = params
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    s.push_str("\n\nshadcn-lint findings:\n");
    for (_, f) in &findings {
        s.push_str(&format!(
            "  {}:{}: [{}] {}\n",
            file.display(),
            f.line,
            f.rule,
            f.detail
        ));
    }
    Some(json!({ "content": s.trim_end() }))
}

fn run_command(argv: &[&str]) -> String {
    match argv.first().copied() {
        Some("on") | Some("off") => {
            let flag = state_dir().join("disabled");
            let res = if argv[0] == "off" {
                std::fs::create_dir_all(state_dir()).and_then(|_| std::fs::write(&flag, b""))
            } else {
                std::fs::remove_file(&flag).or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })
            };
            match res {
                Ok(()) => format!(
                    "shadcn-lint {}",
                    if argv[0] == "off" { "off" } else { "on" }
                ),
                Err(e) => format!("couldn't flip state: {e}"),
            }
        }
        _ => format!(
            "gray-shadcn-lint {} — {} · rules: arbitrary-value, hardcoded-hex, \
            dark-override, class-noise · tool: shadcn_lint · /shadcn on|off",
            env!("CARGO_PKG_VERSION"),
            if enabled() { "on" } else { "off" },
        ),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_shadcn(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_shadcn(),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match call_tool(name, &args) {
                Ok(text) => json!({ "content": text }),
                Err(text) => json!({ "content": text, "is_error": true }),
            }
        }
        "tool/after" => tool_after(&params).unwrap_or_else(|| json!({})),
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

fn main_shadcn() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_shadcn());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle_shadcn(&req);
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

    fn call(method: &str, params: Value) -> Value {
        handle_shadcn(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    #[test]
    fn manifest_claims_tool_command_hook() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "shadcn-lint");
        assert_eq!(m["protocol"], "2.0");
        assert_eq!(m["hooks"], json!(["tool/after"]));
        assert_eq!(m["tools"][0]["name"], "shadcn_lint");
    }

    #[test]
    fn arbitrary_and_hex_and_dark_flagged() {
        let mut out = Vec::new();
        lint_tsx(
            r#"<div className="w-[375px] bg-[#1a1a1a] dark:bg-slate-900 p-4 p-4 text-[15px]" />"#,
            &mut out,
        );
        let rules: Vec<_> = out.iter().map(|f| f.rule).collect();
        assert!(rules.contains(&"arbitrary-value"));
        assert!(rules.contains(&"hardcoded-hex"));
        assert!(rules.contains(&"dark-override"));
        assert!(rules.contains(&"class-noise"));
    }

    #[test]
    fn tokens_and_vars_pass() {
        let mut out = Vec::new();
        lint_tsx(
            r#"<div className="w-96 bg-primary dark:bg-background w-[var(--sidebar-w)] text-sm" />"#,
            &mut out,
        );
        assert!(out.is_empty(), "unexpected: {out:?}");
    }

    #[test]
    fn css_hex_flagged_but_var_defs_pass() {
        let mut out = Vec::new();
        lint_css(
            ":root { --brand: #a1b2c3; }\n.card { color: #fff; }",
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 2);
    }

    #[test]
    fn tool_after_silent_without_project() {
        let dir = std::env::temp_dir().join(format!("sl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.tsx");
        std::fs::write(&f, r#"<div className="w-[375px]" />"#).unwrap();
        let r = call(
            "tool/after",
            json!({
                "name": "write", "content": format!("wrote {}", f.display()),
                "is_error": false, "session": {"cwd": dir}
            }),
        );
        assert_eq!(r["result"], json!({})); // no package.json → silent
        std::fs::write(dir.join("package.json"), r#"{"deps":{"tailwindcss":"4"}}"#).unwrap();
        let r = call(
            "tool/after",
            json!({
                "name": "write", "content": format!("wrote {}", f.display()),
                "is_error": false, "session": {"cwd": dir}
            }),
        );
        let c = r["result"]["content"].as_str().unwrap();
        assert!(c.contains("arbitrary-value"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_after_ignores_non_write_and_errors() {
        let r = call(
            "tool/after",
            json!({
                "name": "read", "content": "x.tsx", "is_error": false, "session": {}
            }),
        );
        assert_eq!(r["result"], json!({}));
        let r = call(
            "tool/after",
            json!({
                "name": "write", "content": "x.tsx", "is_error": true, "session": {}
            }),
        );
        assert_eq!(r["result"], json!({}));
    }
}
