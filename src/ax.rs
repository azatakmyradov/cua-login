//! Filling through the macOS accessibility tree, for a Chrome window that
//! cua-driver has no DevTools route to. The page's URL comes from Chrome's
//! own AppleScript, not the omnibox text, which can be edited without
//! navigating.

use crate::driver::{refusal, Driver};
use crate::fill::{Input, Page};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Command;

/// Browsers whose AppleScript dictionary has `active tab` and `URL`.
const BROWSERS: &[&str] = &["Google Chrome", "Google Chrome Canary", "Brave Browser", "Microsoft Edge", "Chromium"];

pub struct AxPage<'a> {
    driver: &'a mut Driver,
    pid: i64,
    window_id: i64,
    app: &'static str,
    bounds: [i64; 4],
    title: String,
    url: Option<String>,
    snapshot_id: String,
    /// Role and label per element index, to notice the page changing under us.
    rows: HashMap<String, (String, String)>,
    passwords: Vec<String>,
}

impl<'a> AxPage<'a> {
    pub fn new(driver: &'a mut Driver, pid: i64, window_id: i64) -> Result<Self> {
        let windows = driver.call("list_windows", json!({ "pid": pid }))?;
        let window = windows["windows"]
            .as_array()
            .and_then(|w| w.iter().find(|w| w["window_id"] == window_id))
            .context("no such window for that pid")?;
        let app = window["app_name"].as_str().unwrap_or_default();
        let app = *BROWSERS
            .iter()
            .find(|b| **b == app)
            .with_context(|| format!("{app:?} is not a supported Chromium browser"))?;
        let b = &window["bounds"];
        let n = |k: &str| b[k].as_f64().unwrap_or(f64::NAN).round() as i64;
        let (x, y) = (n("x"), n("y"));
        Ok(AxPage {
            driver,
            pid,
            window_id,
            app,
            bounds: [x, y, x + n("width"), y + n("height")],
            title: window["title"].as_str().unwrap_or_default().to_string(),
            url: None,
            snapshot_id: String::new(),
            rows: HashMap::new(),
            passwords: Vec::new(),
        })
    }

    /// The active tab's URL of the one window matching this window's bounds and title.
    fn current_url(&self) -> Result<String> {
        let script = format!(
            r#"set sep to character id 9
tell application "{}"
  set out to ""
  repeat with w in windows
    set b to bounds of w
    set out to out & (item 1 of b) & sep & (item 2 of b) & sep & (item 3 of b) & sep & (item 4 of b) & sep & (title of active tab of w) & sep & (URL of active tab of w) & linefeed
  end repeat
  return out
end tell"#,
            self.app
        );
        let out = Command::new("osascript").args(["-e", &script]).output().context("could not run osascript")?;
        if !out.status.success() {
            bail!(
                "could not read the tab's URL from {} over AppleScript: {}",
                self.app,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let out = String::from_utf8_lossy(&out.stdout);
        let matches: Vec<&str> = out
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(5, '\t');
                let bounds: Vec<i64> = parts.by_ref().take(4).filter_map(|p| p.trim().parse().ok()).collect();
                let (title, url) = parts.next()?.rsplit_once('\t')?;
                (bounds == self.bounds && self.title.starts_with(title)).then_some(url)
            })
            .collect();
        match matches.as_slice() {
            [url] => Ok(url.to_string()),
            [] => bail!("could not match that window in {}'s AppleScript windows", self.app),
            _ => bail!("several {} windows share that window's position and title", self.app),
        }
    }

    fn snapshot(&mut self) -> Result<Vec<Value>> {
        let state = self.driver.call(
            "get_window_state",
            json!({
                "pid": self.pid,
                "window_id": self.window_id,
                "tree_format": "elements",
                "include_screenshot": false,
                "max_elements": 3000,
                "timeout_ms": 5000,
            }),
        )?;
        if let Some(code) = refusal(&state) {
            bail!("cannot read the window: {code}");
        }
        self.snapshot_id = state["snapshot_id"].as_str().context("no snapshot id")?.to_string();
        Ok(state["elements"].as_array().cloned().unwrap_or_default())
    }
}

impl Page for AxPage<'_> {
    fn url(&mut self) -> Result<String> {
        let url = self.current_url()?;
        self.url = Some(url.clone());
        Ok(url)
    }

    fn inputs(&mut self) -> Result<Vec<Input>> {
        let elements = self.snapshot()?;
        let inputs = inputs(&elements);
        self.rows = elements
            .iter()
            .map(|e| (index(e), (str(e, "role"), str(e, "label"))))
            .collect();
        self.passwords = inputs
            .iter()
            .filter(|i| i.attrs.get("type").is_some_and(|t| t == "password"))
            .map(|i| i.reference.clone())
            .collect();
        Ok(inputs)
    }

    fn type_text(&mut self, reference: &str, text: &str) -> Result<()> {
        // The tab must still be on the URL the site check passed.
        if self.url.as_deref() != Some(self.current_url()?.as_str()) {
            bail!("the tab navigated while filling; nothing more was typed");
        }
        let token = format!("{}:{reference}", self.snapshot_id);
        let result = self.driver.call("set_value", json!({ "element_token": token, "value": text }))?;
        if let Some(code) = refusal(&result) {
            bail!("typing failed: {code}");
        }
        if !self.passwords.iter().any(|p| p == reference) {
            return Ok(());
        }
        // A field that only looked like a password field would show the
        // value to anyone reading the tree; clear it if so.
        let rows = self.rows.clone();
        let elements = self.snapshot()?;
        let field = elements.iter().find(|e| index(e) == reference);
        if field.map(|e| (str(e, "role"), str(e, "label"))) != rows.get(reference).cloned() {
            bail!("the page changed while filling");
        }
        if field.is_some_and(|e| str(e, "value") == text) {
            let token = format!("{}:{reference}", self.snapshot_id);
            self.driver.call("set_value", json!({ "element_token": token, "value": "" }))?;
            bail!("the password field is not masked, so it was cleared again");
        }
        Ok(())
    }
}

fn str(e: &Value, key: &str) -> String {
    e[key].as_str().unwrap_or_default().to_string()
}

fn index(e: &Value) -> String {
    e["element_index"].to_string()
}

/// Text inputs in the tab's top web area; ones inside an iframe's own web
/// area are skipped. Labels stand in for the name attribute.
fn inputs(elements: &[Value]) -> Vec<Input> {
    let by_index: HashMap<String, &Value> = elements.iter().map(|e| (index(e), e)).collect();
    let web_areas = |e: &Value| {
        let mut count = 0;
        let mut node = Some(e);
        while let Some(n) = node {
            if n["role"] == "AXWebArea" {
                count += 1;
            }
            node = by_index.get(&n["parent_index"].to_string()).copied();
        }
        count
    };
    elements
        .iter()
        .enumerate()
        .filter(|(_, e)| e["in_web_content"] == true)
        .filter(|(_, e)| matches!(e["role"].as_str(), Some("AXTextField" | "AXSecureTextField" | "AXComboBox")))
        .filter(|(_, e)| web_areas(e) == 1)
        .map(|(order, e)| {
            let label = str(e, "label").to_lowercase();
            let password = e["role"] == "AXSecureTextField" || label.contains("password") || label.contains("passcode");
            let kind = if password { "password" } else { "text" };
            Input {
                index: order,
                reference: index(e),
                attrs: HashMap::from([("type".into(), kind.into()), ("name".into(), label)]),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_top_frame_fields() {
        let elements = json!([
            { "element_index": 0, "role": "AXWindow" },
            { "element_index": 3, "role": "AXTextField", "label": "Address and search bar", "parent_index": 0 },
            { "element_index": 7, "role": "AXWebArea", "in_web_content": true, "parent_index": 0 },
            { "element_index": 11, "role": "AXTextField", "label": "Email", "in_web_content": true, "parent_index": 7 },
            { "element_index": 14, "role": "AXTextField", "label": "Password", "in_web_content": true, "parent_index": 7 },
            { "element_index": 20, "role": "AXWebArea", "in_web_content": true, "parent_index": 7 },
            { "element_index": 21, "role": "AXSecureTextField", "label": "", "in_web_content": true, "parent_index": 20 },
        ]);
        let inputs = inputs(elements.as_array().unwrap());
        let refs: Vec<_> = inputs.iter().map(|i| (i.reference.as_str(), i.attrs["type"].as_str())).collect();
        assert_eq!(refs, [("11", "text"), ("14", "password")]);
    }
}
