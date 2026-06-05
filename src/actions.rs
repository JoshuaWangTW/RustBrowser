//! Action-tree extraction for Browser Use.
//!
//! Surfaces the *operable* parts of a page — links, forms (with their fields and
//! submit), standalone buttons, and downloads — each with a stable `action_id`
//! (assigned by document order) that an agent can later act on without ever
//! opening a browser. This is the "Observe" half of Observe → Act → Verify.

use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use url::Url;

/// The operable surface of a page.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ActionTree {
    pub links: Vec<LinkAction>,
    pub forms: Vec<FormAction>,
    pub buttons: Vec<ButtonAction>,
    pub downloads: Vec<DownloadAction>,
}

impl ActionTree {
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
            && self.forms.is_empty()
            && self.buttons.is_empty()
            && self.downloads.is_empty()
    }

    /// Total number of actions across all categories.
    pub fn len(&self) -> usize {
        self.links.len() + self.forms.len() + self.buttons.len() + self.downloads.len()
    }
}

/// A navigable link: `follow` it to fetch its target.
#[derive(Debug, Clone, Serialize)]
pub struct LinkAction {
    pub action_id: String,
    pub text: String,
    pub href: String,
}

/// A form that can be submitted without JavaScript.
#[derive(Debug, Clone, Serialize)]
pub struct FormAction {
    pub action_id: String,
    /// `GET` or `POST`.
    pub method: String,
    /// Absolute URL the form submits to.
    pub action: String,
    pub fields: Vec<FormField>,
    /// The id to submit this form, e.g. `form_0.submit`.
    pub submit_id: String,
}

/// A single form control.
#[derive(Debug, Clone, Serialize)]
pub struct FormField {
    pub name: String,
    /// `text`, `password`, `email`, `checkbox`, `hidden`, `select`, `textarea`, …
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    pub required: bool,
}

/// A standalone button (outside any form — usually JS-driven).
#[derive(Debug, Clone, Serialize)]
pub struct ButtonAction {
    pub action_id: String,
    pub text: String,
    /// `button`, `submit`, or `reset`.
    pub kind: String,
}

/// A link that downloads a file rather than navigating.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadAction {
    pub action_id: String,
    pub text: String,
    pub href: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

/// Per-category caps so the action tree can't explode on a huge page.
#[derive(Debug, Clone, Copy)]
pub struct ActionLimits {
    pub max_links: usize,
    pub max_forms: usize,
    pub max_buttons: usize,
    pub max_downloads: usize,
}

impl Default for ActionLimits {
    fn default() -> Self {
        Self {
            max_links: 100,
            max_forms: 25,
            max_buttons: 50,
            max_downloads: 50,
        }
    }
}

impl ActionLimits {
    /// Cap every category at the same value.
    pub fn uniform(n: usize) -> Self {
        Self {
            max_links: n,
            max_forms: n,
            max_buttons: n,
            max_downloads: n,
        }
    }
}

/// File extensions that mark an `<a href>` as a download rather than navigation.
const DOWNLOAD_EXTS: &[&str] = &[
    ".pdf", ".zip", ".tar", ".gz", ".tgz", ".rar", ".7z", ".csv", ".tsv", ".xlsx", ".xls", ".doc",
    ".docx", ".ppt", ".pptx", ".odt", ".ods", ".dmg", ".exe", ".msi", ".deb", ".rpm", ".apk",
    ".iso", ".mp3", ".mp4", ".wav", ".mov",
];

/// Extract the action tree from a full HTML page. Relative URLs are resolved
/// against `base_url`; each category is capped by `limits`.
pub fn extract_actions(html: &str, base_url: &str, limits: ActionLimits) -> ActionTree {
    let doc = Html::parse_document(html);
    let base = Url::parse(base_url).ok();
    let (links, downloads) = link_and_download_actions(&doc, &base, &limits);
    ActionTree {
        links,
        forms: form_actions(&doc, &base, limits.max_forms),
        buttons: button_actions(&doc, limits.max_buttons),
        downloads,
    }
}

fn link_and_download_actions(
    doc: &Html,
    base: &Option<Url>,
    limits: &ActionLimits,
) -> (Vec<LinkAction>, Vec<DownloadAction>) {
    let sel = Selector::parse("a[href]").expect("valid selector");
    let mut links = Vec::new();
    let mut downloads = Vec::new();

    for el in doc.select(&sel) {
        let raw = el.value().attr("href").unwrap_or("").trim();
        if raw.is_empty()
            || raw.starts_with('#')
            || raw.starts_with("javascript:")
            || raw.starts_with("mailto:")
            || raw.starts_with("tel:")
        {
            continue;
        }
        let href = resolve(base, raw);
        let text = normalize_ws(&el.text().collect::<String>());

        match download_filename(&el, &href) {
            Some(filename) => {
                if downloads.len() < limits.max_downloads {
                    downloads.push(DownloadAction {
                        action_id: format!("download_{}", downloads.len()),
                        text,
                        href,
                        filename: (!filename.is_empty()).then_some(filename),
                    });
                }
            }
            None => {
                if links.len() < limits.max_links {
                    links.push(LinkAction {
                        action_id: format!("link_{}", links.len()),
                        text,
                        href,
                    });
                }
            }
        }
    }
    (links, downloads)
}

/// `Some(filename)` if the link is a download (explicit `download` attr or a
/// known file extension); `None` if it is ordinary navigation.
fn download_filename(el: &ElementRef, href: &str) -> Option<String> {
    if let Some(dl) = el.value().attr("download") {
        let name = dl.trim();
        return Some(if name.is_empty() {
            filename_from_href(href).unwrap_or_default()
        } else {
            name.to_string()
        });
    }
    let path = href
        .split(['?', '#'])
        .next()
        .unwrap_or(href)
        .to_ascii_lowercase();
    if DOWNLOAD_EXTS.iter().any(|ext| path.ends_with(ext)) {
        return Some(filename_from_href(href).unwrap_or_default());
    }
    None
}

fn filename_from_href(href: &str) -> Option<String> {
    let path = href.split(['?', '#']).next().unwrap_or(href);
    let seg = path.rsplit('/').next().unwrap_or("");
    (!seg.is_empty()).then(|| seg.to_string())
}

fn form_actions(doc: &Html, base: &Option<Url>, max: usize) -> Vec<FormAction> {
    let form_sel = Selector::parse("form").expect("valid selector");
    let mut forms = Vec::new();
    for form in doc.select(&form_sel) {
        if forms.len() >= max {
            break;
        }
        let method = form
            .value()
            .attr("method")
            .map(|m| m.trim().to_ascii_uppercase())
            .filter(|m| m == "POST")
            .unwrap_or_else(|| "GET".to_string());
        let action = resolve(base, form.value().attr("action").unwrap_or("").trim());
        let id = format!("form_{}", forms.len());
        let fields = form_fields(&form);
        forms.push(FormAction {
            submit_id: format!("{id}.submit"),
            action_id: id,
            method,
            action,
            fields,
        });
    }
    forms
}

fn form_fields(form: &ElementRef) -> Vec<FormField> {
    let sel = Selector::parse("input, select, textarea").expect("valid selector");
    let opt_sel = Selector::parse("option").expect("valid selector");
    let mut fields = Vec::new();
    for el in form.select(&sel) {
        let tag = el.value().name();
        let kind = match tag {
            "select" => "select".to_string(),
            "textarea" => "textarea".to_string(),
            _ => el
                .value()
                .attr("type")
                .unwrap_or("text")
                .to_ascii_lowercase(),
        };
        // Submit/reset/button/image inputs are actions, not data fields.
        if matches!(kind.as_str(), "submit" | "reset" | "button" | "image") {
            continue;
        }
        let name = el.value().attr("name").unwrap_or("").to_string();
        let required = el.value().attr("required").is_some();
        let value = el
            .value()
            .attr("value")
            .map(str::to_string)
            .filter(|v| !v.is_empty());
        let options = if tag == "select" {
            el.select(&opt_sel)
                .map(|o| normalize_ws(&o.text().collect::<String>()))
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        fields.push(FormField {
            name,
            kind,
            value,
            options,
            required,
        });
    }
    fields
}

fn button_actions(doc: &Html, max: usize) -> Vec<ButtonAction> {
    let sel = Selector::parse("button, input[type=button], input[type=submit], input[type=reset]")
        .expect("valid selector");
    let mut out = Vec::new();
    for el in doc.select(&sel) {
        if out.len() >= max {
            break;
        }
        // Buttons inside a form belong to that form's submit, not here.
        if within_form(&el) {
            continue;
        }
        let kind = el
            .value()
            .attr("type")
            .unwrap_or("button")
            .to_ascii_lowercase();
        let text = if el.value().name() == "input" {
            normalize_ws(el.value().attr("value").unwrap_or(""))
        } else {
            normalize_ws(&el.text().collect::<String>())
        };
        out.push(ButtonAction {
            action_id: format!("button_{}", out.len()),
            text,
            kind,
        });
    }
    out
}

fn within_form(el: &ElementRef) -> bool {
    el.ancestors().any(|n| {
        n.value()
            .as_element()
            .is_some_and(|e| e.name().eq_ignore_ascii_case("form"))
    })
}

fn resolve(base: &Option<Url>, raw: &str) -> String {
    match base {
        Some(b) => b
            .join(raw)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| raw.to_string()),
        None => raw.to_string(),
    }
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://example.com/dir/page";

    #[test]
    fn splits_links_and_downloads_with_stable_ids() {
        let html = r##"<a href="/about">About</a>
            <a href="#top">top</a>
            <a href="/files/report.pdf">Report</a>
            <a href="https://e.com/x">X</a>
            <a href="/data.csv" download="rows.csv">Data</a>"##;
        let t = extract_actions(html, BASE, ActionLimits::default());

        assert_eq!(t.links.len(), 2); // About, X (anchor #top skipped)
        assert_eq!(t.links[0].action_id, "link_0");
        assert_eq!(t.links[0].href, "https://example.com/about");
        assert_eq!(t.links[1].action_id, "link_1");

        assert_eq!(t.downloads.len(), 2); // report.pdf, data.csv
        assert_eq!(t.downloads[0].action_id, "download_0");
        assert_eq!(t.downloads[0].filename.as_deref(), Some("report.pdf"));
        assert_eq!(t.downloads[1].filename.as_deref(), Some("rows.csv")); // download attr
    }

    #[test]
    fn extracts_form_with_fields_and_submit() {
        let html = r#"<form method="post" action="/login">
            <input type="text" name="user" required>
            <input type="password" name="pass" required>
            <input type="hidden" name="csrf" value="abc123">
            <select name="role"><option>admin</option><option>user</option></select>
            <button type="submit">Sign in</button>
        </form>"#;
        let t = extract_actions(html, BASE, ActionLimits::default());

        assert_eq!(t.forms.len(), 1);
        let f = &t.forms[0];
        assert_eq!(f.action_id, "form_0");
        assert_eq!(f.submit_id, "form_0.submit");
        assert_eq!(f.method, "POST");
        assert_eq!(f.action, "https://example.com/login");
        assert_eq!(f.fields.len(), 4); // user, pass, csrf, role (submit button excluded)
        assert_eq!(f.fields[0].name, "user");
        assert!(f.fields[0].required);
        assert_eq!(f.fields[2].value.as_deref(), Some("abc123")); // hidden csrf
        assert_eq!(f.fields[3].kind, "select");
        assert_eq!(f.fields[3].options, vec!["admin", "user"]);

        // The submit button is part of the form, not a standalone button.
        assert!(t.buttons.is_empty());
    }

    #[test]
    fn default_method_is_get_and_empty_action_is_current_url() {
        let html = r#"<form><input name="q"></form>"#;
        let t = extract_actions(html, BASE, ActionLimits::default());
        assert_eq!(t.forms[0].method, "GET");
        assert_eq!(t.forms[0].action, BASE); // empty action → current URL
    }

    #[test]
    fn standalone_button_is_captured() {
        let html = r#"<button type="button" onclick="x()">Load more</button>"#;
        let t = extract_actions(html, BASE, ActionLimits::default());
        assert_eq!(t.buttons.len(), 1);
        assert_eq!(t.buttons[0].action_id, "button_0");
        assert_eq!(t.buttons[0].text, "Load more");
        assert_eq!(t.buttons[0].kind, "button");
    }

    #[test]
    fn limits_cap_each_category() {
        let html = (0..10)
            .map(|i| format!(r#"<a href="/p{i}">L{i}</a>"#))
            .collect::<String>();
        let t = extract_actions(&html, BASE, ActionLimits::uniform(3));
        assert_eq!(t.links.len(), 3);
    }
}
