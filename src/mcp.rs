//! Stdio MCP server exposing `list_logins` and `fill_login` to agents.
//! Results never contain usernames, passwords or codes.

use crate::ax::AxPage;
use crate::browser::{self, Bound};
use crate::cdp;
use crate::driver::Driver;
use crate::fill::{self, Field, Page};
use crate::vault;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(msg) = serde_json::from_str::<Value>(&line?) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications
        let result = match msg["method"].as_str().unwrap_or_default() {
            "initialize" => json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "cua-login", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }),
            "tools/list" => json!({ "tools": tools() }),
            "tools/call" => {
                let params = &msg["params"];
                match call(params["name"].as_str().unwrap_or_default(), &params["arguments"]) {
                    Ok(v) => json!({ "content": [{ "type": "text", "text": v.to_string() }] }),
                    Err(e) => json!({
                        "content": [{ "type": "text", "text": format!("{e:#}") }],
                        "isError": true,
                    }),
                }
            }
            "ping" => json!({}),
            method => {
                let err = json!({ "code": -32601, "message": format!("unknown method {method}") });
                writeln!(stdout, "{}", json!({ "jsonrpc": "2.0", "id": id, "error": err }))?;
                stdout.flush()?;
                continue;
            }
        };
        writeln!(stdout, "{}", json!({ "jsonrpc": "2.0", "id": id, "result": result }))?;
        stdout.flush()?;
    }
    Ok(())
}

const INSTRUCTIONS: &str = "Fill Bitwarden logins into Chrome without seeing them. \
Find the item with list_logins, then call fill_login with the Chrome window's pid and \
window_id from cua-driver. It fills the active tab only if the tab's site matches a \
URL saved on the item. Submit the form yourself afterwards. On a code page after \
signing in, call fill_login again with fields [\"totp\"].";

fn tools() -> Value {
    json!([
        {
            "name": "list_logins",
            "description": "List Bitwarden login items: id, name, saved websites and whether a TOTP code is set up. Never returns usernames or passwords.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "search": { "type": "string", "description": "Text to match against item names and websites. Omit to list all." }
                }
            }
        },
        {
            "name": "fill_login",
            "description": "Type a Bitwarden login into the active tab of a Chrome window that cua-driver is attached to. Refuses unless the tab's site matches a website saved on the item. Returns which fields were filled, never their values.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item": { "type": "string", "description": "Item id, or its exact name, from list_logins." },
                    "pid": { "type": "integer", "description": "Chrome process id." },
                    "window_id": { "type": "integer", "description": "Chrome window id from cua-driver list_windows." },
                    "fields": {
                        "type": "array",
                        "items": { "enum": ["username", "password", "totp"] },
                        "description": "Fields to fill. Omit to fill whichever of username and password the page shows."
                    }
                },
                "required": ["item", "pid", "window_id"]
            }
        }
    ])
}

fn call(name: &str, args: &Value) -> Result<Value> {
    match name {
        "list_logins" => {
            let logins = vault::list(args["search"].as_str().unwrap_or_default())?;
            Ok(json!(logins
                .iter()
                .map(|l| json!({ "id": l.id, "name": l.name, "websites": l.uris, "totp": l.has_totp }))
                .collect::<Vec<_>>()))
        }
        "fill_login" => {
            let item = args["item"].as_str().context("item is required")?;
            let pid = args["pid"].as_i64().context("pid is required")?;
            let window_id = args["window_id"].as_i64().context("window_id is required")?;
            let fields: Vec<Field> = args["fields"]
                .as_array()
                .map(|f| f.iter().map(|v| Field::parse(v.as_str().unwrap_or_default())).collect())
                .transpose()?
                .unwrap_or_default();
            fill_login(item, pid, window_id, &fields)
        }
        _ => anyhow::bail!("unknown tool {name}"),
    }
}

fn fill_login(item: &str, pid: i64, window_id: i64, fields: &[Field]) -> Result<Value> {
    let login = vault::get(item)?;
    let mut driver = Driver::start()?;
    fill::check_not_recording(&mut driver)?;
    let mut secret = |field: Field| match field {
        Field::Username => login.username.clone().context("the item has no username"),
        Field::Password => login.password.clone().context("the item has no password"),
        Field::Totp => vault::totp(&login.id),
    };
    let mut page: Box<dyn Page + '_> = match browser::bind(&mut driver, pid, window_id)? {
        Bound::Tab(tab) => Box::new(browser::DriverPage::new(&mut driver, tab)),
        Bound::Refused(code) => match cdp::connect_driver_owned(&mut driver, pid, window_id)? {
            Some(page) => Box::new(page),
            // Not attached and not ours: go through the accessibility tree.
            None => Box::new(
                AxPage::new(&mut driver, pid, window_id)
                    .with_context(|| format!("cua-driver refused this window ({code})"))?,
            ),
        },
    };
    let filled = fill::fill(page.as_mut(), &login, fields, &mut secret)?;
    Ok(json!({
        "ok": true,
        "item": login.name,
        "site": filled.host,
        "filled": filled.fields.iter().map(|f| f.name()).collect::<Vec<_>>(),
    }))
}
