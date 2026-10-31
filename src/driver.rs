//! Minimal MCP stdio client for `cua-driver mcp`.
//!
//! One child process is one transport, so browser target/tab ids and refs
//! minted by an earlier call stay valid for later calls on the same Driver.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static SESSIONS: AtomicU32 = AtomicU32::new(0);

pub struct Driver {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    session: String,
}

impl Driver {
    pub fn start() -> Result<Self> {
        let mut child = Command::new("cua-driver")
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if std::env::var_os("CUA_LOGIN_DEBUG").is_some() { Stdio::inherit() } else { Stdio::null() })
            .spawn()
            .context("could not start `cua-driver mcp`; is cua-driver installed?")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut driver = Driver {
            child,
            stdin,
            stdout,
            next_id: 1,
            session: format!("cua-login-{}-{}", std::process::id(), SESSIONS.fetch_add(1, Ordering::Relaxed)),
        };
        driver.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "cua-login", "version": env!("CARGO_PKG_VERSION") }
            }),
        )?;
        driver.notify("notifications/initialized")?;
        driver.call("start_session", json!({}))?;
        Ok(driver)
    }

    /// Calls a tool with this Driver's session and returns its structured result.
    /// A tool-level refusal is returned as Ok so callers can branch on its code.
    pub fn call(&mut self, tool: &str, mut args: Value) -> Result<Value> {
        args["session"] = json!(self.session);
        if std::env::var_os("CUA_LOGIN_DEBUG").is_some() {
            eprintln!("cua-login: {tool} [{}]", self.session);
        }
        let result = self.request("tools/call", json!({ "name": tool, "arguments": args }))?;
        if let Some(structured) = result.get("structuredContent") {
            return Ok(structured.clone());
        }
        // Errors and a few tools only return text content.
        let text = result["content"]
            .as_array()
            .and_then(|c| c.iter().find_map(|b| b["text"].as_str()))
            .unwrap_or_default();
        if result["isError"].as_bool() == Some(true) {
            bail!("cua-driver {tool} failed: {text}");
        }
        Ok(serde_json::from_str(text).unwrap_or_else(|_| json!({ "text": text })))
    }

    pub fn window(&mut self, pid: i64, window_id: i64) -> Result<Value> {
        let windows = self.call("list_windows", json!({ "pid": pid }))?;
        windows["windows"]
            .as_array()
            .and_then(|w| w.iter().find(|w| w["window_id"] == window_id))
            .cloned()
            .context("no such window for that pid")
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method }))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line)? == 0 {
                bail!("cua-driver closed the connection");
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg["id"] != json!(id) {
                continue; // notifications and server requests
            }
            if let Some(err) = msg.get("error") {
                return Err(anyhow!("cua-driver {method}: {}", err["message"]));
            }
            return Ok(msg["result"].clone());
        }
    }

    fn send(&mut self, msg: &Value) -> Result<()> {
        writeln!(self.stdin, "{msg}")?;
        self.stdin.flush()?;
        Ok(())
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let _ = self.call("end_session", json!({}));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Returns the refusal code of a tool result, if it was refused.
pub fn refusal(v: &Value) -> Option<&str> {
    if v["status"] == "refused" {
        Some(v["refusal"]["code"].as_str().unwrap_or("refused"))
    } else {
        None
    }
}
