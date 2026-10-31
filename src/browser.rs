use crate::driver::{refusal, Driver};
use crate::fill::{Input, Page};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct Tab {
    pub target_id: String,
    pub tab_id: String,
}

impl Tab {
    pub fn args(&self) -> Value {
        json!({ "target_id": self.target_id, "tab_id": self.tab_id })
    }
}

pub struct DriverPage<'a> {
    driver: &'a mut Driver,
    tab: Tab,
    snapshot: Option<Value>,
}

impl<'a> DriverPage<'a> {
    pub fn new(driver: &'a mut Driver, tab: Tab) -> Self {
        DriverPage { driver, tab, snapshot: None }
    }

    fn snapshot(&mut self) -> Result<&Value> {
        if self.snapshot.is_none() {
            let snapshot = self.driver.call("get_browser_state", self.tab.args())?;
            if let Some(code) = refusal(&snapshot) {
                bail!("cannot read the tab: {code}");
            }
            self.snapshot = Some(snapshot);
        }
        Ok(self.snapshot.as_ref().unwrap())
    }
}

impl Page for DriverPage<'_> {
    fn url(&mut self) -> Result<String> {
        Ok(self.snapshot()?["url"].as_str().unwrap_or_default().to_string())
    }

    fn inputs(&mut self) -> Result<Vec<Input>> {
        Ok(inputs(self.snapshot()?))
    }

    fn type_text(&mut self, reference: &str, text: &str) -> Result<()> {
        let mut args = self.tab.args();
        args["ref"] = json!(reference);
        args["text"] = json!(text);
        args["replace"] = json!(true);
        let result = self.driver.call("browser_type", args)?;
        if let Some(code) = refusal(&result) {
            bail!("typing failed: {code}");
        }
        Ok(())
    }
}

pub enum Bound {
    Tab(Tab),
    Refused(String),
}

pub fn bind(driver: &mut Driver, pid: i64, window_id: i64) -> Result<Bound> {
    let state = driver.call("get_browser_state", json!({ "pid": pid, "window_id": window_id }))?;
    if let Some(code) = refusal(&state) {
        return Ok(Bound::Refused(code.to_string()));
    }
    let tab = state["tabs"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["active"] == true))
        .context("the window has no active tab")?;
    Ok(Bound::Tab(Tab {
        target_id: state["target_id"].as_str().unwrap_or_default().into(),
        tab_id: tab["tab_id"].as_str().unwrap_or_default().into(),
    }))
}

pub fn inputs(snapshot: &Value) -> Vec<Input> {
    let refs = snapshot["refs"].as_array().cloned().unwrap_or_default();
    refs.iter()
        .enumerate()
        .filter(|(_, r)| r["node"] == "input" && r["frame"] == "main")
        .map(|(index, r)| Input {
            index,
            reference: r["ref"].as_str().unwrap_or_default().into(),
            attrs: r["label"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
        .filter(|i| !matches!(i.kind(), "hidden" | "submit" | "button" | "checkbox" | "radio"))
        .collect()
}
