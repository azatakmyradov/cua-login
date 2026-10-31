use crate::driver::Driver;
use crate::fill::{self, Field};
use crate::vault::Login;
use crate::browser::{self, Bound};
use crate::cdp;
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::json;
use std::time::Duration;

pub fn run() -> Result<()> {
    let public_demo_login = Login {
        id: "selftest".into(),
        name: "selftest".into(),
        username: Some("tomsmith".into()),
        password: Some("SuperSecretPassword!".into()),
        has_totp: false,
        uris: vec!["https://the-internet.herokuapp.com".into()],
    };
    let mut driver = Driver::start()?;
    let prepared = driver.call(
        "browser_prepare",
        json!({ "allow_launch": true, "profile": { "mode": "isolated_new" } }),
    )?;
    let pid = prepared["prepared_pid"].as_i64().context("Chrome did not launch")?;

    let window_id = retry(20, || {
        let windows = driver.call("list_windows", json!({ "pid": pid }))?;
        windows["windows"]
            .as_array()
            .and_then(|w| w.iter().max_by_key(|w| w["bounds"]["height"].as_f64().unwrap_or(0.0) as i64))
            .and_then(|w| w["window_id"].as_i64())
            .context("Chrome opened no window")
    })?;
    let tab = retry(21, || {
        match browser::bind(&mut driver, pid, window_id)? {
            Bound::Tab(tab) => Ok(tab),
            Bound::Refused(code) => Err(anyhow!("could not bind the test Chrome: {code}")),
        }
    })?;
    let tab_args = tab.args();

    let mut nav = tab_args.clone();
    nav["url"] = json!("https://example.com/");
    driver.call("browser_navigate", nav.clone())?;
    let wrong = fill::fill(&mut browser::DriverPage::new(&mut driver, tab.clone()), &public_demo_login, &[], &mut |_| {
        bail!("must not read a secret")
    });
    match wrong {
        Err(e) if e.to_string().contains("refusing") => println!("ok: refused example.com"),
        other => bail!("expected a refusal on example.com, got {:?}", other.map(|f| f.host)),
    }

    nav["url"] = json!("https://the-internet.herokuapp.com/login");
    driver.call("browser_navigate", nav)?;
    retry(40, || {
        let snapshot = driver.call("get_browser_state", tab_args.clone())?;
        let ready = snapshot["refs"].as_array().is_some_and(|r| r.iter().any(|r| r["node"] == "input"));
        ensure!(ready, "the login form did not load");
        Ok(())
    })?;
    let mut second_session = Driver::start()?;
    if let Bound::Tab(_) = browser::bind(&mut second_session, pid, window_id)? {
        bail!("expected the second session to be refused by cua-driver");
    }
    let mut page = cdp::connect_driver_owned(&mut second_session, pid, window_id)?.context("the test Chrome has no DevTools port")?;
    let filled = fill::fill(&mut page, &public_demo_login, &[], &mut |field| {
        Ok(match field {
            Field::Username => public_demo_login.username.clone().unwrap(),
            _ => public_demo_login.password.clone().unwrap(),
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
    retry(20, || {
        let url = driver.call("get_browser_state", tab_args.clone())?["url"].clone();
        ensure!(url.as_str().is_some_and(|u| u.ends_with("/secure")), "the test login did not sign in");
        Ok(())
    })?;
    println!("ok: signed in");
    Ok(())
}

const RETRY_INTERVAL: Duration = Duration::from_millis(250);

fn retry<T>(tries: u32, mut attempt: impl FnMut() -> Result<T>) -> Result<T> {
    let mut result = attempt();
    for _ in 1..tries {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(RETRY_INTERVAL);
        result = attempt();
    }
    result
}
