//! Tier 3i (v1.0): MCP (Model Context Protocol) client.
//!
//! Spawns a configured [`McpServerSpec`] as a child process, speaks JSON-RPC
//! 2.0 over stdio, and exposes a small typed surface ([`McpClient::start`],
//! [`McpClient::list_tools`], [`McpClient::call_tool`], [`McpClient::shutdown`])
//! plus an [`McpRegistry`] used by the orchestrator to register every
//! configured server at session start.
//!
//! Security guards (proposal §3 Tier 3i):
//!
//! * `read_only: true` (default) rejects any tool whose name matches the
//!   configurable `write_tool_patterns` blacklist (`shell`, `exec`, `fs.write`,
//!   `fs.delete`, `write_file`, `run_command`, `execute_command`, …) before
//!   the request reaches the wire.
//! * `spec.allowed_tools` (when non-empty) is an explicit per-server allowlist
//!   intersected with the global `mcp.allow_tools` filter at registry-level.
//! * `role` filters which orchestrator role (reviewer / lead) may discover the
//!   server's tools via [`McpRegistry::all_tools`].
//!
//! Lifecycle:
//!
//! * [`McpClient::start`] spawns the child with `Stdio::piped()`, performs the
//!   `initialize` handshake, lists tools via `tools/list`, and caches them.
//! * [`McpClient::shutdown`] closes stdin (EOF), waits 2s for graceful exit,
//!   then escalates to SIGTERM / kill.
//!
//! Only the stdio transport is supported in v1.0 ([`McpTransport::Stdio`]).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nerve_types::{McpRole, McpServerSpec, McpToolCall, McpToolInfo, McpToolResult, McpTransport};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::time::timeout;
use tracing::{debug, warn};

/// JSON-RPC protocol version advertised by Nerve's client implementation.
/// Matches the spec revision the public MCP servers built between 2024 and 2026
/// understand. Bumping requires re-validating handshake compatibility.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Per-call timeout applied to `tools/call` requests. Keeps a misbehaving
/// server from indefinitely stalling reviewer/lead dispatch.
pub const MCP_CALL_TIMEOUT_SECS: u64 = 30;

/// Timeout used for the `initialize` + `tools/list` handshake. Servers that
/// take longer than this to start almost certainly indicate a misconfigured
/// command and should fail loudly rather than block session startup.
pub const MCP_HANDSHAKE_TIMEOUT_SECS: u64 = 10;

/// Grace period between EOF on stdin and SIGTERM during shutdown.
pub const MCP_SHUTDOWN_GRACE_SECS: u64 = 2;

/// Default write-tool blacklist enforced when `spec.read_only` is true and the
/// caller did not provide a custom list. Mirrors the patterns documented in
/// proposal §3 Tier 3i.
pub fn default_write_tool_patterns() -> Vec<String> {
    vec![
        "shell".to_string(),
        "exec".to_string(),
        "fs.write".to_string(),
        "fs.delete".to_string(),
        "write_file".to_string(),
        "run_command".to_string(),
        "execute_command".to_string(),
    ]
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("server not started: {0}")]
    NotStarted(String),
    #[error("tool not allowed: {0}")]
    ToolBlocked(String),
    #[error("tool not in allowlist: {0}")]
    ToolNotAllowed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error("rpc error: code={0} message={1}")]
    Rpc(i32, String),
    #[error("timeout after {0}s")]
    Timeout(u32),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// JSON-RPC 2.0 client over stdio bound to a single configured MCP server.
///
/// Construction is cheap; the child process is not spawned until
/// [`McpClient::start`] is invoked. After [`McpClient::shutdown`] the client
/// can no longer be reused — callers should drop it and create a new one if
/// the server needs to be restarted.
pub struct McpClient {
    name: String,
    spec: McpServerSpec,
    process: Mutex<Option<Child>>,
    next_request_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcReply>>>>,
    write_tx: Mutex<Option<mpsc::Sender<String>>>,
    tools_cache: RwLock<Vec<McpToolInfo>>,
    write_tool_patterns: Vec<String>,
}

#[derive(Debug)]
enum RpcReply {
    Ok(Value),
    Err(i32, String),
}

impl McpClient {
    /// Construct a new client bound to `spec`. The child process is **not**
    /// spawned until [`McpClient::start`] is called — this lets callers
    /// register multiple clients up-front and start them lazily.
    ///
    /// `write_tool_patterns` overrides the read-only blacklist enforced by
    /// [`McpClient::call_tool`]. Pass an empty vector to disable the guard
    /// even when `spec.read_only` is true (typically only useful in tests).
    pub fn new(spec: McpServerSpec, write_tool_patterns: Vec<String>) -> Self {
        let name = spec.name.clone();
        Self {
            name,
            spec,
            process: Mutex::new(None),
            next_request_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            write_tx: Mutex::new(None),
            tools_cache: RwLock::new(Vec::new()),
            write_tool_patterns,
        }
    }

    /// Spawn the configured server, run the `initialize` handshake, list the
    /// advertised tools, and cache them. Idempotent only in the sense that
    /// calling start twice surfaces a [`McpError::Spawn`] — callers should
    /// shut down and reconstruct the client to restart it.
    pub async fn start(&self) -> Result<(), McpError> {
        // Reject double-start so we don't leak the previous child.
        if self.process.lock().await.is_some() {
            return Err(McpError::Spawn(format!(
                "mcp server `{}` already started",
                self.name
            )));
        }

        // Only stdio is supported in v1.0; the enum is reserved for future
        // transports. Match exhaustively so adding a variant forces a compile
        // error here rather than silently spawning the wrong way.
        match self.spec.transport {
            McpTransport::Stdio => {}
        }

        let argv = &self.spec.command;
        if argv.is_empty() {
            return Err(McpError::Spawn(format!(
                "mcp server `{}`: command argv is empty",
                self.name
            )));
        }

        let mut cmd = Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        for (key, value) in &self.spec.env {
            cmd.env(key, value);
        }
        // `kill_on_drop` is the last line of defence if shutdown() is bypassed
        // (e.g., orchestrator panic). Without it the child can outlive Nerve.
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|err| McpError::Spawn(format!("`{}`: {err}", self.name)))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Spawn(format!("`{}`: stdin pipe missing", self.name)))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Spawn(format!("`{}`: stdout pipe missing", self.name)))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpError::Spawn(format!("`{}`: stderr pipe missing", self.name)))?;

        // Reader task: line-delimited JSON-RPC. Notifications (no `id`) are
        // logged at debug; replies are matched against `pending` and delivered
        // via the per-request oneshot.
        let pending_reader = Arc::clone(&self.pending);
        let reader_name = self.name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let value: Value = match serde_json::from_str(trimmed) {
                            Ok(value) => value,
                            Err(err) => {
                                warn!(server = %reader_name, error = %err, "non-JSON line on mcp stdout");
                                continue;
                            }
                        };
                        dispatch_incoming(&reader_name, value, &pending_reader).await;
                    }
                    Ok(None) => break,
                    Err(err) => {
                        warn!(server = %reader_name, error = %err, "mcp stdout read error");
                        break;
                    }
                }
            }
            // EOF: drain any pending requests so callers don't hang forever.
            let mut pending = pending_reader.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(RpcReply::Err(-32000, "server closed stdout".to_string()));
            }
        });

        // Stderr reader is purely diagnostic; surface lines at warn so a
        // misbehaving server is visible without spamming info-level logs.
        let stderr_name = self.name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                warn!(server = %stderr_name, line = %line, "mcp server stderr");
            }
        });

        // Writer task: one mpsc consumer so concurrent send_request calls
        // serialize naturally without us holding a Mutex<Stdin> across awaits.
        let (write_tx, mut write_rx) = mpsc::channel::<String>(64);
        let writer_name = self.name.clone();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = write_rx.recv().await {
                if let Err(err) = stdin.write_all(line.as_bytes()).await {
                    warn!(server = %writer_name, error = %err, "mcp stdin write failed");
                    break;
                }
                if let Err(err) = stdin.write_all(b"\n").await {
                    warn!(server = %writer_name, error = %err, "mcp stdin newline write failed");
                    break;
                }
                if let Err(err) = stdin.flush().await {
                    warn!(server = %writer_name, error = %err, "mcp stdin flush failed");
                    break;
                }
            }
            // EOF stdin so the server observes graceful close on shutdown.
            let _ = stdin.shutdown().await;
        });

        *self.process.lock().await = Some(child);
        *self.write_tx.lock().await = Some(write_tx);

        // Handshake under a wall-clock guard so a stuck server can't block the
        // whole session start. Failure here tears the client back down.
        let handshake = self.handshake();
        match timeout(Duration::from_secs(MCP_HANDSHAKE_TIMEOUT_SECS), handshake).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                let _ = self.shutdown().await;
                Err(err)
            }
            Err(_) => {
                let _ = self.shutdown().await;
                Err(McpError::Timeout(MCP_HANDSHAKE_TIMEOUT_SECS as u32))
            }
        }
    }

    async fn handshake(&self) -> Result<(), McpError> {
        let init_params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "nerve",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let init_result = self.send_request("initialize", init_params).await?;
        debug!(server = %self.name, result = ?init_result, "mcp initialize ok");

        // Per spec, a `notifications/initialized` notification follows. We do
        // not strictly need to wait for an ack — fire and forget.
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        });
        self.write_line(serde_json::to_string(&notification)?)
            .await?;

        let tools_value = self.send_request("tools/list", json!({})).await?;
        let tools = parse_tools_list(&self.name, &tools_value)?;
        *self.tools_cache.write().await = tools;
        Ok(())
    }

    /// Return the cached tool list captured during [`McpClient::start`].
    /// Returns [`McpError::NotStarted`] if the client has not been started.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        if self.process.lock().await.is_none() {
            return Err(McpError::NotStarted(self.name.clone()));
        }
        Ok(self.tools_cache.read().await.clone())
    }

    /// Dispatch a `tools/call` request, enforcing the read-only blacklist and
    /// the per-server allowlist before sending anything on the wire.
    pub async fn call_tool(&self, call: McpToolCall) -> Result<McpToolResult, McpError> {
        if self.process.lock().await.is_none() {
            return Err(McpError::NotStarted(self.name.clone()));
        }

        // Per-server allowlist: when non-empty, the tool name must appear
        // verbatim. We check this before the read-only guard so callers see
        // the most specific reason a request was rejected.
        if !self.spec.allowed_tools.is_empty()
            && !self.spec.allowed_tools.iter().any(|t| t == &call.tool)
        {
            return Err(McpError::ToolNotAllowed(call.tool.clone()));
        }

        // Read-only guard: case-insensitive substring match against the
        // configured write-tool patterns. Defaults to the spec-mandated list.
        if self.spec.read_only && tool_matches_write_pattern(&call.tool, &self.write_tool_patterns)
        {
            return Err(McpError::ToolBlocked(call.tool.clone()));
        }

        let start = std::time::Instant::now();
        let params = json!({
            "name": call.tool,
            "arguments": call.arguments,
        });
        let send = self.send_request("tools/call", params);
        let value = match timeout(Duration::from_secs(MCP_CALL_TIMEOUT_SECS), send).await {
            Ok(Ok(value)) => value,
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err(McpError::Timeout(MCP_CALL_TIMEOUT_SECS as u32)),
        };
        let duration_ms = start.elapsed().as_millis() as u64;

        let is_error = value
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let content = value
            .get("content")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));

        Ok(McpToolResult {
            server: self.name.clone(),
            tool: call.tool,
            content,
            is_error,
            duration_ms,
        })
    }

    /// Gracefully shut the server down: close stdin (EOF), wait 2s for the
    /// child to exit on its own, then escalate to SIGTERM / kill.
    pub async fn shutdown(&self) -> Result<(), McpError> {
        // Drop the writer first so stdin observes EOF.
        *self.write_tx.lock().await = None;

        let mut guard = self.process.lock().await;
        let Some(mut child) = guard.take() else {
            return Ok(());
        };

        match timeout(Duration::from_secs(MCP_SHUTDOWN_GRACE_SECS), child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                // start_kill maps to SIGTERM on Unix; if the child still
                // refuses to exit, the next wait() will reap it after the OS
                // upgrades to SIGKILL via kill_on_drop's Drop impl.
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }

        // Drain any pending requests so future callers see NotStarted instead
        // of hanging on a oneshot that will never resolve.
        let mut pending = self.pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(RpcReply::Err(-32000, "server shutting down".to_string()));
        }
        Ok(())
    }

    /// The configured spec — useful for callers that need the role/read-only
    /// flag without going through the registry.
    pub fn spec(&self) -> &McpServerSpec {
        &self.spec
    }

    /// The server's logical name (matches `spec.name`).
    pub fn name(&self) -> &str {
        &self.name
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&request)?;
        if let Err(err) = self.write_line(line).await {
            // Roll back the pending entry so it doesn't leak on retry.
            self.pending.lock().await.remove(&id);
            return Err(err);
        }

        match rx.await {
            Ok(RpcReply::Ok(value)) => Ok(value),
            Ok(RpcReply::Err(code, message)) => Err(McpError::Rpc(code, message)),
            Err(_) => Err(McpError::Handshake(format!(
                "`{}`: response channel closed",
                self.name
            ))),
        }
    }

    async fn write_line(&self, line: String) -> Result<(), McpError> {
        let tx_guard = self.write_tx.lock().await;
        let tx = tx_guard
            .as_ref()
            .ok_or_else(|| McpError::NotStarted(self.name.clone()))?;
        tx.send(line)
            .await
            .map_err(|_| McpError::Handshake(format!("`{}`: writer task gone", self.name)))?;
        Ok(())
    }
}

async fn dispatch_incoming(
    server: &str,
    value: Value,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<RpcReply>>>>,
) {
    // Notifications carry no `id` — log at debug and move on.
    let Some(id_value) = value.get("id") else {
        debug!(server = %server, method = ?value.get("method"), "mcp notification");
        return;
    };
    let Some(id) = id_value.as_u64() else {
        debug!(server = %server, id = ?id_value, "mcp reply with non-numeric id");
        return;
    };
    let sender = pending.lock().await.remove(&id);
    let Some(sender) = sender else {
        debug!(server = %server, id = id, "mcp reply with no pending entry");
        return;
    };

    let reply = if let Some(error) = value.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000) as i32;
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("rpc error")
            .to_string();
        RpcReply::Err(code, message)
    } else {
        RpcReply::Ok(value.get("result").cloned().unwrap_or(Value::Null))
    };
    let _ = sender.send(reply);
}

fn parse_tools_list(server: &str, value: &Value) -> Result<Vec<McpToolInfo>, McpError> {
    let tools = value
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            McpError::Handshake(format!("`{server}`: tools/list missing `tools` array"))
        })?;
    let mut out = Vec::with_capacity(tools.len());
    for entry in tools {
        let name = entry.get("name").and_then(Value::as_str).ok_or_else(|| {
            McpError::Handshake(format!("`{server}`: tools/list entry missing `name`"))
        })?;
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let input_schema = entry.get("inputSchema").cloned();
        out.push(McpToolInfo {
            server: server.to_string(),
            name: name.to_string(),
            description,
            input_schema,
        });
    }
    Ok(out)
}

/// Case-insensitive substring match against the configured blacklist. The
/// substring (not exact) match is intentional — most MCP servers namespace
/// their write tools with a prefix (e.g. `fs.write_file`) and we want to catch
/// the whole family without enumerating every variant.
pub fn tool_matches_write_pattern(tool: &str, patterns: &[String]) -> bool {
    let lower = tool.to_ascii_lowercase();
    patterns
        .iter()
        .any(|pattern| !pattern.is_empty() && lower.contains(&pattern.to_ascii_lowercase()))
}

/// Returns true when an MCP server bound to `spec` should be visible to a
/// caller acting as `role`. `Both` is visible to either reviewer or lead.
pub fn role_matches(spec_role: &McpRole, requested: &McpRole) -> bool {
    matches!(
        (spec_role, requested),
        (McpRole::Both, _)
            | (_, McpRole::Both)
            | (McpRole::ReviewerOnly, McpRole::ReviewerOnly)
            | (McpRole::LeadOnly, McpRole::LeadOnly)
    )
}

/// Collection of started [`McpClient`]s keyed by server name. Constructed by
/// the orchestrator at session start and shared with the dispatch path via
/// `Arc<McpRegistry>`.
#[derive(Default)]
pub struct McpRegistry {
    clients: HashMap<String, Arc<McpClient>>,
}

impl McpRegistry {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Spawn every configured server, run the handshake, and register the
    /// resulting client. If any server fails to start, all previously-started
    /// servers are shut down before the error is returned — partial state is
    /// never observable outside this call.
    pub async fn register_all(
        &mut self,
        specs: &[McpServerSpec],
        write_patterns: &[String],
        allow_tools: &[String],
    ) -> Result<(), McpError> {
        let patterns = if write_patterns.is_empty() {
            default_write_tool_patterns()
        } else {
            write_patterns.to_vec()
        };
        for spec in specs {
            let mut spec = spec.clone();
            scope_mcp_spec_to_allowlist(&mut spec, allow_tools);
            let client = Arc::new(McpClient::new(spec.clone(), patterns.clone()));
            if let Err(err) = client.start().await {
                // Tear down everything we already started so we don't leak
                // a half-initialised registry.
                let _ = self.shutdown_all().await;
                return Err(err);
            }
            self.clients.insert(spec.name.clone(), client);
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<McpClient>> {
        self.clients.get(name).cloned()
    }

    /// Flatten every registered server's tools, filtered by role and by the
    /// per-server allowlist. The orchestrator surfaces this list to the
    /// reviewer/lead prompt so the model knows what it can call.
    pub async fn all_tools(&self, role: McpRole) -> Vec<McpToolInfo> {
        let mut out = Vec::new();
        for client in self.clients.values() {
            if !role_matches(&client.spec.role, &role) {
                continue;
            }
            let tools = match client.list_tools().await {
                Ok(tools) => tools,
                Err(_) => continue,
            };
            for tool in tools {
                // Intersect with the per-server allowlist when configured.
                if !client.spec.allowed_tools.is_empty()
                    && !client
                        .spec
                        .allowed_tools
                        .iter()
                        .any(|name| name == &tool.name)
                {
                    continue;
                }
                out.push(tool);
            }
        }
        out
    }

    /// Shut down every registered server. Failures are logged but do not
    /// abort the loop — callers want every child reaped on session end.
    pub async fn shutdown_all(&self) -> Result<(), McpError> {
        for client in self.clients.values() {
            if let Err(err) = client.shutdown().await {
                warn!(server = %client.name(), error = %err, "mcp shutdown error");
            }
        }
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<McpClient>)> {
        self.clients.iter()
    }
}

pub fn scope_mcp_spec_to_allowlist(spec: &mut McpServerSpec, allow_tools: &[String]) {
    if allow_tools.is_empty() {
        return;
    }
    if spec.allowed_tools.is_empty() {
        spec.allowed_tools = allow_tools.to_vec();
        return;
    }
    spec.allowed_tools
        .retain(|tool| allow_tools.iter().any(|allowed| allowed == tool));
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_types::McpTransport;

    fn spec(name: &str) -> McpServerSpec {
        McpServerSpec {
            name: name.to_string(),
            command: vec!["true".to_string()],
            env: Default::default(),
            transport: McpTransport::Stdio,
            allowed_tools: Vec::new(),
            role: McpRole::ReviewerOnly,
            read_only: true,
        }
    }

    #[test]
    fn write_pattern_match_is_case_insensitive_and_substring() {
        let patterns = default_write_tool_patterns();
        assert!(tool_matches_write_pattern("shell", &patterns));
        assert!(tool_matches_write_pattern("RUN_SHELL", &patterns));
        assert!(tool_matches_write_pattern("fs.write_file", &patterns));
        assert!(tool_matches_write_pattern("Execute_Command", &patterns));
        assert!(!tool_matches_write_pattern("read_file", &patterns));
        assert!(!tool_matches_write_pattern("fs.read", &patterns));
    }

    #[test]
    fn write_pattern_ignores_empty_patterns() {
        let patterns = vec!["".to_string(), "shell".to_string()];
        assert!(!tool_matches_write_pattern("read_file", &patterns));
        assert!(tool_matches_write_pattern("shell_exec", &patterns));
    }

    #[test]
    fn role_matches_handles_both_directions() {
        assert!(role_matches(&McpRole::Both, &McpRole::ReviewerOnly));
        assert!(role_matches(&McpRole::Both, &McpRole::LeadOnly));
        assert!(role_matches(&McpRole::ReviewerOnly, &McpRole::Both));
        assert!(role_matches(&McpRole::LeadOnly, &McpRole::Both));
        assert!(role_matches(&McpRole::ReviewerOnly, &McpRole::ReviewerOnly));
        assert!(role_matches(&McpRole::LeadOnly, &McpRole::LeadOnly));
        assert!(!role_matches(&McpRole::ReviewerOnly, &McpRole::LeadOnly));
        assert!(!role_matches(&McpRole::LeadOnly, &McpRole::ReviewerOnly));
    }

    #[test]
    fn parse_tools_list_extracts_name_description_and_schema() {
        let value = json!({
            "tools": [
                {
                    "name": "list_files",
                    "description": "List files",
                    "inputSchema": { "type": "object" }
                },
                { "name": "read_file" }
            ]
        });
        let tools = parse_tools_list("fixture", &value).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "list_files");
        assert_eq!(tools[0].description.as_deref(), Some("List files"));
        assert!(tools[0].input_schema.is_some());
        assert_eq!(tools[1].name, "read_file");
        assert!(tools[1].description.is_none());
        assert!(tools[1].input_schema.is_none());
        assert_eq!(tools[0].server, "fixture");
    }

    #[test]
    fn parse_tools_list_rejects_missing_array() {
        let value = json!({ "not_tools": [] });
        let err = parse_tools_list("fixture", &value).unwrap_err();
        assert!(matches!(err, McpError::Handshake(_)));
    }

    #[test]
    fn global_allowlist_intersects_server_allowlist() {
        let mut spec = spec("scoped");
        spec.allowed_tools = vec!["read_file".to_string(), "write_file".to_string()];

        scope_mcp_spec_to_allowlist(&mut spec, &["read_file".to_string(), "hover".to_string()]);

        assert_eq!(spec.allowed_tools, vec!["read_file".to_string()]);
    }

    #[test]
    fn global_allowlist_applies_when_server_allowlist_is_empty() {
        let mut spec = spec("global-only");

        scope_mcp_spec_to_allowlist(&mut spec, &["hover".to_string()]);

        assert_eq!(spec.allowed_tools, vec!["hover".to_string()]);
    }

    #[tokio::test]
    async fn call_tool_blocks_write_tool_when_read_only() {
        // Build a client without starting it so the guards still fire — we
        // simulate "started" by inserting a placeholder child via a helper.
        let mut spec = spec("guard");
        spec.read_only = true;
        let client = McpClient::new(spec, default_write_tool_patterns());
        // Force the "started" state so call_tool reaches the blacklist check.
        force_started(&client).await;

        let err = client
            .call_tool(McpToolCall {
                server: "guard".to_string(),
                tool: "shell".to_string(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ToolBlocked(t) if t == "shell"));
    }

    #[tokio::test]
    async fn call_tool_rejects_when_not_in_allowlist() {
        let mut spec = spec("allowlist");
        spec.allowed_tools = vec!["read_file".to_string()];
        let client = McpClient::new(spec, default_write_tool_patterns());
        force_started(&client).await;

        let err = client
            .call_tool(McpToolCall {
                server: "allowlist".to_string(),
                tool: "list_files".to_string(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ToolNotAllowed(t) if t == "list_files"));
    }

    #[tokio::test]
    async fn list_tools_errors_when_not_started() {
        let client = McpClient::new(spec("idle"), default_write_tool_patterns());
        let err = client.list_tools().await.unwrap_err();
        assert!(matches!(err, McpError::NotStarted(_)));
    }

    #[tokio::test]
    async fn call_tool_errors_when_not_started() {
        let client = McpClient::new(spec("idle"), default_write_tool_patterns());
        let err = client
            .call_tool(McpToolCall {
                server: "idle".to_string(),
                tool: "anything".to_string(),
                arguments: json!({}),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::NotStarted(_)));
    }

    #[tokio::test]
    async fn registry_register_then_get_round_trips_via_mock_server() {
        // Real subprocess test using a tiny `cat`-driven JSON-RPC echo
        // is brittle in CI, so we exercise the registry's bookkeeping via
        // direct insertion. The subprocess path is covered by the optional
        // ignored test below.
        let mut registry = McpRegistry::new();
        assert!(registry.get("absent").is_none());

        let client = Arc::new(McpClient::new(spec("mock"), default_write_tool_patterns()));
        registry.clients.insert("mock".to_string(), client);

        assert!(registry.get("mock").is_some());
        let tools = registry.all_tools(McpRole::ReviewerOnly).await;
        // The mock client has no started child and no cached tools — all_tools
        // simply skips it via the list_tools NotStarted branch.
        assert!(tools.is_empty());
    }

    /// Drive an end-to-end JSON-RPC handshake against a Python-based mock
    /// MCP server. Marked `#[ignore]` because it depends on `python3` being
    /// on PATH; run via `cargo test -p nerve-adapter -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn mock_server_handshake_lists_advertised_tools() {
        let script = r#"
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    if method == "initialize":
        reply = {"jsonrpc":"2.0","id":req["id"],"result":{"protocolVersion":"2024-11-05"}}
    elif method == "tools/list":
        reply = {"jsonrpc":"2.0","id":req["id"],"result":{"tools":[{"name":"ping","description":"pong"}]}}
    elif method == "tools/call":
        reply = {"jsonrpc":"2.0","id":req["id"],"result":{"content":[{"type":"text","text":"pong"}]}}
    else:
        if "id" in req:
            reply = {"jsonrpc":"2.0","id":req["id"],"result":{}}
        else:
            continue
    sys.stdout.write(json.dumps(reply) + "\n")
    sys.stdout.flush()
"#;
        let spec = McpServerSpec {
            name: "mock".to_string(),
            command: vec!["python3".to_string(), "-c".to_string(), script.to_string()],
            env: Default::default(),
            transport: McpTransport::Stdio,
            allowed_tools: Vec::new(),
            role: McpRole::Both,
            read_only: true,
        };
        let client = McpClient::new(spec, default_write_tool_patterns());
        client.start().await.expect("handshake ok");
        let tools = client.list_tools().await.expect("list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ping");
        let result = client
            .call_tool(McpToolCall {
                server: "mock".to_string(),
                tool: "ping".to_string(),
                arguments: json!({}),
            })
            .await
            .expect("tool call");
        assert!(!result.is_error);
        client.shutdown().await.expect("shutdown clean");
    }

    /// Pretend a client is "started" by inserting a dummy child marker so the
    /// security guards in `call_tool` execute. We never actually speak to the
    /// child — the guards return before any wire IO happens.
    async fn force_started(client: &McpClient) {
        // Spawn a process that immediately exits; we only need a `Child`
        // handle so the `process.lock().await.is_none()` check passes.
        let child = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn placeholder child");
        *client.process.lock().await = Some(child);
    }
}
