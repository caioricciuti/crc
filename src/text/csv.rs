//! CSV and TSV model over the text buffer: dialect detection, a tokenizer
//! that maps every cell to a byte range of the source, and the encoders a
//! table view uses to write a cell or a row back as text.
//!
//! Nothing here touches a buffer. The parse is one linear pass over the
//! bytes and every range it reports lies on a char boundary, because the
//! delimiter, the quote and the line breaks are ASCII and never fall inside
//! a multibyte sequence. Callers splice the strings from `encode_cell` and
//! `row_text` at the ranges the parse handed out.

use std::borrow::Cow;
use std::ops::Range;

pub use super::file_format::Ending;

/// RFC 4180 fixes the quote character; dialects differ only in the delimiter.
pub const QUOTE: u8 = b'"';

/// The byte order mark as it appears in decoded text.
pub const BOM: &str = "\u{feff}";

/// Delimiters tried by `Dialect::detect`, in tie-break order.
const CANDIDATES: [u8; 4] = *b",;\t|";

/// Detection reads at most this many bytes, or this many records,
/// whichever comes first.
const SAMPLE_BYTES: usize = 64 * 1024;
const SAMPLE_LINES: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dialect {
    pub delimiter: u8,
    /// Always `QUOTE`; kept on the dialect so callers never hardcode it.
    pub quote: u8,
    pub line_ending: Ending,
    pub bom: bool,
}

impl Default for Dialect {
    fn default() -> Self {
        Self::with_delimiter(b',')
    }
}

impl Dialect {
    pub fn with_delimiter(delimiter: u8) -> Self {
        Self {
            delimiter,
            quote: QUOTE,
            line_ending: Ending::Lf,
            bom: false,
        }
    }

    /// Guess the dialect from the head of `text`. The delimiter is the
    /// candidate whose per-record count is the same on the most records
    /// (counted outside quotes, so a quoted cell holding the other
    /// delimiter does not vote). The line ending is the first one found,
    /// the BOM is read from the first bytes. Comma and LF are the defaults.
    pub fn detect(text: &str) -> Dialect {
        let bom = text.starts_with(BOM);
        let body = if bom { &text[BOM.len()..] } else { text };
        let bytes = body.as_bytes();
        let line_ending = match bytes.iter().position(|b| *b == b'\r' || *b == b'\n') {
            Some(i) if bytes[i] == b'\n' => Ending::Lf,
            Some(i) if bytes.get(i + 1) == Some(&b'\n') => Ending::CrLf,
            Some(_) => Ending::Cr,
            None => Ending::Lf,
        };
        Dialect {
            delimiter: detect_delimiter(bytes),
            quote: QUOTE,
            line_ending,
            bom,
        }
    }

    /// The line ending as text, for callers inserting a row.
    pub fn line_ending_text(&self) -> &'static str {
        match self.line_ending {
            Ending::Lf => "\n",
            Ending::CrLf => "\r\n",
            Ending::Cr => "\r",
        }
    }
}

fn detect_delimiter(bytes: &[u8]) -> u8 {
    // One pass collects the count of every candidate per record, so the
    // quote state is tracked once rather than once per candidate.
    let limit = bytes.len().min(SAMPLE_BYTES);
    let mut records: Vec<[u32; 4]> = Vec::new();
    let mut current = [0u32; 4];
    let mut quoted = false;
    let mut i = 0;
    while i < limit && records.len() < SAMPLE_LINES {
        let b = bytes[i];
        if quoted {
            // A doubled quote toggles twice and lands back inside, which
            // is the right answer for counting.
            if b == QUOTE {
                quoted = false;
            }
        } else if b == QUOTE {
            quoted = true;
        } else if b == b'\n' || b == b'\r' {
            if b == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            records.push(current);
            current = [0; 4];
        } else if let Some(k) = CANDIDATES.iter().position(|c| *c == b) {
            current[k] += 1;
        }
        i += 1;
    }
    // A final line without a line break is a record; a line cut by the
    // byte limit is not.
    if i == bytes.len() && i > 0 && !matches!(bytes[i - 1], b'\n' | b'\r') {
        records.push(current);
    }
    let mut best = (0usize, b',');
    for (k, candidate) in CANDIDATES.iter().enumerate() {
        let mut counts: Vec<u32> = records
            .iter()
            .map(|r| r[k])
            .filter(|count| *count > 0)
            .collect();
        counts.sort_unstable();
        let mut agreeing = 0;
        let mut run = 0;
        for (j, count) in counts.iter().enumerate() {
            if j > 0 && *count == counts[j - 1] {
                run += 1;
            } else {
                run = 1;
            }
            agreeing = agreeing.max(run);
        }
        if agreeing > best.0 {
            best = (agreeing, *candidate);
        }
    }
    best.1
}

/// One cell as it sits in the source: the raw bytes, quotes included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub range: Range<usize>,
    pub quoted: bool,
}

/// One record. The range covers its text without the line ending; a quoted
/// cell with a line break inside makes the record span several lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub range: Range<usize>,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub rows: Vec<Row>,
}

impl Table {
    /// The width of the longest row. Ragged rows are kept short, so a
    /// view pads with this rather than the parse.
    pub fn column_count(&self) -> usize {
        self.rows
            .iter()
            .map(|row| row.cells.len())
            .max()
            .unwrap_or(0)
    }

    pub fn cell(&self, row: usize, column: usize) -> Option<&Cell> {
        self.rows.get(row)?.cells.get(column)
    }

    /// The value of a cell: quotes stripped and doubled quotes folded.
    /// Borrows unless the cell holds an escaped quote. Text after the
    /// closing quote is kept as written; the parse is lenient about it.
    pub fn cell_text<'a>(text: &'a str, cell: &Cell) -> Cow<'a, str> {
        let raw = &text[cell.range.clone()];
        if !cell.quoted {
            return Cow::Borrowed(raw);
        }
        let inner = &raw[1..];
        let bytes = inner.as_bytes();
        let Some(first_quote) = bytes.iter().position(|b| *b == QUOTE) else {
            // Unterminated and without any quote inside.
            return Cow::Borrowed(inner);
        };
        if first_quote == bytes.len() - 1 {
            return Cow::Borrowed(&inner[..first_quote]);
        }
        let mut out = String::with_capacity(inner.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == QUOTE {
                if bytes.get(i + 1) == Some(&QUOTE) {
                    out.push('"');
                    i += 2;
                    continue;
                }
                // The closing quote; anything after it is stray.
                out.push_str(&inner[i + 1..]);
                return Cow::Owned(out);
            }
            let start = i;
            while i < bytes.len() && bytes[i] != QUOTE {
                i += 1;
            }
            out.push_str(&inner[start..i]);
        }
        Cow::Owned(out)
    }
}

/// Tokenize `text` with `dialect`. A leading BOM is skipped. Quoted cells
/// may hold the delimiter, doubled quotes and line breaks; an unterminated
/// quote runs to the end of the text. A trailing line ending adds no row,
/// an empty line is one row with one empty cell, ragged rows stay ragged.
pub fn parse(text: &str, dialect: &Dialect) -> Table {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let delimiter = dialect.delimiter;
    let quote = dialect.quote;
    let mut rows = Vec::with_capacity(n / 32 + 1);
    let mut pos = if text.starts_with(BOM) { BOM.len() } else { 0 };
    if pos >= n {
        return Table { rows };
    }
    loop {
        let row_start = pos;
        let mut cells = Vec::new();
        loop {
            let cell_start = pos;
            let quoted = pos < n && bytes[pos] == quote;
            if quoted {
                pos += 1;
                while pos < n {
                    if bytes[pos] == quote {
                        if pos + 1 < n && bytes[pos + 1] == quote {
                            pos += 2;
                            continue;
                        }
                        pos += 1;
                        break;
                    }
                    pos += 1;
                }
            }
            // Unquoted content, or whatever stray text follows a closing
            // quote, up to the next delimiter or line break.
            while pos < n && bytes[pos] != delimiter && bytes[pos] != b'\n' && bytes[pos] != b'\r' {
                pos += 1;
            }
            cells.push(Cell {
                range: cell_start..pos,
                quoted,
            });
            if pos < n && bytes[pos] == delimiter {
                pos += 1;
            } else {
                break;
            }
        }
        rows.push(Row {
            range: row_start..pos,
            cells,
        });
        if pos >= n {
            break;
        }
        if bytes[pos] == b'\r' {
            pos += 1;
            if pos < n && bytes[pos] == b'\n' {
                pos += 1;
            }
        } else {
            pos += 1;
        }
        if pos >= n {
            break;
        }
    }
    Table { rows }
}

/// `value` as one cell. Quoted only when it holds the delimiter, a quote,
/// a line break or a leading or trailing space; quotes inside are doubled.
pub fn encode_cell(value: &str, dialect: &Dialect) -> String {
    let delimiter = dialect.delimiter;
    let needs_quotes = value
        .bytes()
        .any(|b| b == delimiter || b == dialect.quote || b == b'\n' || b == b'\r')
        || value.starts_with(' ')
        || value.ends_with(' ');
    if !needs_quotes {
        return value.to_string();
    }
    let quote = dialect.quote as char;
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for ch in value.chars() {
        if ch == quote {
            out.push(quote);
        }
        out.push(ch);
    }
    out.push(quote);
    out
}

/// One record without its line ending.
pub fn row_text(values: &[&str], dialect: &Dialect) -> String {
    let delimiter = dialect.delimiter as char;
    let mut out = String::new();
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push(delimiter);
        }
        out.push_str(&encode_cell(value, dialect));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(text: &str, dialect: &Dialect) -> Vec<Vec<String>> {
        parse(text, dialect)
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| Table::cell_text(text, cell).into_owned())
                    .collect()
            })
            .collect()
    }

    fn rows(cells: &[&[&str]]) -> Vec<Vec<String>> {
        cells
            .iter()
            .map(|row| row.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    fn assert_boundaries(text: &str, table: &Table) {
        for row in &table.rows {
            assert!(text.is_char_boundary(row.range.start));
            assert!(text.is_char_boundary(row.range.end));
            for cell in &row.cells {
                assert!(text.is_char_boundary(cell.range.start));
                assert!(text.is_char_boundary(cell.range.end));
                assert!(cell.range.start >= row.range.start);
                assert!(cell.range.end <= row.range.end);
            }
        }
    }

    #[test]
    fn detects_each_delimiter() {
        assert_eq!(Dialect::detect("a,b,c\n1,2,3\n4,5,6\n").delimiter, b',');
        assert_eq!(Dialect::detect("a;b;c\n1;2;3\n4;5;6\n").delimiter, b';');
        assert_eq!(
            Dialect::detect("a\tb\tc\n1\t2\t3\n4\t5\t6\n").delimiter,
            b'\t'
        );
        assert_eq!(Dialect::detect("a|b|c\n1|2|3\n4|5|6\n").delimiter, b'|');
        assert_eq!(Dialect::detect("").delimiter, b',');
        assert_eq!(
            Dialect::detect("one column\nno delimiter\n").delimiter,
            b','
        );
    }

    #[test]
    fn detection_ignores_delimiters_inside_quotes() {
        // Commas inside quoted cells vary per line; semicolons are steady.
        let text = "name;note\n\"Ada, B.\";\"x,y,z\"\n\"Grace\";\"one, two\"\n\"Linus\";plain\n";
        assert_eq!(Dialect::detect(text).delimiter, b';');
        // The reverse: a comma file whose quoted notes hold semicolons.
        let text = "name,note\nAda,\"a;b;c;d\"\nGrace,\"e;f\"\nLinus,g\n";
        assert_eq!(Dialect::detect(text).delimiter, b',');
    }

    #[test]
    fn detection_prefers_the_consistent_count() {
        // Two pipes on every line, commas scattered unevenly.
        let text = "a|b,c|d\n1|2|3,4,5\n6|7|8\n9,|10|11\n";
        assert_eq!(Dialect::detect(text).delimiter, b'|');
    }

    #[test]
    fn detects_line_ending_and_bom() {
        let crlf = "a,b\r\n1,2\r\n";
        let dialect = Dialect::detect(crlf);
        assert_eq!(dialect.line_ending, Ending::CrLf);
        assert_eq!(dialect.line_ending_text(), "\r\n");
        assert!(!dialect.bom);
        let table = parse(crlf, &dialect);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(&crlf[table.rows[0].range.clone()], "a,b");
        assert_eq!(&crlf[table.rows[1].range.clone()], "1,2");
        assert_eq!(values(crlf, &dialect), rows(&[&["a", "b"], &["1", "2"]]));

        let cr = "a,b\r1,2";
        assert_eq!(Dialect::detect(cr).line_ending, Ending::Cr);
        assert_eq!(
            values(cr, &Dialect::detect(cr)),
            rows(&[&["a", "b"], &["1", "2"]])
        );

        let bom = "\u{feff}x;y\n1;2\n";
        let dialect = Dialect::detect(bom);
        assert!(dialect.bom);
        assert_eq!(dialect.delimiter, b';');
        assert_eq!(dialect.line_ending, Ending::Lf);
        let table = parse(bom, &dialect);
        assert_eq!(table.rows[0].range.start, BOM.len());
        assert_eq!(Table::cell_text(bom, &table.rows[0].cells[0]), "x");
    }

    #[test]
    fn quoted_cell_with_newline_and_doubled_quote() {
        let text = "a,\"line one\nline \"\"two\"\"\",c\nd,e,f\n";
        let dialect = Dialect::detect(text);
        assert_eq!(dialect.delimiter, b',');
        let table = parse(text, &dialect);
        assert_eq!(table.rows.len(), 2);
        let cell = &table.rows[0].cells[1];
        assert!(cell.quoted);
        assert_eq!(&text[cell.range.clone()], "\"line one\nline \"\"two\"\"\"");
        assert_eq!(Table::cell_text(text, cell), "line one\nline \"two\"");
        assert_eq!(
            &text[table.rows[0].range.clone()],
            "a,\"line one\nline \"\"two\"\"\",c"
        );
        assert_eq!(
            values(text, &dialect),
            rows(&[&["a", "line one\nline \"two\"", "c"], &["d", "e", "f"]])
        );
        // Borrowed when nothing needs folding, including the empty cell.
        let plain = "\"x\",\"\"";
        let table = parse(plain, &dialect);
        assert!(matches!(
            Table::cell_text(plain, &table.rows[0].cells[0]),
            Cow::Borrowed("x")
        ));
        assert!(matches!(
            Table::cell_text(plain, &table.rows[0].cells[1]),
            Cow::Borrowed("")
        ));
    }

    #[test]
    fn unterminated_quote_runs_to_the_end() {
        let text = "a,b\nc,\"open\nd,e\n";
        let table = parse(text, &Dialect::default());
        assert_eq!(table.rows.len(), 2);
        let cell = &table.rows[1].cells[1];
        assert!(cell.quoted);
        assert_eq!(cell.range.end, text.len());
        assert_eq!(Table::cell_text(text, cell), "open\nd,e\n");
        // A lone opening quote and nothing else.
        let table = parse("\"", &Dialect::default());
        assert_eq!(table.rows.len(), 1);
        assert_eq!(Table::cell_text("\"", &table.rows[0].cells[0]), "");
    }

    #[test]
    fn ragged_rows_keep_their_width() {
        let text = "a,b,c\n1\n2,3,4,5\n";
        let table = parse(text, &Dialect::default());
        let widths: Vec<usize> = table.rows.iter().map(|r| r.cells.len()).collect();
        assert_eq!(widths, vec![3, 1, 4]);
        assert_eq!(table.column_count(), 4);
        assert!(table.cell(1, 1).is_none());
        assert_eq!(&text[table.cell(2, 3).unwrap().range.clone()], "5");
    }

    #[test]
    fn trailing_newline_empty_lines_and_empty_file() {
        let dialect = Dialect::default();
        assert_eq!(
            values("a,b\n1,2\n", &dialect),
            rows(&[&["a", "b"], &["1", "2"]])
        );
        assert_eq!(
            values("a,b\n1,2", &dialect),
            rows(&[&["a", "b"], &["1", "2"]])
        );
        assert_eq!(values("a\n\nb\n", &dialect), rows(&[&["a"], &[""], &["b"]]));
        assert_eq!(values("\n", &dialect), rows(&[&[""]]));
        assert_eq!(values("a,,\n", &dialect), rows(&[&["a", "", ""]]));
        assert_eq!(parse("", &dialect), Table::default());
        assert_eq!(Table::default().column_count(), 0);
        assert_eq!(parse("\u{feff}", &dialect).rows.len(), 0);
    }

    #[test]
    fn multibyte_text_keeps_ranges_on_char_boundaries() {
        let text = "名前,絵文字,note\n太郎,😀😀,\"日本, 東京\"\nÉlodie,🇫🇷,\"naïve \"\"café\"\"\"\n";
        let dialect = Dialect::detect(text);
        assert_eq!(dialect.delimiter, b',');
        let table = parse(text, &dialect);
        assert_boundaries(text, &table);
        assert_eq!(
            values(text, &dialect),
            rows(&[
                &["名前", "絵文字", "note"],
                &["太郎", "😀😀", "日本, 東京"],
                &["Élodie", "🇫🇷", "naïve \"café\""],
            ])
        );
        assert_eq!(&text[table.cell(1, 1).unwrap().range.clone()], "😀😀");
        // Tabs and pipes as delimiters next to multibyte content.
        for delimiter in *b"\t|" {
            let d = delimiter as char;
            let text = format!("α{d}β{d}γ\n😀{d}中文{d}z\n");
            let dialect = Dialect::detect(&text);
            assert_eq!(dialect.delimiter, delimiter);
            let table = parse(&text, &dialect);
            assert_boundaries(&text, &table);
            assert_eq!(
                values(&text, &dialect),
                rows(&[&["α", "β", "γ"], &["😀", "中文", "z"]])
            );
        }
    }

    #[test]
    fn encode_cell_round_trips_awkward_values() {
        let awkward = [
            "",
            "plain",
            "with,comma",
            "with;semicolon",
            "with\ttab",
            "with|pipe",
            "has \"quotes\"",
            "\"",
            "\"\"",
            "line\nbreak",
            "crlf\r\nbreak",
            " leading",
            "trailing ",
            " ",
            "😀,\"日本\"\n",
            "a\"\"b",
        ];
        for delimiter in CANDIDATES {
            let dialect = Dialect::with_delimiter(delimiter);
            // An empty value alone is an empty file, which has no rows;
            // the row round trip below still covers it.
            for value in awkward.iter().copied().filter(|v| !v.is_empty()) {
                let encoded = encode_cell(value, &dialect);
                let table = parse(&encoded, &dialect);
                assert_eq!(table.rows.len(), 1, "{value:?} with {delimiter:?}");
                assert_eq!(table.rows[0].cells.len(), 1, "{value:?} with {delimiter:?}");
                assert_eq!(
                    Table::cell_text(&encoded, &table.rows[0].cells[0]),
                    value,
                    "{value:?} with {delimiter:?}"
                );
            }
            let line = row_text(&awkward, &dialect);
            let table = parse(&line, &dialect);
            assert_eq!(table.rows.len(), 1);
            let back: Vec<String> = table.rows[0]
                .cells
                .iter()
                .map(|c| Table::cell_text(&line, c).into_owned())
                .collect();
            assert_eq!(back, awkward);
        }
        assert_eq!(encode_cell("plain", &Dialect::default()), "plain");
        assert_eq!(encode_cell("a\"b", &Dialect::default()), "\"a\"\"b\"");
        assert_eq!(encode_cell("a;b", &Dialect::default()), "a;b");
        assert_eq!(
            encode_cell("a;b", &Dialect::with_delimiter(b';')),
            "\"a;b\""
        );
        assert_eq!(
            row_text(&["a", "b,c", ""], &Dialect::default()),
            "a,\"b,c\","
        );
        assert_eq!(row_text(&[], &Dialect::default()), "");
    }

    #[test]
    fn parses_a_hundred_thousand_rows() {
        let mut text = String::with_capacity(6_000_000);
        text.push_str("id,name,city,note,amount\n");
        for i in 0..100_000 {
            text.push_str(&format!(
                "{i},Person {i},\"Lisbon, PT\",\"says \"\"hi\"\" {i}\",{}.{:02}\n",
                i * 7,
                i % 100
            ));
        }
        let start = std::time::Instant::now();
        let dialect = Dialect::detect(&text);
        let detect = start.elapsed();
        let start = std::time::Instant::now();
        let table = parse(&text, &dialect);
        let parsed = start.elapsed();
        eprintln!(
            "csv: {} bytes, detect {detect:?}, parse {parsed:?}, debug={}",
            text.len(),
            cfg!(debug_assertions)
        );
        assert_eq!(dialect.delimiter, b',');
        assert_eq!(table.rows.len(), 100_001);
        assert_eq!(table.column_count(), 5);
        assert_eq!(
            Table::cell_text(&text, table.cell(100_000, 3).unwrap()),
            "says \"hi\" 99999"
        );
        assert_eq!(
            &text[table.rows[100_000].range.clone()].chars().last(),
            &Some('9')
        );
    }
}
