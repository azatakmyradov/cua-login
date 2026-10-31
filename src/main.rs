mod ax;
mod cdp;
mod driver;
mod fill;
mod mcp;
mod vault;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

/// Fill Bitwarden logins into Chrome through cua-driver, without showing
/// them to the agent that asked.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Unlock the vault for agents (asks for the master password).
    Unlock,
    /// Lock the vault and forget the stored session.
    Lock,
    /// Show whether the vault is unlocked for agents.
    Status,
    /// Run the MCP server agents use (stdio).
    Mcp,
    /// Fill a test login on the-internet.herokuapp.com in a throwaway Chrome.
    Selftest,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("cua-login: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Unlock => {
            vault::unlock()?;
            println!("unlocked; run `cua-login lock` when done");
        }
        Command::Lock => {
            vault::lock()?;
            println!("locked");
        }
        Command::Status => println!("{}", vault::status()?),
        Command::Mcp => mcp::serve()?,
        Command::Selftest => selftest()?,
    }
    Ok(())
}

/// Exercises everything except Bitwarden: an isolated Chrome, the site check,
/// field detection and typing, then submits and checks the login worked.
fn selftest() -> Result<()> {
    // Public test credentials published on the page itself.
    let login = vault::Login {
        id: "selftest".into(),
        name: "selftest".into(),
        username: Some("tomsmith".into()),
        password: Some("SuperSecretPassword!".into()),
        has_totp: false,
        uris: vec!["https://the-internet.herokuapp.com".into()],
    };
    let mut driver = driver::Driver::start()?;
    let prepared = driver.call(
        "browser_prepare",
        json!({ "allow_launch": true, "profile": { "mode": "isolated_new" } }),
    )?;
    let pid = prepared["prepared_pid"].as_i64().context("Chrome did not launch")?;

    let mut window_id = None;
    for _ in 0..20 {
        let windows = driver.call("list_windows", json!({ "pid": pid }))?;
        window_id = windows["windows"]
            .as_array()
            .and_then(|w| w.iter().max_by_key(|w| w["bounds"]["height"].as_f64().unwrap_or(0.0) as i64))
            .and_then(|w| w["window_id"].as_i64());
        if window_id.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let window_id = window_id.context("Chrome opened no window")?;
    let mut tab = fill::bind(&mut driver, pid, window_id);
    for _ in 0..20 {
        if tab.as_ref().is_ok_and(|t| t.is_ok()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        tab = fill::bind(&mut driver, pid, window_id);
    }
    let tab = tab?.map_err(|code| anyhow::anyhow!("could not bind the test Chrome: {code}"))?;
    let tab_args = json!({ "target_id": tab.target_id, "tab_id": tab.tab_id });

    // A site that isn't saved on the item must be refused.
    let mut nav = tab_args.clone();
    nav["url"] = json!("https://example.com/");
    driver.call("browser_navigate", nav.clone())?;
    let wrong = fill::fill(&mut fill::DriverPage::new(&mut driver, tab.clone()), &login, &[], &mut |_| bail!("must not read a secret"));
    match wrong {
        Err(e) if e.to_string().contains("refusing") => println!("ok: refused example.com"),
        other => bail!("expected a refusal on example.com, got {:?}", other.map(|f| f.host)),
    }

    nav["url"] = json!("https://the-internet.herokuapp.com/login");
    driver.call("browser_navigate", nav)?;
    for _ in 0..40 {
        let snapshot = driver.call("get_browser_state", tab_args.clone())?;
        if snapshot["refs"].as_array().is_some_and(|r| r.iter().any(|r| r["node"] == "input")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // Fill from a second session, as the MCP server does: it can't bind a
    // browser another session launched, so this goes through DevTools.
    let mut other = driver::Driver::start()?;
    if fill::bind(&mut other, pid, window_id)?.is_ok() {
        bail!("expected the second session to be refused by cua-driver");
    }
    let mut page = mcp::open_isolated(&mut other, pid, window_id)?.context("the test Chrome has no DevTools port")?;
    let filled = fill::fill(&mut page, &login, &[], &mut |field| {
        Ok(match field {
            fill::Field::Username => login.username.clone().unwrap(),
            _ => login.password.clone().unwrap(),
        })
    })?;
    println!("ok: filled {:?} on {} from another session", filled.fields, filled.host);

    let snapshot = driver.call("get_browser_state", tab_args.clone())?;
    let submit = snapshot["refs"]
        .as_array()
        .and_then(|r| r.iter().find(|r| r["node"] == "button"))
        .and_then(|r| r["ref"].as_str())
        .context("no submit button")?
        .to_string();
    let mut click = tab_args.clone();
    click["ref"] = json!(submit);
    driver.call("browser_click", click)?;
    for _ in 0..20 {
        let url = driver.call("get_browser_state", tab_args.clone())?["url"].clone();
        if url.as_str().is_some_and(|u| u.ends_with("/secure")) {
            println!("ok: signed in");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    bail!("the test login did not sign in")
}
