//! gray-lint — merged post-write checkers + command rewriter.
//!
//! One sidecar for gray-lens, gray-shadcn-lint and gray-uv. Each source
//! lives verbatim in `mods/` with its own `~/.gray/<orig>/` state and slash
//! commands — only the wire plumbing is shared.
//!
//! Hook fan-out:
//! - `tool/before` → lens (stashes the write target path) then uv (rewrites
//!   pip/poetry/`python -m venv` bash commands). Chained: a `modify` from an
//!   earlier module re-points later ones at the new args; deny wins, else
//!   the last modify, else allow.
//! - `tool/after`  → shadcn then lens, chained: each sees the previous
//!   module's rewritten `content`, so both appendices survive. shadcn runs
//!   first so it scans the original tool output for the written path.
//! - `tool/call`   → `shadcn_lint` → shadcn.
//! - `command/run` → `/lens`, `/shadcn`, `/uv` routed to their owners;
//!   `/lint` prints an overview.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Mutex;

use serde_json::{Value, json};

mod parts;

#[allow(dead_code)]
#[path = "mods/lens.rs"]
mod lens;
#[allow(dead_code)]
#[path = "mods/shadcn.rs"]
mod shadcn;
#[allow(dead_code)]
#[path = "mods/uv.rs"]
mod uv;

const PARTS: &[parts::Part] = &[
    parts::Part {
        name: "lens",
        default: true,
        description: "post-edit language checkers appended to file results",
    },
    parts::Part {
        name: "shadcn-lint",
        default: true,
        description: "shadcn and Tailwind lint after .tsx/.css writes",
    },
    parts::Part {
        name: "uv",
        default: true,
        description: "rewrite pip, poetry and venv commands to uv",
    },
];

fn manifest(parts: &parts::Parts) -> Value {
    let mut tools: Vec<Value> = Vec::new();
    if parts.on("shadcn-lint") {
        tools.push(json!({
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
        }));
    }
    let mut commands: Vec<&str> = vec!["/lint"];
    let mut descriptions = serde_json::Map::new();
    descriptions.insert("/lint".into(), json!("list the diagnostic checkers"));
    if parts.on("lens") {
        commands.push("/lens");
        descriptions.insert("/lens".into(), json!("toggle post-edit language checkers"));
    }
    if parts.on("shadcn-lint") {
        commands.push("/shadcn");
        descriptions.insert("/shadcn".into(), json!("toggle shadcn and Tailwind lint"));
    }
    if parts.on("uv") {
        commands.push("/uv");
        descriptions.insert("/uv".into(), json!("toggle rewriting pip and poetry to uv"));
    }
    let mut hooks: Vec<&str> = Vec::new();
    if parts.on("lens") || parts.on("uv") {
        hooks.push("tool/before");
    }
    if parts.on("lens") || parts.on("shadcn-lint") {
        hooks.push("tool/after");
    }
    json!({
        "name": "lint",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": tools,
        "commands": commands,
        "command_descriptions": descriptions,
        "hooks": hooks,
        "parts": parts.manifest(PARTS),
    })
}

/// `result` of a module reply (`{}` when it abstained or errored).
fn result_of(r: (Option<Value>, bool)) -> Value {
    r.0.and_then(|v| v.get("result").cloned())
        .unwrap_or_else(|| json!({}))
}

/// `tool/before` chain — lens then uv. Deny wins; otherwise the last
/// `modify` (each module sees earlier rewrites); otherwise allow.
fn tool_before(state: &lens::State, req: &Value, parts: &parts::Parts) -> Value {
    let mut req = req.clone();
    let mut denied: Option<Value> = None;
    let mut modified: Option<Value> = None;
    let mut messages: Vec<String> = Vec::new();
    let lens_step = |r: &Value| result_of(lens::handle_lens(state, r));
    let uv_step = |r: &Value| result_of(uv::handle_uv(r));
    let mut steps: Vec<&dyn Fn(&Value) -> Value> = Vec::new();
    if parts.on("lens") {
        steps.push(&lens_step);
    }
    if parts.on("uv") {
        steps.push(&uv_step);
    }
    for step in steps {
        let r = step(&req);
        for k in ["message", "text"] {
            if let Some(m) = r.get(k).and_then(Value::as_str)
                && !m.trim().is_empty()
            {
                messages.push(m.trim_end().to_string());
            }
        }
        match r.get("decision").and_then(Value::as_str) {
            Some("deny") => {
                denied = Some(r);
                break;
            }
            Some("modify") => {
                if let Some(a) = r.get("args") {
                    req["params"]["args"] = a.clone();
                    modified = Some(a.clone());
                }
            }
            _ => {}
        }
    }
    let mut out = if let Some(d) = denied {
        d
    } else if let Some(args) = modified {
        json!({ "decision": "modify", "args": args })
    } else {
        json!({ "decision": "allow" })
    };
    if !messages.is_empty() && out.get("message").is_none() {
        out["message"] = json!(messages.join("\n\n"));
    }
    out
}

/// `tool/after` chain — shadcn then lens; each sees the previous module's
/// rewritten fields. `{}` when nobody rewrote anything.
fn tool_after(state: &lens::State, req: &Value, parts: &parts::Parts) -> Value {
    let mut req = req.clone();
    let mut changed = serde_json::Map::new();
    let mut apply = |r: Value, req: &mut Value| {
        if let Some(obj) = r.as_object() {
            for (k, val) in obj {
                changed.insert(k.clone(), val.clone());
                req["params"][k] = val.clone();
            }
        }
    };
    if parts.on("shadcn-lint") {
        let r = result_of(shadcn::handle_shadcn(&req));
        apply(r, &mut req);
    }
    if parts.on("lens") {
        let r = result_of(lens::handle_lens(state, &req));
        apply(r, &mut req);
    }
    Value::Object(changed)
}

/// `/lint` — merged-plugin overview.
fn overview(parts: &parts::Parts) -> String {
    let mut lines: Vec<String> = vec![format!(
        "gray-lint {} — one sidecar, three checkers:",
        env!("CARGO_PKG_VERSION")
    )];
    if parts.on("lens") {
        lines.push(
            "\x20 /lens     post-edit language checkers appended to file-tool results (on|off|status|lang)"
                .into(),
        );
    }
    if parts.on("shadcn-lint") {
        lines.push(
            "\x20 /shadcn   shadcn/Tailwind lint after .tsx/.css writes + shadcn_lint tool (on|off)"
                .into(),
        );
    }
    if parts.on("uv") {
        lines.push(
            "\x20 /uv       rewrite pip/poetry/`python -m venv` bash commands to uv (on|off|status)"
                .into(),
        );
    }
    lines.join("\n")
}

fn handle(state: &lens::State, req: &Value, parts: &parts::Parts) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(parts),
        "tool/before" => {
            if !parts.on("lens") && !parts.on("uv") {
                return (Some(json!({ "id": id, "result": {} })), false);
            }
            tool_before(state, req, parts)
        }
        "tool/after" => {
            if !parts.on("lens") && !parts.on("shadcn-lint") {
                return (Some(json!({ "id": id, "result": {} })), false);
            }
            tool_after(state, req, parts)
        }
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name == "shadcn_lint" && parts.on("shadcn-lint") {
                let (reply, _) = shadcn::handle_shadcn(req);
                return (reply, false);
            }
            json!({ "content": format!("unknown tool {name}"), "is_error": true })
        }
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let reply = match name {
                "/lint" => {
                    return (
                        Some(json!({ "id": id, "result": { "text": overview(parts) } })),
                        false,
                    );
                }
                "/lens" if parts.on("lens") => lens::handle_lens(state, req).0,
                "/shadcn" if parts.on("shadcn-lint") => shadcn::handle_shadcn(req).0,
                "/uv" if parts.on("uv") => uv::handle_uv(req).0,
                _ => {
                    let text = format!("unknown command {name} — try /lint");
                    return (Some(json!({ "id": id, "result": { "text": text } })), false);
                }
            };
            return (reply, false);
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main() -> std::io::Result<()> {
    let parts = parts::Parts::load("lint", PARTS);
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest(&parts));
        return Ok(());
    }
    let state = lens::State {
        stash: Mutex::new(HashMap::new()),
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (reply, exit) = handle(&state, &req, &parts);
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

    fn st() -> lens::State {
        lens::State {
            stash: Mutex::new(HashMap::new()),
        }
    }

    fn call(s: &lens::State, method: &str, params: Value) -> Value {
        let parts = parts::Parts::load("lint", PARTS);
        handle(
            s,
            &json!({ "id": 1, "method": method, "params": params }),
            &parts,
        )
        .0
        .unwrap()
    }

    #[test]
    fn manifest_is_union() {
        let m = manifest(&parts::Parts::load("lint", PARTS));
        assert_eq!(m["name"], "lint");
        assert_eq!(m["hooks"], json!(["tool/before", "tool/after"]));
        assert_eq!(m["tools"][0]["name"], "shadcn_lint");
    }

    #[test]
    fn uv_rewrite_survives_chain() {
        let r = call(
            &st(),
            "tool/before",
            json!({"name":"bash","args":{"command":"pip install x"},"session":{"id":"s"}}),
        );
        assert_eq!(r["result"]["decision"], "modify");
        assert_eq!(r["result"]["args"]["command"], "uv pip install x");
    }

    #[test]
    fn lens_stash_survives_chain() {
        let s = st();
        let r = call(
            &s,
            "tool/before",
            json!({"name":"write","args":{"path":"/tmp/x.py"},"session":{"id":"s"}}),
        );
        assert_eq!(r["result"]["decision"], "allow");
        assert_eq!(s.stash.lock().unwrap().len(), 1);
    }

    #[test]
    fn commands_route() {
        let s = st();
        for c in ["/lint", "/lens", "/shadcn", "/uv"] {
            let r = call(&s, "command/run", json!({"name": c, "argv": ["status"]}));
            assert!(r["result"]["text"].as_str().is_some(), "{c}: {r}");
        }
    }
}
