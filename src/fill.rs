//! Typing a login into a Chrome tab, after checking the tab's site against
//! the URLs saved on the item.

use crate::driver::Driver;
use crate::vault::Login;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::collections::HashMap;
use url::Url;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Field {
    Username,
    Password,
    Totp,
}

impl Field {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "username" => Field::Username,
            "password" => Field::Password,
            "totp" => Field::Totp,
            _ => bail!("unknown field {s:?}: use username, password or totp"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Field::Username => "username",
            Field::Password => "password",
            Field::Totp => "totp",
        }
    }
}

pub struct Filled {
    pub host: String,
    pub fields: Vec<Field>,
}

pub struct Input {
    /// Document order, used to find the field just before the password.
    pub index: usize,
    pub reference: String,
    pub attrs: HashMap<String, String>,
}

/// A browser tab that can be read and typed into.
pub trait Page {
    fn url(&mut self) -> Result<String>;
    fn inputs(&mut self) -> Result<Vec<Input>>;
    fn type_text(&mut self, reference: &str, text: &str) -> Result<()>;
}

impl Input {
    fn attr(&self, key: &str) -> &str {
        self.attrs.get(key).map(String::as_str).unwrap_or("")
    }

    pub fn kind(&self) -> &str {
        match self.attr("type") {
            "" => "text",
            t => t,
        }
    }

    fn named_like(&self, words: &[&str]) -> bool {
        ["name", "id", "autocomplete"].iter().any(|k| {
            let v = self.attr(k).to_ascii_lowercase();
            words.iter().any(|w| v.contains(w))
        })
    }
}

/// Refuses while cua-driver is recording, so values never land in a video.
pub fn check_not_recording(driver: &mut Driver) -> Result<()> {
    let recording = driver.call("get_recording_state", json!({}))?;
    if recording["recording"] == true || recording["video_active"] == true {
        bail!("cua-driver is recording; stop the recording before filling a login");
    }
    Ok(())
}

/// Fills `wanted` (or, if empty, whichever of username and password the page
/// shows) into the tab. Values are fetched lazily through `secret` only after
/// every check has passed.
pub fn fill(
    page: &mut dyn Page,
    login: &Login,
    wanted: &[Field],
    secret: &mut dyn FnMut(Field) -> Result<String>,
) -> Result<Filled> {
    let url = Url::parse(&page.url()?).context("the tab has no readable URL")?;
    let host = check_site(&url, login)?;

    let inputs = page.inputs()?;
    let password = inputs.iter().find(|i| i.kind() == "password");
    let username = find_username(&inputs, password);
    let totp = inputs.iter().find(|i| {
        matches!(i.kind(), "text" | "tel" | "number")
            && i.named_like(&["otp", "totp", "2fa", "mfa", "one-time", "code", "token"])
    });

    let fields: Vec<Field> = if wanted.is_empty() {
        [(Field::Username, username), (Field::Password, password)]
            .into_iter()
            .filter(|(_, input)| input.is_some())
            .map(|(f, _)| f)
            .collect()
    } else {
        wanted.to_vec()
    };
    if fields.is_empty() {
        bail!("no username or password field found on {host}");
    }

    // Resolve every target before typing anything, so a missing field
    // never leaves the form half filled.
    let mut plan = Vec::new();
    for field in &fields {
        let input = match field {
            Field::Username => username,
            Field::Password => password,
            Field::Totp => totp,
        };
        let input = input.with_context(|| format!("no {} field found on {host}", field.name()))?;
        plan.push((*field, input.reference.clone()));
    }

    for (field, reference) in plan {
        let value = secret(field)?;
        page.type_text(&reference, &value)
            .with_context(|| format!("filling the {} failed", field.name()))?;
    }
    Ok(Filled { host, fields })
}

/// The page must be on a host saved on the item (or a subdomain of one), over
/// https unless the saved URL itself is http.
fn check_site(page: &Url, login: &Login) -> Result<String> {
    let host = page.host_str().context("the tab has no host")?.to_ascii_lowercase();
    let allowed: Vec<Url> = login.uris.iter().filter_map(|u| parse_saved(u)).collect();
    if allowed.is_empty() {
        bail!("{:?} has no website saved in Bitwarden, so it can't be checked", login.name);
    }
    let ok = allowed.iter().any(|saved| {
        let Some(saved_host) = saved.host_str().map(str::to_ascii_lowercase) else {
            return false;
        };
        let host_ok = host == saved_host || host.ends_with(&format!(".{saved_host}"));
        let scheme_ok = page.scheme() == "https" || page.scheme() == saved.scheme();
        host_ok && scheme_ok
    });
    if !ok {
        bail!("{:?} is not saved for {host}; refusing to fill", login.name);
    }
    Ok(host)
}

fn parse_saved(uri: &str) -> Option<Url> {
    Url::parse(uri)
        .ok()
        .filter(|u| u.host_str().is_some())
        .or_else(|| Url::parse(&format!("https://{uri}")).ok())
}

fn find_username<'a>(inputs: &'a [Input], password: Option<&Input>) -> Option<&'a Input> {
    let textual = |i: &&Input| matches!(i.kind(), "text" | "email" | "tel");
    inputs
        .iter()
        .filter(textual)
        .find(|i| i.named_like(&["user", "email", "login", "account", "identifier"]))
        .or_else(|| {
            // Otherwise the last text field before the password field.
            let before = password.map_or(usize::MAX, |p| p.index);
            inputs.iter().filter(textual).filter(|i| i.index < before).last()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login(uris: &[&str]) -> Login {
        Login {
            id: "x".into(),
            name: "test".into(),
            username: None,
            password: None,
            has_totp: false,
            uris: uris.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn check(page: &str, uris: &[&str]) -> bool {
        check_site(&Url::parse(page).unwrap(), &login(uris)).is_ok()
    }

    #[test]
    fn site_matching() {
        assert!(check("https://github.com/login", &["https://github.com"]));
        assert!(check("https://www.github.com/login", &["github.com"]));
        assert!(!check("https://github.com.evil.io/", &["https://github.com"]));
        assert!(!check("https://evilgithub.com/", &["https://github.com"]));
        assert!(!check("http://github.com/", &["https://github.com"]));
        assert!(check("http://localhost:8080/", &["http://localhost:8080"]));
        assert!(!check("https://github.com/", &[]));
    }

    #[test]
    fn finds_fields() {
        let snapshot = json!({ "refs": [
            { "frame": "main", "node": "input", "ref": "p1:1", "label": "type=hidden name=csrf" },
            { "frame": "main", "node": "input", "ref": "p1:2", "label": "name=q type=search" },
            { "frame": "main", "node": "input", "ref": "p1:3", "label": "name=login_field id=login_field type=text" },
            { "frame": "main", "node": "input", "ref": "p1:4", "label": "name=password type=password" },
            { "frame": "ad", "node": "input", "ref": "p1:5", "label": "type=password" },
        ]});
        let inputs = crate::browser::inputs(&snapshot);
        let password = inputs.iter().find(|i| i.kind() == "password");
        assert_eq!(password.unwrap().reference, "p1:4");
        assert_eq!(find_username(&inputs, password).unwrap().reference, "p1:3");
    }
}
