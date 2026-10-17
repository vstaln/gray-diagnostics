//! gray-uv — rewrite Python packaging commands to their uv equivalents.
//!
//! Port-by-spec of mitsupi's `uv.ts` + `intercepted-commands/` (proprietary —
//! behavior reimplemented from the port spec, no code copied). Where the
//! original blocked pip/poetry outright, this sidecar rewrites instead:
//! `tool/before` on `bash` inspects the FIRST shell segment of args.command
//! and answers `{decision:"modify"}` with the uv equivalent:
//!
//!   pip install X      -> uv pip install X     (pip3 -> uv pip)
//!   poetry add X       -> uv add X
//!   poetry install     -> uv sync
//!   poetry remove X    -> uv remove X
//!   poetry run C       -> uv run C
//!   python -m venv …   -> uv venv …            (python3, python3.x too)
//!
//! Only the first segment (before the first `|` `;` `&` or newline) is
//! examined and only when the tool word is its first token — a pip inside a
//! pipe, a quoted string, or a later `&&` segment is never touched.
//!
//! `/uv on|off|status` toggles the rewriter; state persists in
//! ~/.gray/uv/enabled (default on). Fail open: any rewrite problem -> allow.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

fn manifest_uv() -> Value {
    json!({
        "name": "uv",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [],
        "commands": ["/uv"],
        "hooks": ["tool/before"],
    })
}

fn state_dir() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("uv")
}

fn enabled() -> bool {
    match std::fs::read_to_string(state_dir().join("enabled")) {
        Ok(s) => !matches!(s.trim(), "off" | "0" | "false" | "no"),
        Err(_) => true,
    }
}

fn set_enabled(on: bool) -> std::io::Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("enabled"), if on { "on" } else { "off" })
}

/// `python`, `python3`, `python3.12` — an interpreter token, never a path
/// like `.venv/bin/python` (the tool word must BE the first token).
fn is_python_token(tok: &str) -> bool {
    let Some(rest) = tok.strip_prefix("python") else { return false };
    if rest.is_empty() {
        return true;
    }
    let mut parts = rest.split('.');
    matches!(parts.next(), Some(maj) if !maj.is_empty() && maj.chars().all(|c| c.is_ascii_digit()))
        && parts.all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Rewrite `command` if its first segment starts with a Python-packaging
/// tool word. Returns the new command, or None to leave it alone.
fn rewrite(command: &str) -> Option<String> {
    let split = command
        .find(|c| matches!(c, '|' | ';' | '&' | '\n'))
        .unwrap_or(command.len());
    let (head, tail) = command.split_at(split);
    let indent = &head[..head.len() - head.trim_start().len()];
    let trail = &head[head.trim_end().len()..];
    let tokens: Vec<&str> = head.split_whitespace().collect();
    let first = *tokens.first()?;

    let new_head: String = match first {
        // `pip install X` -> `uv pip install X`; `pip3 freeze` -> `uv pip freeze`.
        "pip" | "pip3" => format!("uv pip{}", join_rest(&tokens, 1)),
        "poetry" => {
            let mapped = match tokens.get(1).copied() {
                Some("add") => "add",
                Some("install") => "sync",
                Some("remove") => "remove",
                Some("run") => "run",
                _ => return None,
            };
            format!("uv {mapped}{}", join_rest(&tokens, 2))
        }
        // `python -m venv .venv` -> `uv venv .venv`.
        t if is_python_token(t) => {
            if tokens.get(1) == Some(&"-m") && tokens.get(2) == Some(&"venv") {
                format!("uv venv{}", join_rest(&tokens, 3))
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(format!("{indent}{new_head}{trail}{tail}"))
}

/// Tokens `from` onward, re-joined with single spaces (leading space when
/// non-empty) — the spec's naive whitespace tokenization.
fn join_rest(tokens: &[&str], from: usize) -> String {
    if tokens.len() > from {
        format!(" {}", tokens[from..].join(" "))
    } else {
        String::new()
    }
}

fn tool_before(params: &Value) -> Value {
    if !enabled() {
        return json!({"decision": "allow"});
    }
    if params.get("name").and_then(Value::as_str) != Some("bash") {
        return json!({"decision": "allow"});
    }
    let command = params
        .get("args")
        .and_then(|a| a.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match rewrite(command) {
        Some(new) if new != command => json!({
            "decision": "modify",
            "args": { "command": new }
        }),
        _ => json!({"decision": "allow"}),
    }
}

/// `/uv …` — `argv` excludes the command name.
fn run_command(argv: &[&str]) -> String {
    match argv.first().copied() {
        Some("on") => match set_enabled(true) {
            Ok(()) => "uv rewrites on".into(),
            Err(e) => format!("couldn't write state: {e}"),
        },
        Some("off") => match set_enabled(false) {
            Ok(()) => "uv rewrites off".into(),
            Err(e) => format!("couldn't write state: {e}"),
        },
        _ => format!(
            "gray-uv {} — {}rewriting pip/poetry/`python -m venv` to uv. /uv on|off|status",
            env!("CARGO_PKG_VERSION"),
            if enabled() { "" } else { "NOT " }
        ),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
pub fn handle_uv(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest_uv(),
        "tool/before" => tool_before(&params),
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

fn main_uv() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest_uv());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (reply, exit) = handle_uv(&req);
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
        handle_uv(&json!({ "id": 1, "method": method, "params": params }))
            .0
            .unwrap()
    }

    fn before(command: &str) -> Value {
        call(
            "tool/before",
            json!({"name": "bash", "args": {"command": command}, "session": {"id": "s"}}),
        )
    }

    #[test]
    fn manifest_shape() {
        let m = manifest_uv();
        assert_eq!(m["name"], "uv");
        assert_eq!(m["hooks"], json!(["tool/before"]));
        assert_eq!(m["commands"], json!(["/uv"]));
    }

    #[test]
    fn pip_install_rewrites() {
        let r = before("pip install requests");
        assert_eq!(r["result"]["decision"], "modify");
        assert_eq!(r["result"]["args"]["command"], "uv pip install requests");
    }

    #[test]
    fn pip3_and_requirements_rewrite() {
        let r = before("pip3 install -r requirements.txt");
        assert_eq!(r["result"]["args"]["command"], "uv pip install -r requirements.txt");
        let r = before("pip install -r dev.txt");
        assert_eq!(r["result"]["args"]["command"], "uv pip install -r dev.txt");
    }

    #[test]
    fn poetry_maps_subcommands() {
        for (from, to) in [
            ("poetry add rich", "uv add rich"),
            ("poetry install", "uv sync"),
            ("poetry remove rich", "uv remove rich"),
            ("poetry run pytest -q", "uv run pytest -q"),
        ] {
            let r = before(from);
            assert_eq!(r["result"]["args"]["command"], to, "for {from}");
        }
        assert_eq!(before("poetry build")["result"]["decision"], "allow");
    }

    #[test]
    fn python_m_venv_rewrites() {
        let r = before("python -m venv .venv");
        assert_eq!(r["result"]["args"]["command"], "uv venv .venv");
        let r = before("python3 -m venv");
        assert_eq!(r["result"]["args"]["command"], "uv venv");
        let r = before("python3.12 -m venv v");
        assert_eq!(r["result"]["args"]["command"], "uv venv v");
        // python -m anything else is left alone
        assert_eq!(before("python -m pip install x")["result"]["decision"], "allow");
        assert_eq!(before("python script.py")["result"]["decision"], "allow");
    }

    #[test]
    fn only_first_token_rewrites() {
        // pip inside a pipe / after a separator must not be touched
        for cmd in [
            "echo hi | pip install x",
            "cat requirements.txt && pip install -r requirements.txt",
            "echo 'pip install x'",
            "sudo pip install x",
            "uv pip install already-uv",
            "ls; pip install x",
        ] {
            assert_eq!(before(cmd)["result"]["decision"], "allow", "for {cmd}");
        }
        // leading whitespace still counts as "starts with"
        let r = before("  pip install x");
        assert_eq!(r["result"]["args"]["command"], "  uv pip install x");
    }

    #[test]
    fn tail_after_first_segment_is_preserved() {
        let r = before("pip install x && echo done");
        assert_eq!(r["result"]["args"]["command"], "uv pip install x && echo done");
    }

    #[test]
    fn non_bash_and_missing_command_allow() {
        assert_eq!(
            call("tool/before", json!({"name": "read", "args": {"path": "x"}}))["result"]["decision"],
            "allow"
        );
        assert_eq!(
            call("tool/before", json!({"name": "bash", "args": {}}))["result"]["decision"],
            "allow"
        );
    }

    #[test]
    fn shutdown_replies_then_exits() {
        let (reply, exit) = handle_uv(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
    }
}
