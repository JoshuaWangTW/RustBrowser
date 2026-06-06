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
    pub options: Vec<FormOption>,
    pub required: bool,
}

/// A selectable option. `value` is what a form submit sends; `label` is what the
/// user sees.
#[derive(Debug, Clone, Serialize)]
pub struct FormOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
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

/// Caps so the action tree can't explode on a huge page.
#[derive(Debug, Clone, Copy)]
pub struct ActionLimits {
    pub max_links: usize,
    pub max_forms: usize,
    pub max_buttons: usize,
    pub max_downloads: usize,
    pub max_fields_per_form: usize,
    pub max_options_per_field: usize,
}

impl Default for ActionLimits {
    fn default() -> Self {
        Self {
            max_links: 100,
            max_forms: 25,
            max_buttons: 50,
            max_downloads: 50,
            max_fields_per_form: 50,
            max_options_per_field: 50,
        }
    }
}

impl ActionLimits {
    /// Cap every category and nested field/option list at the same value.
    pub fn uniform(n: usize) -> Self {
        Self {
            max_links: n,
            max_forms: n,
            max_buttons: n,
            max_downloads: n,
            max_fields_per_form: n,
            max_options_per_field: n,
        }
    }
}

const MAX_TEXT_CHARS: usize = 240;
const MAX_URL_CHARS: usize = 2_048;
const MAX_FIELD_NAME_CHARS: usize = 128;
const MAX_FIELD_VALUE_CHARS: usize = 512;
const MAX_FILENAME_CHARS: usize = 240;

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
    let base = document_base(&doc, base_url);
    let (links, downloads) = link_and_download_actions(&doc, &base, &limits);
    ActionTree {
        links,
        forms: form_actions(&doc, &base, &limits),
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
        let Some(href) = resolve_http(base, raw) else {
            continue;
        };
        let text = bounded_text(&el.text().collect::<String>(), MAX_TEXT_CHARS);

        match download_filename(&el, &href) {
            Some(filename) => {
                if downloads.len() < limits.max_downloads {
                    downloads.push(DownloadAction {
                        action_id: format!("download_{}", downloads.len()),
                        text,
                        href,
                        filename: (!filename.is_empty())
                            .then(|| truncate_chars(&filename, MAX_FILENAME_CHARS)),
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

fn form_actions(doc: &Html, base: &Option<Url>, limits: &ActionLimits) -> Vec<FormAction> {
    let form_sel = Selector::parse("form").expect("valid selector");
    let mut forms = Vec::new();
    for form in doc.select(&form_sel) {
        if forms.len() >= limits.max_forms {
            break;
        }
        let method = form
            .value()
            .attr("method")
            .map(|m| m.trim().to_ascii_uppercase())
            .filter(|m| m == "POST")
            .unwrap_or_else(|| "GET".to_string());
        let Some(action) = resolve_form_action(base, form.value().attr("action").unwrap_or(""))
        else {
            continue;
        };
        let id = format!("form_{}", forms.len());
        let fields = form_fields(&form, limits);
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

fn form_fields(form: &ElementRef, limits: &ActionLimits) -> Vec<FormField> {
    let sel = Selector::parse("input, select, textarea").expect("valid selector");
    let opt_sel = Selector::parse("option").expect("valid selector");
    let mut fields = Vec::new();
    for el in form.select(&sel) {
        if fields.len() >= limits.max_fields_per_form {
            break;
        }
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
        let Some(name) = el
            .value()
            .attr("name")
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .and_then(|n| exact_bounded(n, MAX_FIELD_NAME_CHARS))
        else {
            continue;
        };
        let required = el.value().attr("required").is_some();
        let value = el
            .value()
            .attr("value")
            .and_then(|v| exact_bounded(v, MAX_FIELD_VALUE_CHARS))
            .filter(|v| !v.is_empty());
        let options = if tag == "select" {
            el.select(&opt_sel)
                .take(limits.max_options_per_field)
                .filter_map(|o| form_option(&o))
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
            bounded_text(el.value().attr("value").unwrap_or(""), MAX_TEXT_CHARS)
        } else {
            bounded_text(&el.text().collect::<String>(), MAX_TEXT_CHARS)
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

fn document_base(doc: &Html, base_url: &str) -> Option<Url> {
    let fallback = Url::parse(base_url).ok()?;
    let sel = Selector::parse("base[href]").expect("valid selector");
    doc.select(&sel)
        .find_map(|b| b.value().attr("href"))
        .and_then(|raw| fallback.join(raw.trim()).ok())
        .filter(is_http_url)
        .or(Some(fallback))
}

fn resolve_http(base: &Option<Url>, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }

    let url = match base {
        Some(b) => b.join(raw).ok()?,
        None => Url::parse(raw).ok()?,
    };
    if is_http_url(&url) {
        exact_bounded(url.as_str(), MAX_URL_CHARS)
    } else {
        None
    }
}

fn resolve_form_action(base: &Option<Url>, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return base
            .as_ref()
            .and_then(|b| exact_bounded(b.as_str(), MAX_URL_CHARS));
    }

    let url = match base {
        Some(b) => b.join(raw).ok()?,
        None => Url::parse(raw).ok()?,
    };
    if is_http_url(&url) {
        exact_bounded(url.as_str(), MAX_URL_CHARS)
    } else {
        None
    }
}

fn is_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

fn form_option(el: &ElementRef) -> Option<FormOption> {
    let label = bounded_text(&el.text().collect::<String>(), MAX_TEXT_CHARS);
    let value = el.value().attr("value").map_or_else(
        || Some(label.clone()),
        |v| exact_bounded(v, MAX_FIELD_VALUE_CHARS),
    )?;
    (!label.is_empty() || !value.is_empty()).then(|| FormOption {
        value,
        label,
        selected: el.value().attr("selected").is_some(),
    })
}

fn bounded_text(s: &str, max_chars: usize) -> String {
    truncate_chars(&normalize_ws(s), max_chars)
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

fn exact_bounded(s: &str, max_chars: usize) -> Option<String> {
    (s.chars().count() <= max_chars).then(|| s.to_string())
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
            <select name="role">
                <option value="admin-role" selected>admin</option>
                <option value="user-role">user</option>
            </select>
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
        assert_eq!(option_values(&f.fields[3]), vec!["admin-role", "user-role"]);
        assert_eq!(option_labels(&f.fields[3]), vec!["admin", "user"]);
        assert!(f.fields[3].options[0].selected);

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
    fn document_base_href_controls_relative_action_urls() {
        let html = r#"<html><head><base href="https://cdn.example.net/app/"></head>
            <body>
              <a href="next">Next</a>
              <form action="search"><input name="q"></form>
            </body></html>"#;
        let t = extract_actions(html, BASE, ActionLimits::default());
        assert_eq!(t.links[0].href, "https://cdn.example.net/app/next");
        assert_eq!(t.forms[0].action, "https://cdn.example.net/app/search");
    }

    #[test]
    fn non_http_urls_are_not_actions() {
        let html = r##"<a href="JaVaScRiPt:alert(1)">bad js</a>
            <a href="data:text/html,hello">bad data</a>
            <a href="mailto:a@example.com">mail</a>
            <form action="javascript:alert(1)"><input name="q"></form>
            <a href="https://safe.example.com/path">safe</a>"##;
        let t = extract_actions(html, BASE, ActionLimits::default());
        assert_eq!(t.links.len(), 1);
        assert_eq!(t.links[0].href, "https://safe.example.com/path");
        assert!(t.forms.is_empty());
    }

    #[test]
    fn limits_cap_nested_fields_options_and_long_values() {
        let overlong_href = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS + 1));
        let html = format!(
            r#"<a href="{overlong_href}">too long</a>
            <a href="/ok">{}</a>
            <form action="/submit">
              <input name="a" value="{}">
              <input name="b" value="keep">
              <input name="c">
              <select name="pick">
                <option value="1">one</option>
                <option value="2">two</option>
                <option value="3">three</option>
              </select>
            </form>"#,
            "visible ".repeat(80),
            "x".repeat(MAX_FIELD_VALUE_CHARS + 1)
        );
        let t = extract_actions(&html, BASE, ActionLimits::uniform(2));
        assert_eq!(t.links.len(), 1, "overlong href should be skipped");
        assert!(t.links[0].text.chars().count() <= MAX_TEXT_CHARS);
        assert_eq!(t.forms[0].fields.len(), 2);
        assert!(t.forms[0].fields[0].value.is_none());

        let options_html = r#"<form action="/submit">
            <select name="pick">
              <option value="1">one</option>
              <option value="2">two</option>
              <option value="3">three</option>
            </select>
        </form>"#;
        let t = extract_actions(
            options_html,
            BASE,
            ActionLimits {
                max_options_per_field: 2,
                ..Default::default()
            },
        );
        assert_eq!(option_values(&t.forms[0].fields[0]), vec!["1", "2"]);
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

    fn option_values(field: &FormField) -> Vec<&str> {
        field.options.iter().map(|o| o.value.as_str()).collect()
    }

    fn option_labels(field: &FormField) -> Vec<&str> {
        field.options.iter().map(|o| o.label.as_str()).collect()
    }
}
