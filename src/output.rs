//! Output formats: aligned table, JSON Lines, and CSV.
//!
//! Every writer implements the same two-call protocol: [`Writer::write`] for
//! each row and [`Writer::finish`] once at the end. The table writer buffers by
//! default so it can size its columns, and switches to an incremental mode for
//! `--follow`, where there is no "end" to wait for.

use std::io::{self, Write};

use serde_json::Value;

use crate::engine::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    Csv,
}

impl Format {
    pub fn parse(name: &str) -> Option<Format> {
        match name.to_ascii_lowercase().as_str() {
            "table" => Some(Format::Table),
            "json" | "jsonl" | "ndjson" => Some(Format::Json),
            "csv" => Some(Format::Csv),
            _ => None,
        }
    }
}

pub trait Writer {
    fn write(&mut self, row: &Row) -> io::Result<()>;
    fn finish(&mut self) -> io::Result<()>;
}

/// Build a writer for a format.
///
/// `columns` is the fixed column list when the query names its fields; `None`
/// means `select *`, where columns are discovered from the rows. `incremental`
/// asks the table writer not to buffer.
pub fn writer<W: Write + 'static>(
    format: Format,
    out: W,
    columns: Option<Vec<String>>,
    incremental: bool,
) -> Box<dyn Writer> {
    match format {
        Format::Table => Box::new(TableWriter::new(out, columns, incremental)),
        Format::Json => Box::new(JsonWriter { out }),
        Format::Csv => Box::new(CsvWriter {
            out,
            columns,
            header_written: false,
            rows: Vec::new(),
        }),
    }
}

/// Render a value for a text cell. Strings are printed bare so that `msg`
/// reads like a log message rather than a JSON string.
pub fn cell_text(v: Option<&Value>) -> String {
    match v {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".to_string(),
        Some(other) => other.to_string(),
    }
}

// --- JSON Lines ----------------------------------------------------------

struct JsonWriter<W: Write> {
    out: W,
}

impl<W: Write> Writer for JsonWriter<W> {
    fn write(&mut self, row: &Row) -> io::Result<()> {
        let text = serde_json::to_string(row).map_err(io::Error::other)?;
        writeln!(self.out, "{text}")
    }
    fn finish(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

// --- CSV -----------------------------------------------------------------

struct CsvWriter<W: Write> {
    out: W,
    columns: Option<Vec<String>>,
    header_written: bool,
    /// Buffered rows, used only when the column set has to be discovered.
    rows: Vec<Row>,
}

impl<W: Write> CsvWriter<W> {
    fn emit(&mut self, cols: &[String], row: &Row) -> io::Result<()> {
        let cells: Vec<String> = cols.iter().map(|c| cell_text(row.get(c))).collect();
        let line: Vec<String> = cells.iter().map(|c| csv_escape(c)).collect();
        writeln!(self.out, "{}", line.join(","))
    }
}

impl<W: Write> Writer for CsvWriter<W> {
    fn write(&mut self, row: &Row) -> io::Result<()> {
        match self.columns.clone() {
            Some(cols) => {
                if !self.header_written {
                    let header: Vec<String> = cols.iter().map(|c| csv_escape(c)).collect();
                    writeln!(self.out, "{}", header.join(","))?;
                    self.header_written = true;
                }
                self.emit(&cols, row)
            }
            None => {
                // `select *`: hold rows so the header covers every key seen.
                self.rows.push(row.clone());
                Ok(())
            }
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.columns.is_none() {
            let rows = std::mem::take(&mut self.rows);
            let cols = discover_columns(&rows);
            if !cols.is_empty() {
                let header: Vec<String> = cols.iter().map(|c| csv_escape(c)).collect();
                writeln!(self.out, "{}", header.join(","))?;
            }
            for row in &rows {
                self.emit(&cols, row)?;
            }
        }
        self.out.flush()
    }
}

/// RFC 4180 quoting: wrap in double quotes when the cell contains a comma,
/// quote, or newline, and double any embedded quotes.
pub fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// --- Table ---------------------------------------------------------------

struct TableWriter<W: Write> {
    out: W,
    columns: Option<Vec<String>>,
    incremental: bool,
    rows: Vec<Row>,
    /// Column widths and header state, used in incremental mode.
    live_cols: Vec<String>,
    widths: Vec<usize>,
    header_written: bool,
}

impl<W: Write> TableWriter<W> {
    fn new(out: W, columns: Option<Vec<String>>, incremental: bool) -> Self {
        let live_cols = columns.clone().unwrap_or_default();
        let widths = live_cols.iter().map(|c| display_width(c)).collect();
        TableWriter {
            out,
            columns,
            incremental,
            rows: Vec::new(),
            live_cols,
            widths,
            header_written: false,
        }
    }

    fn write_header(&mut self) -> io::Result<()> {
        let cells: Vec<String> = self
            .live_cols
            .iter()
            .zip(&self.widths)
            .map(|(c, w)| pad(c, *w))
            .collect();
        writeln!(self.out, "{}", cells.join("  ").trim_end())?;
        let rule: Vec<String> = self.widths.iter().map(|w| "-".repeat(*w)).collect();
        writeln!(self.out, "{}", rule.join("  "))
    }

    fn write_row(&mut self, row: &Row) -> io::Result<()> {
        let cells: Vec<String> = self
            .live_cols
            .iter()
            .zip(&self.widths)
            .map(|(c, w)| pad(&cell_text(row.get(c)), *w))
            .collect();
        writeln!(self.out, "{}", cells.join("  ").trim_end())
    }
}

impl<W: Write> Writer for TableWriter<W> {
    fn write(&mut self, row: &Row) -> io::Result<()> {
        if !self.incremental {
            self.rows.push(row.clone());
            return Ok(());
        }

        // Incremental mode: columns and widths grow as rows arrive. When
        // either changes, reprint the header so the output stays readable.
        let mut layout_changed = false;
        if self.columns.is_none() {
            for key in row.keys() {
                if !self.live_cols.iter().any(|c| c == key) {
                    self.live_cols.push(key.clone());
                    self.widths.push(display_width(key));
                    layout_changed = true;
                }
            }
        }
        for (i, col) in self.live_cols.iter().enumerate() {
            let w = display_width(&cell_text(row.get(col)));
            if w > self.widths[i] {
                self.widths[i] = w;
                layout_changed = true;
            }
        }

        if !self.header_written || layout_changed {
            self.write_header()?;
            self.header_written = true;
        }
        self.write_row(row)?;
        self.out.flush()
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.incremental {
            return self.out.flush();
        }
        let rows = std::mem::take(&mut self.rows);
        self.live_cols = match &self.columns {
            Some(c) => c.clone(),
            None => discover_columns(&rows),
        };
        if self.live_cols.is_empty() {
            return self.out.flush();
        }
        self.widths = self
            .live_cols
            .iter()
            .map(|c| {
                rows.iter()
                    .map(|r| display_width(&cell_text(r.get(c))))
                    .chain(std::iter::once(display_width(c)))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        self.write_header()?;
        for row in &rows {
            self.write_row(row)?;
        }
        self.out.flush()
    }
}

/// Union of the keys of all rows, in first-seen order.
fn discover_columns(rows: &[Row]) -> Vec<String> {
    let mut cols: Vec<String> = Vec::new();
    for row in rows {
        for key in row.keys() {
            if !cols.iter().any(|c| c == key) {
                cols.push(key.clone());
            }
        }
    }
    cols
}

/// Width in characters. Control characters are replaced before this is called,
/// so a char count is a good enough approximation for alignment.
fn display_width(s: &str) -> usize {
    s.chars().count()
}

fn pad(s: &str, width: usize) -> String {
    let mut out = sanitize(s);
    let w = display_width(&out);
    if w < width {
        out.push_str(&" ".repeat(width - w));
    }
    out
}

/// Newlines and tabs inside a value would break the alignment, so they are
/// shown escaped.
fn sanitize(s: &str) -> String {
    if s.contains(['\n', '\r', '\t']) {
        s.replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Row {
        v.as_object().unwrap().clone()
    }

    fn render(format: Format, columns: Option<Vec<String>>, rows: &[Value]) -> String {
        // The writer owns its sink, so render into a shared buffer via a Vec
        // and pull it back out afterwards.
        struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut w = writer(format, Shared(sink.clone()), columns, false);
        for r in rows {
            w.write(&row(r.clone())).unwrap();
        }
        w.finish().unwrap();
        let out = sink.borrow().clone();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn format_names_parse() {
        assert_eq!(Format::parse("table"), Some(Format::Table));
        assert_eq!(Format::parse("JSON"), Some(Format::Json));
        assert_eq!(Format::parse("ndjson"), Some(Format::Json));
        assert_eq!(Format::parse("csv"), Some(Format::Csv));
        assert_eq!(Format::parse("yaml"), None);
    }

    #[test]
    fn json_output_is_one_object_per_line() {
        let out = render(
            Format::Json,
            Some(vec!["a".into()]),
            &[json!({"a": 1}), json!({"a": "x"})],
        );
        assert_eq!(out, "{\"a\":1}\n{\"a\":\"x\"}\n");
    }

    #[test]
    fn csv_writes_a_header_then_rows() {
        let out = render(
            Format::Csv,
            Some(vec!["level".into(), "msg".into()]),
            &[json!({"level": "error", "msg": "boom"})],
        );
        assert_eq!(out, "level,msg\nerror,boom\n");
    }

    #[test]
    fn csv_quotes_when_needed() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_escape("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn csv_leaves_missing_fields_empty() {
        let out = render(
            Format::Csv,
            Some(vec!["a".into(), "b".into()]),
            &[json!({"a": 1})],
        );
        assert_eq!(out, "a,b\n1,\n");
    }

    #[test]
    fn csv_star_header_is_the_union_of_keys() {
        let out = render(Format::Csv, None, &[json!({"a": 1}), json!({"b": 2})]);
        assert_eq!(out, "a,b\n1,\n,2\n");
    }

    #[test]
    fn table_aligns_columns_and_underlines_the_header() {
        let out = render(
            Format::Table,
            Some(vec!["level".into(), "msg".into()]),
            &[
                json!({"level": "error", "msg": "upstream timeout"}),
                json!({"level": "info", "msg": "ok"}),
            ],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "level  msg");
        assert_eq!(lines[1], "-----  ----------------");
        assert_eq!(lines[2], "error  upstream timeout");
        assert_eq!(lines[3], "info   ok");
    }

    #[test]
    fn table_widens_to_fit_the_header() {
        let out = render(
            Format::Table,
            Some(vec!["duration_ms".into()]),
            &[json!({"duration_ms": 3})],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "duration_ms");
        assert_eq!(lines[2], "3");
    }

    #[test]
    fn table_star_discovers_columns_in_first_seen_order() {
        let out = render(
            Format::Table,
            None,
            &[json!({"b": 1, "a": 2}), json!({"c": 3})],
        );
        assert_eq!(out.lines().next().unwrap(), "b  a  c");
    }

    #[test]
    fn table_escapes_embedded_newlines() {
        let out = render(
            Format::Table,
            Some(vec!["msg".into()]),
            &[json!({"msg": "a\nb"})],
        );
        assert_eq!(out.lines().count(), 3);
        assert_eq!(out.lines().nth(2).unwrap(), "a\\nb");
    }

    #[test]
    fn empty_output_produces_nothing() {
        assert_eq!(render(Format::Table, None, &[]), "");
        assert_eq!(render(Format::Csv, None, &[]), "");
        assert_eq!(render(Format::Json, None, &[]), "");
    }

    #[test]
    fn cell_text_renders_each_json_type() {
        assert_eq!(cell_text(None), "");
        assert_eq!(cell_text(Some(&json!("hi"))), "hi");
        assert_eq!(cell_text(Some(&json!(3))), "3");
        assert_eq!(cell_text(Some(&json!(1.5))), "1.5");
        assert_eq!(cell_text(Some(&json!(true))), "true");
        assert_eq!(cell_text(Some(&Value::Null)), "null");
        assert_eq!(cell_text(Some(&json!(["a"]))), "[\"a\"]");
        assert_eq!(cell_text(Some(&json!({"k": 1}))), "{\"k\":1}");
    }

    #[test]
    fn incremental_table_prints_rows_as_they_arrive() {
        let sink = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut w = writer(
            Format::Table,
            Shared(sink.clone()),
            Some(vec!["a".into()]),
            true,
        );
        w.write(&row(json!({"a": 1}))).unwrap();
        let after_first = String::from_utf8(sink.borrow().clone()).unwrap();
        assert_eq!(after_first, "a\n-\n1\n");

        w.write(&row(json!({"a": "wide value"}))).unwrap();
        w.finish().unwrap();
        let all = String::from_utf8(sink.borrow().clone()).unwrap();
        // The wider row forces a fresh header.
        assert_eq!(
            all.lines().collect::<Vec<_>>(),
            vec!["a", "-", "1", "a", "----------", "wide value"]
        );
    }
}
