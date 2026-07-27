//! logquery — SQL-like queries over structured log files.

mod ast;
mod engine;
mod eval;
mod lexer;
mod output;
mod parser;
mod record;

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::engine::Engine;
use crate::output::Format;
use crate::record::parse_line;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
logquery — run SQL-like queries over JSON Lines and logfmt logs

USAGE:
    logquery [OPTIONS] <QUERY> [FILE...]

    With no FILE, or with `-`, the query reads standard input.

OPTIONS:
    -f, --format <FORMAT>  table (default), json, or csv
    -F, --follow           keep reading the file as it grows, like `tail -f`
    -q, --quiet            do not report malformed lines
    -h, --help             print this help
    -V, --version          print the version

QUERY:
    select <* | field[, field...]>
      [ where <expression> ]
      [ order by <field> [asc|desc] ]
      [ limit <n> ]

EXAMPLES:
    logquery 'select level, msg where level = \"error\"' app.log
    logquery 'select * where duration_ms > 100 order by duration_ms desc limit 20' app.log
    kubectl logs pod | logquery -f json 'select ts, msg where msg like \"%timeout%\"'
";

/// Exit code for a query the user wrote incorrectly, kept distinct from a
/// runtime failure such as a missing file.
const EXIT_QUERY_ERROR: u8 = 2;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("logquery: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug)]
struct Options {
    query: String,
    inputs: Vec<Input>,
    format: Format,
    follow: bool,
    quiet: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Input {
    Stdin,
    File(PathBuf),
}

fn run() -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args)? {
        Some(opts) => opts,
        // --help or --version already printed.
        None => return Ok(ExitCode::SUCCESS),
    };

    let query = match parser::parse(&opts.query) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("logquery: {e}");
            return Ok(ExitCode::from(EXIT_QUERY_ERROR));
        }
    };

    if opts.follow && query.order_by.is_some() {
        bail!("--follow cannot be combined with `order by`: sorting needs the whole input");
    }

    let mut engine = Engine::new(&query);
    let mut writer = output::writer(
        opts.format,
        io::BufWriter::new(io::stdout()),
        engine.columns(),
        opts.follow,
    );

    let mut skipped = 0usize;
    let mut stopped_early = false;

    for input in &opts.inputs {
        let reader: Box<dyn BufRead> = match input {
            Input::Stdin => Box::new(BufReader::new(io::stdin())),
            Input::File(p) => Box::new(BufReader::new(
                File::open(p).with_context(|| format!("cannot open {}", p.display()))?,
            )),
        };
        match pump(reader, &mut engine, writer.as_mut(), &mut skipped) {
            Ok(Done::LimitReached) => {
                stopped_early = true;
                break;
            }
            Ok(Done::Eof) => {}
            Err(e) if is_broken_pipe(&e) => return Ok(ExitCode::SUCCESS),
            Err(e) => return Err(e).context("reading input"),
        }
    }

    if opts.follow && !stopped_early {
        // `parse_args` guarantees exactly one file when following.
        if let Some(Input::File(path)) = opts.inputs.last() {
            match follow(path, &mut engine, writer.as_mut(), &mut skipped) {
                Ok(()) => {}
                Err(e) if is_broken_pipe(&e) => return Ok(ExitCode::SUCCESS),
                Err(e) => return Err(e).context("following input"),
            }
        }
    }

    for row in engine.finish() {
        match writer.write(&row) {
            Ok(()) => {}
            Err(e) if is_broken_pipe(&e) => return Ok(ExitCode::SUCCESS),
            Err(e) => return Err(e).context("writing output"),
        }
    }
    match writer.finish() {
        Ok(()) => {}
        Err(e) if is_broken_pipe(&e) => return Ok(ExitCode::SUCCESS),
        Err(e) => return Err(e).context("writing output"),
    }

    if skipped > 0 && !opts.quiet {
        eprintln!(
            "logquery: skipped {skipped} malformed line{}",
            if skipped == 1 { "" } else { "s" }
        );
    }

    Ok(ExitCode::SUCCESS)
}

enum Done {
    Eof,
    LimitReached,
}

/// Read every line from `reader`, feeding records to the engine.
fn pump(
    mut reader: Box<dyn BufRead>,
    engine: &mut Engine<'_>,
    writer: &mut dyn output::Writer,
    skipped: &mut usize,
) -> io::Result<Done> {
    let mut raw = Vec::new();
    loop {
        if engine.is_done() {
            return Ok(Done::LimitReached);
        }
        raw.clear();
        // Read bytes rather than a String so that a log file containing one
        // invalid UTF-8 line costs a skipped line instead of the whole run.
        if read_until_newline(&mut *reader, &mut raw)? == 0 {
            return Ok(Done::Eof);
        }
        match std::str::from_utf8(&raw) {
            Ok(s) => {
                if !handle_line(s, engine, writer, skipped)? {
                    return Ok(Done::LimitReached);
                }
            }
            Err(_) => *skipped += 1,
        }
    }
}

/// Read one line, stripping `\n` and a preceding `\r`. Returns the number of
/// bytes consumed, so 0 means end of input.
fn read_until_newline(reader: &mut dyn BufRead, buf: &mut Vec<u8>) -> io::Result<usize> {
    let n = reader.read_until(b'\n', buf)?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    Ok(n)
}

/// Parse and process one line. Returns false once the engine is satisfied.
fn handle_line(
    line: &str,
    engine: &mut Engine<'_>,
    writer: &mut dyn output::Writer,
    skipped: &mut usize,
) -> io::Result<bool> {
    match parse_line(line) {
        Some((rec, _)) => {
            if let Some(row) = engine.accept(&rec) {
                writer.write(&row)?;
            }
        }
        None => {
            // Blank lines are empty, not malformed.
            if !line.trim().is_empty() {
                *skipped += 1;
            }
        }
    }
    Ok(!engine.is_done())
}

/// Watch a file for appended data, like `tail -f`.
///
/// A rotated log is detected by the file shrinking below the current offset or
/// by the path pointing at a smaller file than the open handle; either way
/// reading restarts from the beginning of the new file.
fn follow(
    path: &Path,
    engine: &mut Engine<'_>,
    writer: &mut dyn output::Writer,
    skipped: &mut usize,
) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut offset = file.seek(SeekFrom::End(0))?;
    // Holds a trailing fragment when the producer has not finished the line.
    let mut pending: Vec<u8> = Vec::new();

    loop {
        if engine.is_done() {
            return Ok(());
        }
        let len = match file.metadata() {
            Ok(m) => m.len(),
            // The file can vanish mid-rotation; wait for it to come back.
            Err(_) => {
                std::thread::sleep(Duration::from_millis(POLL_MS));
                continue;
            }
        };

        if len < offset {
            file = File::open(path)?;
            offset = 0;
            pending.clear();
        } else if len == offset {
            std::thread::sleep(Duration::from_millis(POLL_MS));
            if let Ok(fresh) = File::open(path) {
                if let Ok(meta) = fresh.metadata() {
                    if meta.len() < offset {
                        file = fresh;
                        offset = 0;
                        pending.clear();
                    }
                }
            }
            continue;
        }

        file.seek(SeekFrom::Start(offset))?;
        let mut chunk = Vec::new();
        let read = file.read_to_end(&mut chunk)?;
        offset += read as u64;
        pending.extend_from_slice(&chunk);

        // Process whole lines only; keep any tail for the next round.
        let mut start = 0;
        while let Some(idx) = pending[start..].iter().position(|&b| b == b'\n') {
            let end = start + idx;
            let mut raw = &pending[start..end];
            if raw.last() == Some(&b'\r') {
                raw = &raw[..raw.len() - 1];
            }
            match std::str::from_utf8(raw) {
                Ok(s) => {
                    if !handle_line(s, engine, writer, skipped)? {
                        return Ok(());
                    }
                }
                Err(_) => *skipped += 1,
            }
            start = end + 1;
        }
        pending.drain(..start);
    }
}

const POLL_MS: u64 = 200;

fn is_broken_pipe(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::BrokenPipe
}

/// Parse the command line. `Ok(None)` means help or version was printed and the
/// program should exit successfully.
fn parse_args(args: &[String]) -> Result<Option<Options>> {
    let mut query: Option<String> = None;
    let mut inputs: Vec<Input> = Vec::new();
    let mut format = Format::Table;
    let mut follow = false;
    let mut quiet = false;
    let mut no_more_flags = false;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if no_more_flags || !arg.starts_with('-') || arg == "-" {
            if query.is_none() {
                query = Some(arg.to_string());
            } else if arg == "-" {
                inputs.push(Input::Stdin);
            } else {
                inputs.push(Input::File(PathBuf::from(arg)));
            }
            i += 1;
            continue;
        }

        match arg {
            "--" => no_more_flags = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                io::stdout().flush().ok();
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("logquery {VERSION}");
                return Ok(None);
            }
            "-F" | "--follow" => follow = true,
            "-q" | "--quiet" => quiet = true,
            "-f" | "--format" => {
                i += 1;
                let Some(name) = args.get(i) else {
                    bail!("{arg} needs a value: table, json, or csv");
                };
                format = Format::parse(name)
                    .with_context(|| format!("unknown format `{name}`; use table, json, or csv"))?;
            }
            other => {
                if let Some(name) = other.strip_prefix("--format=") {
                    format = Format::parse(name).with_context(|| {
                        format!("unknown format `{name}`; use table, json, or csv")
                    })?;
                } else {
                    bail!("unknown option `{other}`\n\n{USAGE}");
                }
            }
        }
        i += 1;
    }

    let Some(query) = query else {
        bail!("no query given\n\n{USAGE}");
    };

    if inputs.is_empty() {
        inputs.push(Input::Stdin);
    }

    if follow {
        if inputs.len() != 1 {
            bail!("--follow takes exactly one file");
        }
        if inputs[0] == Input::Stdin {
            bail!("--follow needs a file; standard input is already a stream");
        }
    }

    Ok(Some(Options {
        query,
        inputs,
        format,
        follow,
        quiet,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn query_only_reads_stdin() {
        let o = parse_args(&args(&["select *"])).unwrap().unwrap();
        assert_eq!(o.query, "select *");
        assert_eq!(o.inputs, vec![Input::Stdin]);
        assert_eq!(o.format, Format::Table);
        assert!(!o.follow);
        assert!(!o.quiet);
    }

    #[test]
    fn files_follow_the_query() {
        let o = parse_args(&args(&["select *", "a.log", "b.log"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            o.inputs,
            vec![Input::File("a.log".into()), Input::File("b.log".into())]
        );
    }

    #[test]
    fn dash_means_stdin() {
        let o = parse_args(&args(&["select *", "-"])).unwrap().unwrap();
        assert_eq!(o.inputs, vec![Input::Stdin]);
    }

    #[test]
    fn format_accepts_every_spelling() {
        for a in [
            args(&["-f", "csv", "select *"]),
            args(&["--format", "csv", "select *"]),
            args(&["--format=csv", "select *"]),
        ] {
            assert_eq!(parse_args(&a).unwrap().unwrap().format, Format::Csv);
        }
    }

    #[test]
    fn flags_may_appear_after_the_query() {
        let o = parse_args(&args(&["select *", "app.log", "-f", "json", "-q"]))
            .unwrap()
            .unwrap();
        assert_eq!(o.format, Format::Json);
        assert!(o.quiet);
        assert_eq!(o.inputs, vec![Input::File("app.log".into())]);
    }

    #[test]
    fn double_dash_stops_flag_parsing() {
        let o = parse_args(&args(&["select *", "--", "-weird-name.log"]))
            .unwrap()
            .unwrap();
        assert_eq!(o.inputs, vec![Input::File("-weird-name.log".into())]);
    }

    #[test]
    fn unknown_format_is_rejected() {
        let e = parse_args(&args(&["-f", "yaml", "select *"])).unwrap_err();
        assert!(format!("{e:#}").contains("unknown format"), "{e:#}");
    }

    #[test]
    fn format_without_a_value_is_rejected() {
        assert!(parse_args(&args(&["select *", "-f"])).is_err());
    }

    #[test]
    fn unknown_option_is_rejected() {
        let e = parse_args(&args(&["--nope", "select *"])).unwrap_err();
        assert!(e.to_string().contains("unknown option"), "{e}");
    }

    #[test]
    fn missing_query_is_rejected() {
        let e = parse_args(&[]).unwrap_err();
        assert!(e.to_string().contains("no query"), "{e}");
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(parse_args(&args(&["--help"])).unwrap().is_none());
        assert!(parse_args(&args(&["-V"])).unwrap().is_none());
    }

    #[test]
    fn follow_requires_exactly_one_file() {
        assert!(parse_args(&args(&["-F", "select *"])).is_err());
        assert!(parse_args(&args(&["-F", "select *", "a.log", "b.log"])).is_err());
        let o = parse_args(&args(&["-F", "select *", "a.log"]))
            .unwrap()
            .unwrap();
        assert!(o.follow);
    }

    #[test]
    fn read_until_newline_strips_line_endings() {
        let data = b"a\r\nb\nc";
        let mut reader: Box<dyn BufRead> = Box::new(&data[..]);
        let mut buf = Vec::new();
        assert!(read_until_newline(&mut *reader, &mut buf).unwrap() > 0);
        assert_eq!(buf, b"a");
        buf.clear();
        read_until_newline(&mut *reader, &mut buf).unwrap();
        assert_eq!(buf, b"b");
        buf.clear();
        read_until_newline(&mut *reader, &mut buf).unwrap();
        assert_eq!(buf, b"c");
        buf.clear();
        assert_eq!(read_until_newline(&mut *reader, &mut buf).unwrap(), 0);
    }
}
