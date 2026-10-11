//! Claude Code and Codex for the agent panel (`vectorcraft_ui_egui::agent_panel`).
//!
//! Each message starts the user's own CLI headless (`claude -p`, `codex exec`) with its JSON event
//! stream, and an MCP server that is this executable in `--mcp-bridge` mode, connected back to
//! this window's control channel. The CLIs use the login the user made with them (their Claude or
//! ChatGPT subscription): the API key variables are removed from their environment so a key set
//! for something else is never billed instead.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use vectorcraft_ui_egui::agent_panel::{AgentEvent, AgentRequest, AgentRun, AgentStartFn};
use vectorcraft_ui_egui::state::AgentKind;

/// The MCP server name the agents see (`mcp__vectorcraft__…` in Claude Code).
const SERVER: &str = "vectorcraft";

/// What the agents are told about where they are.
const INSTRUCTIONS: &str = "You are inside VectorCraft, an Illustrator-like vector illustration app, and act on the \
document open in its window through the `vectorcraft` MCP tools. Use `command_list` (with a filter) to find command ids \
and their params, `doc_inspect` to read the layers, artboards and selection, `command_run` or `command_batch` to edit \
(or `draw_shape`, `draw_path`, `set_paint`, `add_text` for quick drawing), and `ui_screenshot` or `render_preview` to \
look at the result. Every edit is undoable, so make ordinary edits without asking. Keep replies short: say what you \
changed. Reply in the language the user writes in.";

/// Environment variables that would make the CLIs bill an API account instead of the user's
/// subscription login.
const API_KEY_VARS: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY", "CODEX_API_KEY"];

/// The most stderr kept to explain a failed run.
const MAX_STDERR: usize = 16 * 1024;

/// The agent service. `existing` is the control port the app was launched with (`--control
/// <port>`), if any; otherwise one is started on a free port at the first message.
pub fn service(existing: Option<u16>) -> AgentStartFn {
    let mut endpoint = existing;
    Box::new(move |req: AgentRequest, ctx: egui::Context| {
        let mut control = None;
        let port = match endpoint {
            Some(port) => port,
            None => {
                let (rx, port) = crate::control_server::start_ephemeral(ctx.clone())
                    .map_err(|e| format!("Could not start the control channel for the agent: {e}"))?;
                control = Some(rx);
                endpoint = Some(port);
                port
            }
        };
        let mut run = start(&req, &ctx, port)?;
        run.control = control;
        Ok(run)
    })
}

/// Files the agents use: the MCP config and their working directory.
struct Paths {
    mcp_config: PathBuf,
    workspace: PathBuf,
}

fn prepare(port: u16) -> Result<Paths, String> {
    let dir = crate::prefs_dir::prefs_path_for("VectorCraft", "vectorcraft")
        .and_then(|p| Some(p.parent()?.to_path_buf()))
        .unwrap_or_else(|| std::env::temp_dir().join("vectorcraft"))
        .join("Agents");
    // The agents run here, away from any project, so they pick up no CLAUDE.md / AGENTS.md.
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).map_err(|e| format!("Could not create {}: {e}", workspace.display()))?;
    let exe = std::env::current_exe().map_err(|e| format!("Could not find the VectorCraft executable: {e}"))?;
    let config = json!({
        "mcpServers": {
            SERVER: {
                "type": "stdio",
                "command": exe,
                "args": bridge_args(port),
            }
        }
    });
    let mcp_config = dir.join("mcp.json");
    std::fs::write(&mcp_config, config.to_string()).map_err(|e| format!("Could not write {}: {e}", mcp_config.display()))?;
    Ok(Paths { mcp_config, workspace })
}

fn bridge_args(port: u16) -> Vec<String> {
    vec!["--mcp-bridge".into(), format!("127.0.0.1:{port}")]
}

/// Folders where the CLIs' installers put them; an app started from the Dock or a desktop menu
/// does not get the login shell's PATH.
fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from) {
        for sub in [".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin", ".volta/bin", ".cargo/bin", "AppData/Roaming/npm"] {
            dirs.push(home.join(sub));
        }
    }
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// The search path: the inherited PATH, then [`extra_dirs`].
fn search_path() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for d in extra_dirs() {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

fn find_program(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) { vec![format!("{name}.exe"), format!("{name}.cmd")] } else { vec![name.to_string()] };
    dirs.iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.is_file())
}

fn program(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
    }
}

fn not_installed(kind: AgentKind) -> String {
    match kind {
        AgentKind::Claude => {
            "Claude Code is not installed. Install it (https://claude.com/claude-code), run `claude` once in a terminal and sign in with your Claude subscription.".into()
        }
        AgentKind::Codex => "Codex is not installed. Install it (`npm install -g @openai/codex` or `brew install codex`) and run `codex login` once with your ChatGPT account.".into(),
    }
}

/// The CLI arguments for one message.
fn args(req: &AgentRequest, port: u16, paths: &Paths) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    match req.kind {
        AgentKind::Claude => {
            a.extend(["-p".into(), req.prompt.clone().into(), "--output-format".into(), "stream-json".into(), "--verbose".into()]);
            a.extend(["--mcp-config".into(), paths.mcp_config.clone().into_os_string(), "--strict-mcp-config".into()]);
            // Only VectorCraft's tools: no shell, no file edits.
            a.extend(["--tools".into(), "".into(), "--allowedTools".into(), format!("mcp__{SERVER}").into()]);
            a.extend(["--append-system-prompt".into(), INSTRUCTIONS.into()]);
            if let Some(session) = &req.session {
                a.extend(["--resume".into(), session.into()]);
            }
        }
        AgentKind::Codex => {
            a.extend(["exec".into(), "--json".into(), "--skip-git-repo-check".into(), "--sandbox".into(), "read-only".into()]);
            let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            // `-c key=value` takes TOML values; JSON strings and arrays of strings are valid TOML.
            let toml_args = serde_json::to_string(&bridge_args(port)).unwrap_or_else(|_| "[]".into());
            for c in [
                "approval_policy=\"never\"".to_string(),
                format!("mcp_servers.{SERVER}.command={}", Value::String(exe)),
                format!("mcp_servers.{SERVER}.args={toml_args}"),
                // Codex asks before every tool without a read-only hint, and `never` turns that
                // ask into a refusal: pre-approve VectorCraft's tools (the shell stays read-only).
                format!("mcp_servers.{SERVER}.default_tools_approval_mode=\"approve\""),
            ] {
                a.extend(["-c".into(), c.into()]);
            }
            match &req.session {
                Some(session) => a.extend(["resume".into(), session.into(), req.prompt.clone().into()]),
                // Codex has no extra system prompt flag: the first message carries the context.
                None => a.push(format!("{INSTRUCTIONS}\n\n{}", req.prompt).into()),
            }
        }
    }
    a
}

fn start(req: &AgentRequest, ctx: &egui::Context, port: u16) -> Result<AgentRun, String> {
    let dirs = search_path();
    let Some(exe) = find_program(program(req.kind), &dirs) else {
        return Err(not_installed(req.kind));
    };
    let paths = prepare(port)?;
    let mut cmd = Command::new(&exe);
    cmd.args(args(req, port, &paths)).current_dir(&paths.workspace).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for var in API_KEY_VARS {
        cmd.env_remove(var);
    }
    // npm-installed CLIs start through `env node`: give them the same search path.
    if let Ok(path) = std::env::join_paths(exe.parent().map(Path::to_path_buf).into_iter().chain(dirs)) {
        cmd.env("PATH", path);
    }
    let mut child = cmd.spawn().map_err(|e| format!("Could not start {}: {e}", exe.display()))?;
    let stdout = child.stdout.take().ok_or("no output from the agent")?;
    let stderr = child.stderr.take();
    let child = Arc::new(Mutex::new(child));
    let (tx, rx) = channel();
    let kind = req.kind;
    let reader_child = Arc::clone(&child);
    let ctx = ctx.clone();
    std::thread::spawn(move || read_events(kind, stdout, stderr, &reader_child, &tx, &ctx));
    let cancel = move || {
        // Best effort: the process may have ended already.
        let _ = child.lock().unwrap_or_else(PoisonError::into_inner).kill();
    };
    Ok(AgentRun { events: rx, cancel: Box::new(cancel), control: None })
}

fn read_events(
    kind: AgentKind,
    stdout: impl Read,
    stderr: Option<impl Read + Send + 'static>,
    child: &Mutex<Child>,
    tx: &Sender<AgentEvent>,
    ctx: &egui::Context,
) {
    let stderr_text: Receiver<String> = {
        let (etx, erx) = channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut e) = stderr {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = e.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    if buf.len() < MAX_STDERR {
                        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                    }
                }
            }
            // The receiver is gone only when the reader stopped waiting: nothing to report then.
            let _ = etx.send(String::from_utf8_lossy(&buf).into_owned());
        });
        erx
    };
    let send = |e: AgentEvent| {
        // The panel closed the run (Stop): nobody listens any more.
        let _ = tx.send(e);
        ctx.request_repaint();
    };
    let mut reported = false;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let events = match kind {
            AgentKind::Claude => claude_events(&v),
            AgentKind::Codex => codex_events(&v),
        };
        for e in events {
            reported |= matches!(e, AgentEvent::Text(_) | AgentEvent::Error(_));
            send(e);
        }
    }
    let status = child.lock().unwrap_or_else(PoisonError::into_inner).wait();
    let stderr = stderr_text.recv().unwrap_or_default();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) if !reported => send(AgentEvent::Error(failure_message(kind, s.code(), &stderr))),
        Ok(_) => {}
        Err(e) => send(AgentEvent::Error(format!("{} stopped: {e}", kind.label()))),
    }
    send(AgentEvent::Done);
}

/// Explains a run that ended without saying anything, pointing at the login when that is it.
fn failure_message(kind: AgentKind, code: Option<i32>, stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    if ["login", "log in", "logged in", "authenticat", "api key", "unauthorized", "401"].iter().any(|k| lower.contains(k)) {
        return match kind {
            AgentKind::Claude => "Claude Code is not signed in. Run `claude` in a terminal and use /login with your Claude subscription.".into(),
            AgentKind::Codex => "Codex is not signed in. Run `codex login` in a terminal and choose Sign in with ChatGPT.".into(),
        };
    }
    let tail: String = {
        let chars: Vec<char> = stderr.trim().chars().collect();
        chars.get(chars.len().saturating_sub(600)..).unwrap_or_default().iter().collect()
    };
    let code = code.map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
    if tail.is_empty() { format!("{} exited with {code}.", kind.label()) } else { format!("{} exited with {code}: {tail}", kind.label()) }
}

/// A tool call as one short line: the tool and its command id or method when it has one.
fn tool_line(tool: &str, input: &Value) -> String {
    let tool = tool.strip_prefix(&format!("mcp__{SERVER}__")).unwrap_or(tool);
    let detail = ["id", "command", "method", "filter", "shape"].iter().find_map(|k| input.get(*k).and_then(Value::as_str));
    let steps = input.get("steps").and_then(Value::as_array).map(|s| {
        let ids: Vec<&str> = s.iter().filter_map(|st| st.get("id").or_else(|| st.get("command")).and_then(Value::as_str)).collect();
        ids.join(", ")
    });
    match (detail, steps) {
        (Some(d), _) => format!("{tool} {d}"),
        (None, Some(s)) if !s.is_empty() => format!("{tool} [{s}]"),
        _ => tool.to_string(),
    }
}

/// Claude Code's `--output-format stream-json` lines.
fn claude_events(v: &Value) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            if let Some(id) = v.get("session_id").and_then(Value::as_str) {
                out.push(AgentEvent::Session(id.to_string()));
            }
            let failed = v.get("mcp_servers").and_then(Value::as_array).into_iter().flatten().any(|s| {
                s.get("name").and_then(Value::as_str) == Some(SERVER) && s.get("status").and_then(Value::as_str).is_some_and(|st| st != "connected")
            });
            if failed {
                out.push(AgentEvent::Error("Claude Code could not connect to VectorCraft's tools.".into()));
            }
        }
        Some("assistant") => {
            for c in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                match c.get("type").and_then(Value::as_str) {
                    Some("text") => out.extend(c.get("text").and_then(Value::as_str).map(|t| AgentEvent::Text(t.to_string()))),
                    Some("tool_use") => {
                        let name = c.get("name").and_then(Value::as_str).unwrap_or("tool");
                        out.push(AgentEvent::Tool(tool_line(name, c.get("input").unwrap_or(&Value::Null))));
                    }
                    _ => {}
                }
            }
        }
        Some("result") => {
            if let Some(id) = v.get("session_id").and_then(Value::as_str) {
                out.push(AgentEvent::Session(id.to_string()));
            }
            if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                let msg =
                    v.get("result").and_then(Value::as_str).or_else(|| v.get("subtype").and_then(Value::as_str)).unwrap_or("Claude Code failed");
                out.push(AgentEvent::Error(msg.to_string()));
            }
        }
        _ => {}
    }
    out
}

/// Codex's `exec --json` lines.
fn codex_events(v: &Value) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    let item = v.get("item");
    let item_type = item.and_then(|i| i.get("type")).and_then(Value::as_str);
    match v.get("type").and_then(Value::as_str) {
        Some("thread.started") => out.extend(v.get("thread_id").and_then(Value::as_str).map(|id| AgentEvent::Session(id.to_string()))),
        Some("item.started") if item_type == Some("mcp_tool_call") => {
            let i = item.unwrap_or(&Value::Null);
            let tool = i.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let args = i.get("arguments").cloned().unwrap_or(Value::Null);
            // Some versions send the arguments as a JSON string.
            let args = match &args {
                Value::String(s) => serde_json::from_str(s).unwrap_or(args),
                _ => args,
            };
            out.push(AgentEvent::Tool(tool_line(tool, &args)));
        }
        Some("item.completed") => {
            let i = item.unwrap_or(&Value::Null);
            match item_type {
                Some("agent_message") => out.extend(i.get("text").and_then(Value::as_str).map(|t| AgentEvent::Text(t.to_string()))),
                Some("mcp_tool_call") if i.get("status").and_then(Value::as_str) == Some("failed") => {
                    let msg = i.pointer("/error/message").and_then(Value::as_str).unwrap_or("a VectorCraft tool call failed");
                    out.push(AgentEvent::Error(msg.to_string()));
                }
                Some("error") => out.extend(i.get("message").and_then(Value::as_str).map(|m| AgentEvent::Error(m.to_string()))),
                _ => {}
            }
        }
        Some("turn.failed") => {
            out.push(AgentEvent::Error(v.pointer("/error/message").and_then(Value::as_str).unwrap_or("Codex failed").to_string()));
        }
        Some("error") => out.extend(v.get("message").and_then(Value::as_str).map(|m| AgentEvent::Error(m.to_string()))),
        _ => {}
    }
    out
}

/// `vectorcraft --mcp-bridge <127.0.0.1:port>`: the MCP server the agents start, forwarding to a
/// running window. Returns whether it ran (and ended) cleanly.
pub fn mcp_bridge_main(args: &[OsString]) -> bool {
    let mut addr = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--mcp-bridge") => addr = it.next().and_then(|v| v.to_str()).map(str::to_owned),
            _ => {
                eprintln!("vectorcraft: --mcp-bridge: unexpected argument `{}`", a.to_string_lossy());
                return false;
            }
        }
    }
    let Some(addr) = addr else {
        eprintln!("vectorcraft: --mcp-bridge needs an address (127.0.0.1:<port>)");
        return false;
    };
    let remote = match vectorcraft_mcp::Remote::connect(&addr) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("vectorcraft: MCP bridge: cannot connect to {addr}: {e}");
            return false;
        }
    };
    vectorcraft_mcp::logging::install();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match vectorcraft_mcp::Server::new(Box::new(remote)).serve(stdin.lock(), stdout.lock()) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("vectorcraft: MCP bridge: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths { mcp_config: PathBuf::from("/cfg/mcp.json"), workspace: PathBuf::from("/cfg/workspace") }
    }

    fn strings(a: Vec<OsString>) -> Vec<String> {
        a.into_iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn claude_runs_print_mode_with_only_vectorcraft_tools_and_resumes() {
        let req = AgentRequest { kind: AgentKind::Claude, prompt: "draw".into(), session: Some("s1".into()) };
        let a = strings(args(&req, 4000, &paths()));
        assert_eq!(a.get(..2), Some(&["-p".to_string(), "draw".to_string()][..]));
        let after = |flag: &str| a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned();
        assert_eq!(after("--output-format").as_deref(), Some("stream-json"));
        assert_eq!(after("--tools").as_deref(), Some(""));
        assert_eq!(after("--allowedTools").as_deref(), Some("mcp__vectorcraft"));
        assert_eq!(after("--resume").as_deref(), Some("s1"));
        assert!(a.contains(&"--strict-mcp-config".to_string()));
    }

    #[test]
    fn codex_gets_the_bridge_as_toml_overrides_and_resumes_with_the_prompt_last() {
        let req = AgentRequest { kind: AgentKind::Codex, prompt: "draw".into(), session: None };
        let a = strings(args(&req, 4000, &paths()));
        assert_eq!(a.first().map(String::as_str), Some("exec"));
        assert!(a.iter().any(|x| x == r#"mcp_servers.vectorcraft.args=["--mcp-bridge","127.0.0.1:4000"]"#));
        assert!(a.iter().any(|x| x == r#"mcp_servers.vectorcraft.default_tools_approval_mode="approve""#));
        assert!(a.last().is_some_and(|p| p.starts_with("You are inside VectorCraft") && p.ends_with("draw")));
        let req = AgentRequest { session: Some("t1".into()), ..req };
        let a = strings(args(&req, 4000, &paths()));
        assert_eq!(a.get(a.len() - 3..), Some(&["resume".to_string(), "t1".to_string(), "draw".to_string()][..]));
    }

    #[test]
    fn claude_stream_json_becomes_events() {
        let init = json!({"type":"system","subtype":"init","session_id":"abc","mcp_servers":[{"name":"vectorcraft","status":"failed"}]});
        assert_eq!(
            claude_events(&init),
            vec![AgentEvent::Session("abc".into()), AgentEvent::Error("Claude Code could not connect to VectorCraft's tools.".into())]
        );
        let msg = json!({"type":"assistant","message":{"content":[
            {"type":"text","text":"Drawing."},
            {"type":"tool_use","name":"mcp__vectorcraft__command_run","input":{"id":"shape.ellipse","params":{"width":40}}},
            {"type":"tool_use","name":"mcp__vectorcraft__command_batch","input":{"steps":[{"id":"a"},{"id":"b"}]}}
        ]}});
        assert_eq!(
            claude_events(&msg),
            vec![
                AgentEvent::Text("Drawing.".into()),
                AgentEvent::Tool("command_run shape.ellipse".into()),
                AgentEvent::Tool("command_batch [a, b]".into())
            ]
        );
        let err = json!({"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"abc"});
        assert_eq!(claude_events(&err), vec![AgentEvent::Session("abc".into()), AgentEvent::Error("error_during_execution".into())]);
        assert!(claude_events(&json!("not an object")).is_empty());
    }

    #[test]
    fn codex_json_becomes_events() {
        assert_eq!(codex_events(&json!({"type":"thread.started","thread_id":"t1"})), vec![AgentEvent::Session("t1".into())]);
        let call = json!({"type":"item.started","item":{"type":"mcp_tool_call","server":"vectorcraft","tool":"command_run","arguments":"{\"id\":\"object.group\"}"}});
        assert_eq!(codex_events(&call), vec![AgentEvent::Tool("command_run object.group".into())]);
        let msg = json!({"type":"item.completed","item":{"type":"agent_message","text":"Grouped."}});
        assert_eq!(codex_events(&msg), vec![AgentEvent::Text("Grouped.".into())]);
        let failed = json!({"type":"turn.failed","error":{"message":"usage limit"}});
        assert_eq!(codex_events(&failed), vec![AgentEvent::Error("usage limit".into())]);
        assert!(codex_events(&json!({"type":"item.completed"})).is_empty());
    }

    #[test]
    fn failures_point_at_the_login_or_show_the_stderr_tail() {
        assert!(failure_message(AgentKind::Claude, Some(1), "Invalid API key · Please run /login").contains("/login"));
        assert!(failure_message(AgentKind::Codex, Some(1), "error: not logged in").contains("codex login"));
        assert_eq!(failure_message(AgentKind::Codex, Some(3), ""), "Codex exited with code 3.");
        let long = "é".repeat(2000);
        assert!(failure_message(AgentKind::Claude, None, &long).starts_with("Claude Code exited with a signal: "));
    }

    #[test]
    fn bridge_mode_rejects_bad_arguments_without_panicking() {
        assert!(!mcp_bridge_main(&["--mcp-bridge".into()]));
        assert!(!mcp_bridge_main(&["--mcp-bridge".into(), "127.0.0.1:1".into(), "--bogus".into()]));
        // Nothing listens on port 1: an error, not a hang.
        assert!(!mcp_bridge_main(&["--mcp-bridge".into(), "127.0.0.1:1".into()]));
    }

    #[test]
    fn programs_are_found_on_the_search_path() {
        let dir = std::env::temp_dir().join(format!("vectorcraft-agent-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "claude.exe" } else { "claude" };
        std::fs::write(dir.join(name), "").unwrap();
        assert_eq!(find_program("claude", std::slice::from_ref(&dir)), Some(dir.join(name)));
        assert_eq!(find_program("codex", std::slice::from_ref(&dir)), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
