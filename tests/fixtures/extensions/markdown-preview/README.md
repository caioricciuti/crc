# Markdown Preview

A rendered preview of the Markdown document, in a pane beside the editor.
Press **Cmd-E** in a Markdown file (or run **Open Preview**) to open it, and
Cmd-E again to close it. It follows your edits as you type and keeps its
scroll position.

- CommonMark, plus the GitHub additions: tables, task lists,
  strikethrough and footnotes.
- Images with a relative path, like `![](img/diagram.png)`, load from the
  document's folder.
- Light and dark follow crc's appearance.

What it may do, and what it may not:

- It reads the whole document, and returns a page for crc to show. It never
  changes your text.
- HTML written in the Markdown is shown as text, not rendered.
- crc shows the page with scripts turned off, loads nothing from the
  network, reads files only from the document's folder, and does not follow
  links.

Rendering is done by [pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark),
built from source into the extension.
