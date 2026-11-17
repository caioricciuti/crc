//! Markdown as an HTML page for the preview pane (Cmd-E), written from the
//! same blocks the editor styles source with, so the two read Markdown the
//! same way.
//!
//! Raw HTML in the source is text: the parser never produces markup, and
//! everything written here is escaped. An image from the network, which the
//! preview never loads, shows as its description.

use super::{Block, Run, Style};

const CSP: &str = "default-src 'none'; img-src file: data:; media-src file: data:; \
                   style-src 'unsafe-inline'";

const STYLE: &str = r#"
:root { color-scheme: light dark; --fg: #1f2328; --muted: #59636e; --line: #d1d9e0;
  --soft: #f6f8fa; --link: #0969da; --bg: #ffffff; }
@media (prefers-color-scheme: dark) {
  :root { --fg: #d1d7e0; --muted: #9198a1; --line: #3d444d; --soft: #151b23;
    --link: #4493f8; --bg: #0d1117; }
}
html { background: var(--bg); }
body { color: var(--fg); font: 15px/1.6 -apple-system, BlinkMacSystemFont, "Helvetica Neue", sans-serif;
  max-width: 860px; margin: 0 auto; padding: 24px 32px 64px; word-wrap: break-word; }
h1, h2, h3, h4, h5, h6 { line-height: 1.25; margin: 1.5em 0 0.6em; font-weight: 600; }
h1 { font-size: 2em; padding-bottom: 0.3em; border-bottom: 1px solid var(--line); }
h2 { font-size: 1.5em; padding-bottom: 0.3em; border-bottom: 1px solid var(--line); }
h3 { font-size: 1.25em; } h4 { font-size: 1em; } h5 { font-size: 0.875em; }
h6 { font-size: 0.85em; color: var(--muted); }
body > :first-child { margin-top: 0; }
p, ul, ol, blockquote, pre, table { margin: 0 0 1em; }
li > ul, li > ol { margin: 0.25em 0 0; }
a { color: var(--link); text-decoration: none; }
ul, ol { padding-left: 2em; }
li + li { margin-top: 0.25em; }
li:has(> input[type=checkbox]) { list-style: none; margin-left: -1.4em; }
input[type=checkbox] { margin: 0 0.4em 0 0; vertical-align: middle; }
blockquote { padding: 0 1em; color: var(--muted); border-left: 0.25em solid var(--line); }
blockquote p:last-child { margin-bottom: 0; }
code, pre { font: 0.875em/1.45 ui-monospace, "SF Mono", Menlo, monospace; }
code { background: var(--soft); padding: 0.2em 0.4em; border-radius: 6px; }
pre { background: var(--soft); padding: 16px; border-radius: 6px; overflow: auto; }
pre code { background: none; padding: 0; font-size: 1em; }
table { border-collapse: collapse; display: block; width: max-content; max-width: 100%; overflow: auto; }
th, td { border: 1px solid var(--line); padding: 6px 13px; }
th { font-weight: 600; }
tr:nth-child(2n) { background: var(--soft); }
img { max-width: 100%; }
.remote-image { display: inline-block; padding: 0 0.45em; border: 1px solid var(--line);
  border-radius: 4px; font-size: 0.85em; color: var(--muted); }
hr { border: 0; height: 0.25em; background: var(--line); margin: 1.5em 0; }
del { color: var(--muted); }
"#;

/// The whole page: the body, a stylesheet that follows light and dark, and a
/// Content-Security-Policy that says what the pane's own rules say (no
/// script, nothing from the network).
pub fn page(markdown: &str) -> String {
    let title = escape(title(markdown).as_deref().unwrap_or("Preview"));
    let body = body(markdown);
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">\
         <title>{title}</title><style>{STYLE}</style></head>\n<body>\n{body}</body></html>\n"
    )
}

/// The first level-one heading's text, for the page title.
fn title(markdown: &str) -> Option<String> {
    super::parse(markdown).into_iter().find_map(|b| match b {
        Block::Heading { level: 1, runs } => {
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            let text = text.trim().to_owned();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    })
}

/// The open containers the next block may continue.
#[derive(Default)]
struct Open {
    /// Lists from the outside in, as (depth, ordered); each has an `<li>` open.
    lists: Vec<(usize, bool)>,
    /// Blockquotes open, and whether a paragraph is open inside the innermost.
    quotes: usize,
    quote_paragraph: bool,
    /// A table's body is open.
    table: bool,
}

impl Open {
    fn close_lists_to(&mut self, out: &mut String, keep: impl Fn(usize, bool) -> bool) {
        while let Some(&(depth, ordered)) = self.lists.last() {
            if keep(depth, ordered) {
                break;
            }
            out.push_str(if ordered {
                "</li></ol>\n"
            } else {
                "</li></ul>\n"
            });
            self.lists.pop();
        }
    }

    fn close_quote_paragraph(&mut self, out: &mut String) {
        if self.quote_paragraph {
            out.push_str("</p>\n");
            self.quote_paragraph = false;
        }
    }

    fn close_quotes_to(&mut self, out: &mut String, depth: usize) {
        if self.quotes > depth {
            self.close_quote_paragraph(out);
        }
        while self.quotes > depth {
            out.push_str("</blockquote>\n");
            self.quotes -= 1;
        }
    }

    fn close_table(&mut self, out: &mut String) {
        if self.table {
            out.push_str("</tbody></table>\n");
            self.table = false;
        }
    }

    /// Closes everything except what `block` continues.
    fn close_for(&mut self, out: &mut String, block: &Block) {
        if !matches!(block, Block::ListItem { .. }) {
            self.close_lists_to(out, |_, _| false);
        }
        if !matches!(block, Block::Quote { .. }) {
            self.close_quotes_to(out, 0);
        }
        if !matches!(block, Block::TableRow { header: false, .. }) {
            self.close_table(out);
        }
    }
}

/// The page body.
pub fn body(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len() * 3 / 2);
    let mut open = Open::default();
    for block in super::parse(markdown) {
        open.close_for(&mut out, &block);
        match block {
            Block::Heading { level, runs } => {
                out.push_str(&format!("<h{level}>"));
                inline(&mut out, &runs);
                out.push_str(&format!("</h{level}>\n"));
            }
            Block::Paragraph { runs } => {
                out.push_str("<p>");
                inline(&mut out, &runs);
                out.push_str("</p>\n");
            }
            Block::Code { lang, lines } => {
                let lang = lang.split_whitespace().next().unwrap_or("");
                if lang.is_empty() {
                    out.push_str("<pre><code>");
                } else {
                    out.push_str(&format!("<pre><code class=\"language-{}\">", escape(lang)));
                }
                for line in &lines {
                    out.push_str(&escape(line));
                    out.push('\n');
                }
                out.push_str("</code></pre>\n");
            }
            Block::ListItem {
                depth,
                number,
                task,
                runs,
            } => {
                let ordered = number.is_some();
                open.close_lists_to(&mut out, |d, o| d < depth || (d == depth && o == ordered));
                if open.lists.last().is_some_and(|&(d, _)| d == depth) {
                    out.push_str("</li>\n");
                } else {
                    match number {
                        Some(n) if n != 1 => out.push_str(&format!("<ol start=\"{n}\">\n")),
                        Some(_) => out.push_str("<ol>\n"),
                        None => out.push_str("<ul>\n"),
                    }
                    open.lists.push((depth, ordered));
                }
                out.push_str("<li>");
                match task {
                    Some(true) => out.push_str("<input type=\"checkbox\" disabled checked>"),
                    Some(false) => out.push_str("<input type=\"checkbox\" disabled>"),
                    None => {}
                }
                inline(&mut out, &runs);
            }
            Block::Quote { depth, runs } => {
                let want = depth + 1;
                open.close_quotes_to(&mut out, want);
                if open.quotes < want {
                    open.close_quote_paragraph(&mut out);
                }
                while open.quotes < want {
                    out.push_str("<blockquote>\n");
                    open.quotes += 1;
                }
                if runs.is_empty() {
                    open.close_quote_paragraph(&mut out);
                } else {
                    if open.quote_paragraph {
                        out.push('\n');
                    } else {
                        out.push_str("<p>");
                        open.quote_paragraph = true;
                    }
                    inline(&mut out, &runs);
                }
            }
            Block::TableRow { cells, header } => {
                let cell = if header { "th" } else { "td" };
                if header {
                    out.push_str("<table><thead>");
                } else if !open.table {
                    out.push_str("<table><tbody>");
                }
                out.push_str("<tr>");
                for runs in &cells {
                    out.push_str(&format!("<{cell}>"));
                    inline(&mut out, runs);
                    out.push_str(&format!("</{cell}>"));
                }
                out.push_str("</tr>");
                if header {
                    out.push_str("</thead><tbody>");
                }
                out.push('\n');
                open.table = true;
            }
            Block::Rule => out.push_str("<hr>\n"),
            Block::Blank => {}
        }
    }
    open.close_for(&mut out, &Block::Blank);
    out
}

fn inline(out: &mut String, runs: &[Run]) {
    for run in runs {
        let text = escape(&run.text);
        let target = run.target.as_deref().unwrap_or("");
        match run.style {
            Style::Plain => out.push_str(&text),
            Style::Strong => out.push_str(&format!("<strong>{text}</strong>")),
            Style::Emphasis => out.push_str(&format!("<em>{text}</em>")),
            Style::StrongEmphasis => out.push_str(&format!("<strong><em>{text}</em></strong>")),
            Style::Code => out.push_str(&format!("<code>{text}</code>")),
            Style::Strike => out.push_str(&format!("<del>{text}</del>")),
            Style::Link if is_script(target) => out.push_str(&text),
            Style::Link => out.push_str(&format!("<a href=\"{}\">{text}</a>", escape(target))),
            Style::Image if is_remote(target) || is_script(target) => out.push_str(&format!(
                "<span class=\"remote-image\" title=\"{}\">{text}</span>",
                escape(target)
            )),
            Style::Image => {
                out.push_str(&format!("<img src=\"{}\" alt=\"{text}\">", escape(target)))
            }
        }
    }
}

/// An address on the network: the preview loads files from the document's
/// folder only.
fn is_remote(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    url.starts_with("http:") || url.starts_with("https:") || url.starts_with("//")
}

/// A `javascript:` address. Script is off in the pane, so this is belt and
/// braces.
fn is_script(url: &str) -> bool {
    url.trim_start()
        .to_ascii_lowercase()
        .starts_with("javascript:")
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_blocks_people_write() {
        let html = body(
            "# Title\n\nSome *em* and **strong** and `code`, [a link](docs/a.md \"t\").\n\n\
             | a | b |\n|---|:-:|\n| 1 | 2 |\n| 3 | 4 |\n\n- [x] done\n- [ ] todo\n\n~~gone~~\n\n\
             ```rust\nfn main() {}\n```\n\n---\n",
        );
        for want in [
            "<h1>Title</h1>",
            "<em>em</em>",
            "<strong>strong</strong>",
            "<code>code</code>",
            "<a href=\"docs/a.md\">a link</a>",
            "<table><thead><tr><th>a</th><th>b</th></tr></thead><tbody>\n<tr><td>1</td><td>2</td></tr>\n<tr><td>3</td><td>4</td></tr>\n</tbody></table>",
            "<li><input type=\"checkbox\" disabled checked>done</li>",
            "<del>gone</del>",
            "<pre><code class=\"language-rust\">fn main() {}\n</code></pre>",
            "<hr>",
        ] {
            assert!(html.contains(want), "missing {want:?} in\n{html}");
        }
    }

    #[test]
    fn lists_nest_and_switch_kind() {
        let html = body("- a\n  - b\n  - c\n- d\n\n3. x\n4. y\n");
        assert_eq!(
            html,
            "<ul>\n<li>a<ul>\n<li>b</li>\n<li>c</li></ul>\n</li>\n<li>d</li></ul>\n\
             <ol start=\"3\">\n<li>x</li>\n<li>y</li></ol>\n"
        );
    }

    #[test]
    fn quotes_join_lines_and_nest() {
        let html = body("> one\n> two\n>\n> > inner\n\nafter\n");
        assert_eq!(
            html,
            "<blockquote>\n<p>one\ntwo</p>\n<blockquote>\n<p>inner</p>\n</blockquote>\n</blockquote>\n<p>after</p>\n"
        );
    }

    #[test]
    fn raw_html_is_text() {
        let html = body("<script>alert(1)</script>\n\nand <b onclick=\"x\">inline</b>\n");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<b "), "{html}");
        assert!(
            html.contains("&lt;b onclick=&quot;x&quot;&gt;inline"),
            "{html}"
        );
    }

    #[test]
    fn remote_images_show_as_their_description() {
        let html = body("![CI](https://example.com/ci.svg?x=1&y=2)\n\n![map](img/map.png)\n");
        assert!(
            html.contains(
                "<span class=\"remote-image\" title=\"https://example.com/ci.svg?x=1&amp;y=2\">CI</span>"
            ),
            "{html}"
        );
        assert!(
            html.contains("<img src=\"img/map.png\" alt=\"map\">"),
            "{html}"
        );
    }

    #[test]
    fn script_links_are_text() {
        let html = body("[x](javascript:alert(1))\n");
        assert!(!html.contains("href"), "{html}");
    }

    #[test]
    fn a_whole_page_with_an_escaped_title() {
        let html = page("# a </title><b>\n\nHi & bye\n");
        assert!(html.starts_with("<!doctype html>"));
        assert!(
            html.contains("<title>a &lt;/title&gt;&lt;b&gt;</title>"),
            "{html}"
        );
        assert!(html.contains("Content-Security-Policy"));
        assert!(html.contains("<p>Hi &amp; bye</p>"));
    }
}
