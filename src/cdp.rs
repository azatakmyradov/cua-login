//! Direct Chrome DevTools access for browsers cua-driver launched with an
//! isolated profile. Those belong to the cua-driver session that launched
//! them, so cua-login's own session cannot bind them; their DevTools port is
//! in the profile folder instead.

use crate::driver::Driver;
use crate::fill::{Input, Page};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

const PROFILES: &str = "Library/Application Support/CuaDriver/BrowserProfiles/";

pub struct CdpPage {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

/// The DevTools port of a Chrome process, if it runs a cua-driver isolated profile.
pub fn driver_owned_port(pid: i64) -> Option<u16> {
    let out = Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]).output().ok()?;
    let args = String::from_utf8(out.stdout).ok()?;
    let dir = args.split("--user-data-dir=").nth(1)?;
    let dir = dir.split(" --").next()?.trim();
    let home = std::env::var("HOME").ok()?;
    if !dir.starts_with(&format!("{home}/{PROFILES}")) || dir.contains("..") {
        return None;
    }
    let port = std::fs::read_to_string(format!("{dir}/DevToolsActivePort")).ok()?;
    port.lines().next()?.trim().parse().ok()
}

pub fn connect_driver_owned(driver: &mut Driver, pid: i64, window_id: i64) -> Result<Option<CdpPage>> {
    let Some(port) = driver_owned_port(pid) else {
        return Ok(None);
    };
    let window = driver.window(pid, window_id)?;
    let title = window["title"].as_str().context("no such window for that pid")?;
    CdpPage::connect(port, title).map(Some)
}

impl CdpPage {
    /// Connects to the tab showing in the window titled `window_title`.
    pub fn connect(port: u16, window_title: &str) -> Result<Self> {
        let pages: Vec<Value> = serde_json::from_str(&http_get(port, "/json/list")?)?;
        let pages: Vec<&Value> = pages.iter().filter(|p| p["type"] == "page").collect();
        let mut visible = Vec::new();
        for page in pages {
            let url = page["webSocketDebuggerUrl"].as_str().context("tab has no DevTools URL")?;
            if std::env::var_os("CUA_LOGIN_DEBUG").is_some() {
                eprintln!("cua-login: cdp open {url}");
            }
            let mut tab = CdpPage::open(url)?;
            let state = tab.eval("document.visibilityState")?;
            if state == "visible" {
                visible.push((page["title"].as_str().unwrap_or_default().to_string(), tab));
            }
        }
        if visible.len() > 1 {
            visible.retain(|(title, _)| window_title.starts_with(title.as_str()));
        }
        match visible.len() {
            1 => Ok(visible.pop().unwrap().1),
            0 => bail!("no visible tab found in that Chrome window"),
            _ => bail!("several visible tabs match that window; close the others"),
        }
    }

    fn open(url: &str) -> Result<Self> {
        let addr = url
            .strip_prefix("ws://")
            .and_then(|rest| rest.split('/').next())
            .context("unexpected DevTools URL")?;
        let stream = TcpStream::connect(addr).with_context(|| format!("could not reach Chrome DevTools at {addr}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let (socket, _) = tungstenite::client(url, MaybeTlsStream::Plain(stream))
            .map_err(|e| anyhow::anyhow!("DevTools handshake failed: {e}"))?;
        Ok(CdpPage { socket, next_id: 1 })
    }

    fn send(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({ "id": id, "method": method, "params": params });
        if std::env::var_os("CUA_LOGIN_DEBUG").is_some() {
            eprintln!("cua-login: cdp {method}");
        }
        self.socket.send(Message::text(msg.to_string()))?;
        loop {
            let Message::Text(text) = self.socket.read()? else { continue };
            let msg: Value = serde_json::from_str(&text)?;
            if msg["id"] != json!(id) {
                continue; // events
            }
            if let Some(err) = msg.get("error") {
                bail!("Chrome {method}: {}", err["message"]);
            }
            return Ok(msg["result"].clone());
        }
    }

    fn eval(&mut self, expression: &str) -> Result<Value> {
        let r = self.send("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true }))?;
        if r.get("exceptionDetails").is_some() {
            bail!("the page script failed");
        }
        Ok(r["result"]["value"].clone())
    }
}

/// Visible inputs of the top document, as `index type name id autocomplete`.
/// While a modal is open, the page behind it is inert, so only its inputs count.
const LIST_INPUTS: &str = r#"(() => {
  const visible = el => el.getClientRects().length > 0 && getComputedStyle(el).visibility !== 'hidden';
  const modal = document.querySelector('dialog:modal')
    || [...document.querySelectorAll('[aria-modal="true"]')].find(visible);
  return [...document.querySelectorAll('input')].map((el, i) => ({
    i, type: el.type, name: el.name, id: el.id, autocomplete: el.autocomplete,
    usable: visible(el) && (!modal || modal.contains(el)),
  })).filter(x => x.usable);
})()"#;

impl Page for CdpPage {
    /// The URL Chrome loaded in the top frame; page script and the address bar can't change it.
    fn url(&mut self) -> Result<String> {
        let tree = self.send("Page.getFrameTree", json!({}))?;
        Ok(tree["frameTree"]["frame"]["url"].as_str().unwrap_or_default().to_string())
    }

    fn inputs(&mut self) -> Result<Vec<Input>> {
        let list = self.eval(LIST_INPUTS)?;
        Ok(list
            .as_array()
            .context("could not list the page's inputs")?
            .iter()
            .enumerate()
            .map(|(order, x)| {
                let attrs = ["type", "name", "id", "autocomplete"]
                    .iter()
                    .filter_map(|k| Some((k.to_string(), x[*k].as_str()?.to_string())))
                    .filter(|(_, v)| !v.is_empty())
                    .collect();
                Input { index: order, reference: x["i"].to_string(), attrs }
            })
            .collect())
    }

    fn type_text(&mut self, reference: &str, text: &str) -> Result<()> {
        let index: usize = reference.parse()?;
        let focused = self.eval(&format!(
            "(() => {{ const el = document.querySelectorAll('input')[{index}]; \
             if (!el) return false; el.focus(); el.select(); return document.activeElement === el; }})()"
        ))?;
        if focused != true {
            bail!("the field went away before it could be filled");
        }
        // The value goes through the input pipeline, never into page script source.
        self.send("Input.insertText", json!({ "text": text }))?;
        Ok(())
    }
}

fn http_get(port: u16, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).context("Chrome DevTools is not reachable")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")?;
    // Chrome may keep the connection open, so read exactly Content-Length bytes.
    let mut response = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&response);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
                .and_then(|v| v.parse::<usize>().ok());
            if length.is_some_and(|len| body.len() >= len) {
                break;
            }
        }
    }
    let text = String::from_utf8(response)?;
    let (_, body) = text.split_once("\r\n\r\n").context("bad DevTools response")?;
    Ok(body.to_string())
}
