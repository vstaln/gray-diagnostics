//! gray-diagnostics — merged post-write checkers + command rewriter.
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
//!   `/diagnostics` prints an overview.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Mutex;

use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "mods/lens.rs"]
mod lens;
#[allow(dead_code)]
#[path = "mods/shadcn.rs"]
mod shadcn;
#[allow(dead_code)]
#[path = "mods/uv.rs"]
mod uv;

fn manifest() -> Value {
    json!({
        "name": "diagnostics",
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
        "commands": ["/diagnostics", "/lens", "/shadcn", "/uv"],
        "hooks": ["tool/before", "tool/after"],
    })
}

/// `result` of a module reply (`{}` when it abstained or errored).
fn result_of(r: (Option<Value>, bool)) -> Value {
    r.0.and_then(|v| v.get("result").cloned()).unwrap_or_else(|| json!({}))
}

/// `tool/before` chain — lens then uv. Deny wins; otherwise the last
/// `modify` (each module sees earlier rewrites); otherwise allow.
fn tool_before(state: &lens::State, req: &Value) -> Value {
    let mut req = req.clone();
    let mut denied: Option<Value> = None;
    let mut modified: Option<Value> = None;
    let mut messages: Vec<String> = Vec::new();
    let steps: [&dyn Fn(&Value) -> Value; 2] = [
        &|r| result_of(lens::handle_lens(state, r)),
        &|r| result_of(uv::handle_uv(r)),
    ];
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
fn tool_after(state: &lens::State, req: &Value) -> Value {
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
    let r = result_of(shadcn::handle_shadcn(&req));
    apply(r, &mut req);
    let r = result_of(lens::handle_lens(state, &req));
    apply(r, &mut req);
    Value::Object(changed)
}

/// `/diagnostics` — merged-plugin overview.
fn overview() -> String {
    format!(
        "gray-diagnostics {} — one sidecar, three checkers:\n\
         \x20 /lens     post-edit language checkers appended to file-tool results (on|off|status|lang)\n\
         \x20 /shadcn   shadcn/Tailwind lint after .tsx/.css writes + shadcn_lint tool (on|off)\n\
         \x20 /uv       rewrite pip/poetry/`python -m venv` bash commands to uv (on|off|status)",
        env!("CARGO_PKG_VERSION")
    )
}

fn handle(state: &lens::State, req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let result = match method {
        "plugin/manifest" => manifest(),
        "tool/before" => tool_before(state, req),
        "tool/after" => tool_after(state, req),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if name == "shadcn_lint" {
                let (reply, _) = shadcn::handle_shadcn(req);
                return (reply, false);
            }
            json!({ "content": format!("unknown tool {name}"), "is_error": true })
        }
        "command/run" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let reply = match name {
                "/diagnostics" => return (Some(json!({ "id": id, "result": { "text": overview() } })), false),
                "/lens" => lens::handle_lens(state, req).0,
                "/shadcn" => shadcn::handle_shadcn(req).0,
                "/uv" => uv::handle_uv(req).0,
                _ => {
                    let text = format!("unknown command {name} — try /diagnostics");
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
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return Ok(());
    }
    let state = lens::State { stash: Mutex::new(HashMap::new()) };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (reply, exit) = handle(&state, &req);
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
        lens::State { stash: Mutex::new(HashMap::new()) }
    }

    fn call(s: &lens::State, method: &str, params: Value) -> Value {
        handle(s, &json!({ "id": 1, "method": method, "params": params })).0.unwrap()
    }

    #[test]
    fn manifest_is_union() {
        let m = manifest();
        assert_eq!(m["name"], "diagnostics");
        assert_eq!(m["hooks"], json!(["tool/before", "tool/after"]));
        assert_eq!(m["tools"][0]["name"], "shadcn_lint");
    }

    #[test]
    fn uv_rewrite_survives_chain() {
        let r = call(&st(), "tool/before",
            json!({"name":"bash","args":{"command":"pip install x"},"session":{"id":"s"}}));
        assert_eq!(r["result"]["decision"], "modify");
        assert_eq!(r["result"]["args"]["command"], "uv pip install x");
    }

    #[test]
    fn lens_stash_survives_chain() {
        let s = st();
        let r = call(&s, "tool/before",
            json!({"name":"write","args":{"path":"/tmp/x.py"},"session":{"id":"s"}}));
        assert_eq!(r["result"]["decision"], "allow");
        assert_eq!(s.stash.lock().unwrap().len(), 1);
    }

    #[test]
    fn commands_route() {
        let s = st();
        for c in ["/diagnostics", "/lens", "/shadcn", "/uv"] {
            let r = call(&s, "command/run", json!({"name": c, "argv": ["status"]}));
            assert!(r["result"]["text"].as_str().is_some(), "{c}: {r}");
        }
    }
}
