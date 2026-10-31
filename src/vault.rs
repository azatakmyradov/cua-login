//! Bitwarden access through the `bw` CLI.
//!
//! The session key lives in the login Keychain under an item this binary
//! creates, so other programs (including `security find-generic-password`)
//! get a Keychain access prompt instead of the key.

use anyhow::{bail, Context, Result};
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use serde::Deserialize;
use serde_json::Value;
use std::process::{Command, Stdio};

const KEYCHAIN_SERVICE: &str = "cua-login";
const KEYCHAIN_ACCOUNT: &str = "bw-session";
const PASSWORD_ENV: &str = "CUA_LOGIN_BW_PASSWORD";

pub const LOCKED: &str =
    "vault is locked: ask the user to run `cua-login unlock` in their own terminal";

pub struct Login {
    pub id: String,
    pub name: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub has_totp: bool,
    pub uris: Vec<String>,
}

#[derive(Deserialize)]
struct Item {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: u8,
    login: Option<ItemLogin>,
}

#[derive(Deserialize)]
struct ItemLogin {
    username: Option<String>,
    password: Option<String>,
    totp: Option<String>,
    #[serde(default)]
    uris: Option<Vec<ItemUri>>,
}

#[derive(Deserialize)]
struct ItemUri {
    uri: Option<String>,
}

const LOGIN_TYPE: u8 = 1;

impl Item {
    fn into_login(self) -> Login {
        let login = self.login;
        let uris = login
            .as_ref()
            .and_then(|l| l.uris.as_ref())
            .map(|u| u.iter().filter_map(|u| u.uri.clone()).collect())
            .unwrap_or_default();
        Login {
            id: self.id,
            name: self.name,
            username: login.as_ref().and_then(|l| l.username.clone()),
            password: login.as_ref().and_then(|l| l.password.clone()),
            has_totp: login.as_ref().is_some_and(|l| l.totp.is_some()),
            uris,
        }
    }
}

fn session_key() -> Result<String> {
    let key = get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).context(LOCKED)?;
    Ok(String::from_utf8(key)?)
}

/// Runs `bw` with the stored session and returns stdout. Error messages carry
/// only bw's first stderr line, never stdout, which may hold secrets.
fn bw(args: &[&str]) -> Result<String> {
    let out = Command::new("bw")
        .args(args)
        .arg("--nointeraction")
        .env("BW_SESSION", session_key()?)
        .stdin(Stdio::null())
        .output()
        .context("could not run `bw`; is the Bitwarden CLI installed?")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().next().unwrap_or("unknown error").trim();
        if first.contains("locked") || first.contains("not logged in") {
            bail!(LOCKED);
        }
        bail!("bw {}: {first}", args[0]);
    }
    Ok(String::from_utf8(out.stdout)?)
}

pub fn unlock() -> Result<()> {
    let password = rpassword::prompt_password("Bitwarden master password: ")?;
    let out = Command::new("bw")
        .args(["unlock", "--raw", "--passwordenv", PASSWORD_ENV])
        .env(PASSWORD_ENV, password)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("could not run `bw`; is the Bitwarden CLI installed?")?;
    let key = String::from_utf8(out.stdout)?.trim().to_string();
    if !out.status.success() || key.is_empty() {
        bail!("unlock failed");
    }
    set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, key.as_bytes())?;
    bw(&["sync"])?;
    Ok(())
}

pub fn lock() -> Result<()> {
    let _ = delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
    let _ = Command::new("bw").arg("lock").stdout(Stdio::null()).status();
    Ok(())
}

pub fn status() -> Result<String> {
    let out = Command::new("bw").arg("status").output()?;
    let status: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let bw = status["status"].as_str().unwrap_or("unknown").to_string();
    let stored = get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).is_ok();
    Ok(match (bw.as_str(), stored) {
        ("unauthenticated", _) => "logged out: run `bw login`".into(),
        (_, false) => "locked".into(),
        _ => "unlocked".into(),
    })
}

fn items(search: &str) -> Result<Vec<Item>> {
    let json = bw(&["list", "items", "--search", search])?;
    let items: Vec<Item> = serde_json::from_str(&json)?;
    Ok(items.into_iter().filter(|i| i.kind == LOGIN_TYPE).collect())
}

/// Login items matching `search`, without usernames or passwords.
pub fn list(search: &str) -> Result<Vec<Login>> {
    Ok(items(search)?
        .into_iter()
        .map(|i| {
            let mut l = i.into_login();
            l.username = None;
            l.password = None;
            l
        })
        .collect())
}

/// One login by exact id or exact (case-insensitive) name.
pub fn get(item: &str) -> Result<Login> {
    let mut found: Vec<Item> = items(item)?
        .into_iter()
        .filter(|i| i.id == item || i.name.eq_ignore_ascii_case(item))
        .collect();
    match found.len() {
        0 => bail!("no login named or with id {item:?}; use list_logins to find it"),
        1 => Ok(found.remove(0).into_login()),
        n => {
            let ids: Vec<_> = found.iter().map(|i| i.id.as_str()).collect();
            bail!("{n} logins are named {item:?}; pass one id instead: {}", ids.join(", "))
        }
    }
}

pub fn totp(id: &str) -> Result<String> {
    Ok(bw(&["get", "totp", id])?.trim().to_string())
}
