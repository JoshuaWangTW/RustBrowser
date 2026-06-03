//! Structured extraction: pull links and tables out as *data* rather than
//! prose. Useful when an LLM needs the relations in a page (where links go,
//! what a table says) instead of a readable narrative.

use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use url::Url;

/// A hyperlink with its visible text and resolved absolute URL.
#[derive(Debug, Clone, Serialize)]
pub struct Link {
    pub text: String,
    pub href: String,
}

/// A table reduced to an optional header row plus body rows.
#[derive(Debug, Clone, Serialize)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Extract all links, resolving relative URLs against `base_url` and skipping
/// empty/in-page (`#…`) anchors.
pub fn extract_links(html: &str, base_url: &str) -> Vec<Link> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("a[href]").expect("valid selector");
    let base = Url::parse(base_url).ok();

    let mut links = Vec::new();
    for el in doc.select(&sel) {
        let raw = el.value().attr("href").unwrap_or("").trim();
        if raw.is_empty() || raw.starts_with('#') || raw.starts_with("javascript:") {
            continue;
        }
        let href = match &base {
            Some(b) => b
                .join(raw)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| raw.to_string()),
            None => raw.to_string(),
        };
        let text = normalize_ws(&el.text().collect::<String>());
        links.push(Link { text, href });
    }
    links
}

/// Extract all tables. The first row made entirely of `<th>` becomes the
/// header; everything else becomes body rows.
pub fn extract_tables(html: &str) -> Vec<Table> {
    let doc = Html::parse_document(html);
    let table_sel = Selector::parse("table").expect("valid selector");
    let tr_sel = Selector::parse("tr").expect("valid selector");
    let th_sel = Selector::parse("th").expect("valid selector");
    let td_sel = Selector::parse("td").expect("valid selector");

    let mut tables = Vec::new();
    for table in doc.select(&table_sel) {
        let mut headers: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();

        for tr in table.select(&tr_sel) {
            let ths: Vec<String> = tr.select(&th_sel).map(|c| cell_text(&c)).collect();
            let tds: Vec<String> = tr.select(&td_sel).map(|c| cell_text(&c)).collect();

            if headers.is_empty() && !ths.is_empty() && tds.is_empty() {
                headers = ths; // a pure-<th> row is the header
            } else if !tds.is_empty() || !ths.is_empty() {
                // Mixed or data row: keep all cells in document order.
                let mut cells = ths;
                cells.extend(tds);
                rows.push(cells);
            }
        }

        if !headers.is_empty() || !rows.is_empty() {
            tables.push(Table { headers, rows });
        }
    }
    tables
}

fn cell_text(el: &ElementRef) -> String {
    normalize_ws(&el.text().collect::<String>())
}

/// Collapse all runs of whitespace (incl. newlines) into single spaces, trimmed.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_resolve_relative_and_skip_anchors() {
        let html = r##"<a href="/a">A</a><a href="#x">x</a><a href="https://e.com/b">B</a>"##;
        let links = extract_links(html, "https://example.com/dir/page");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].href, "https://example.com/a");
        assert_eq!(links[0].text, "A");
        assert_eq!(links[1].href, "https://e.com/b");
    }

    #[test]
    fn tables_split_header_and_rows() {
        let html = r#"<table>
            <tr><th>Name</th><th>Age</th></tr>
            <tr><td>Alice</td><td>30</td></tr>
            <tr><td>Bob</td><td>25</td></tr>
        </table>"#;
        let tables = extract_tables(html);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].headers, vec!["Name", "Age"]);
        assert_eq!(tables[0].rows, vec![vec!["Alice", "30"], vec!["Bob", "25"]]);
    }
}
