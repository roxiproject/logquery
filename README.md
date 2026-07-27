# logquery

Run SQL-like queries over structured log files — JSON Lines and logfmt — from the command line.

```console
$ logquery 'select level, msg, duration_ms where level = "error" and duration_ms > 100' examples/app.log
level  msg               duration_ms
-----  ----------------  -----------
error  upstream timeout  3001
error  upstream timeout  3002
error  payment declined  220
```

## Why not grep

`grep` searches text. Structured logs are not text — they are records that happen to be serialised one per line. That mismatch shows up the moment a question involves anything but a substring:

- **Numeric comparisons.** "Requests slower than 100 ms" is not a pattern. `grep 'duration_ms":[1-9][0-9][0-9]'` is a lie that misses `1042` and matches `"duration_ms":100` inside a different field.
- **Field scoping.** `grep error` matches the level `error`, a message containing "error", a URL path `/error-report`, and a hostname. `where level = "error"` matches the level.
- **Nested data.** `user.id = 1042` requires knowing where `id` lives. Text search cannot tell `{"user":{"id":1042}}` from `{"actor":{"id":1042}}`.
- **Absence.** "Lines with no `user` field" has no textual form. `where not user` does.
- **Projection and ordering.** Reading three fields out of a forty-field JSON blob, sorted by latency, is a job for `select` and `order by`, not for `grep | jq | sort -t`.

logquery keeps the shell ergonomics — pipes, stdin, `tail -f` — and adds the parts of SQL that actually matter for logs. It is a single binary with no runtime and no index to build.

## Install

```sh
cargo install --path .
```

Or build in place:

```sh
cargo build --release   # ./target/release/logquery
```

Requires Rust 1.74 or newer (2021 edition).

## Usage

```
logquery [OPTIONS] <QUERY> [FILE...]
```

With no `FILE`, or with `-`, the query reads standard input. Multiple files are read in order, as one stream.

| Option | Meaning |
| --- | --- |
| `-f`, `--format <FORMAT>` | `table` (default), `json`, or `csv` |
| `-F`, `--follow` | keep reading the file as it grows, like `tail -f` |
| `-q`, `--quiet` | do not report malformed lines |
| `-h`, `--help` | print help |
| `-V`, `--version` | print the version |

Exit codes: `0` success, `1` runtime failure (missing file, I/O error), `2` the query did not parse.

## Query grammar

```
query      := "select" selection [ "where" expr ]
                                 [ "order" "by" field [ "asc" | "desc" ] ]
                                 [ "limit" number ]

selection  := "*" | field { "," field }

expr       := or_expr
or_expr    := and_expr { "or" and_expr }
and_expr   := not_expr { "and" not_expr }
not_expr   := "not" not_expr | predicate
predicate  := primary [ ( cmp_op primary ) | ( ["not"] "like" primary ) ]
primary    := "(" expr ")" | field | literal

cmp_op     := "=" | "!=" | "<" | "<=" | ">" | ">="
field      := name { "." name }
literal    := string | number | "true" | "false" | "null"
```

Precedence, loosest first: `or`, `and`, `not`, then comparisons. So `not a = 1 and b = 2` is `(not (a = 1)) and (b = 2)`. Parentheses override.

**Keywords** (`select`, `where`, `order`, `by`, `asc`, `desc`, `limit`, `and`, `or`, `not`, `like`, `true`, `false`, `null`) are case-insensitive. Field names are case-sensitive, because log fields are.

**Fields** are dotted paths into nested data: `user.id`, `http.status`, `req.headers.host`. A numeric segment indexes an array: `items.0.sku`. The path as written becomes the output column name.

**Literals.** Strings use `'` or `"` and understand `\n`, `\t`, `\r`, `\0`, `\\`, `\'`, `\"` and `\%`. Numbers are decimal with an optional sign, fraction, and exponent.

**Clauses are optional but ordered.** `where` before `order by` before `limit`.

**`select`** takes `*` or a comma-separated field list. `*` emits every field of the record; a field list emits exactly those, in the order you wrote them. A selected field the record does not have becomes an empty cell (and is simply absent in JSON output).

**`where`** takes a boolean expression. A bare field or literal is also a valid condition, evaluated for truthiness — `where cached` and `where not err` are the idiomatic presence tests.

**`like`** matches a whole string against a pattern where `%` stands for any run of characters, including none. It is anchored at both ends, so `like "error"` is equality and `like "%error%"` is a substring search. Matching is case-sensitive. `\%` matches a literal percent sign. `not like` negates.

**`order by`** sorts on any field, whether or not it is selected. Records missing that field sort last in both directions. Ties keep their input order. `order by` requires reading all input, so it cannot be combined with `--follow`.

**`limit N`** caps the output. Without `order by`, logquery stops reading as soon as it has `N` rows.

## Type coercion rules

Logs are not a typed database: a field may be a number in one line and a string in the next, or missing entirely. These are the rules, and they are what the test suite pins down.

### Missing fields

A path that does not resolve is **missing**, which is not the same as a JSON `null`.

**Every comparison involving a missing operand is false — including `!=`.** `where code != 500` does not match a line that has no `code`. This never errors and never panics; it simply does not match.

To ask about presence, use the bare-field form:

| Query | True when |
| --- | --- |
| `where err` | `err` is present and truthy |
| `where not err` | `err` is missing, `null`, or falsy |

### Comparisons

Both sides are reduced to a comparable pair:

| Left | Right | Rule |
| --- | --- | --- |
| number | number | numeric |
| string | string | lexicographic by code point |
| bool | bool | `false < true` |
| null | null | equal; `<`, `<=`, `>`, `>=` are false |
| string | number | the string is parsed as a number; `"500" = 500` is true, `"error" = 500` is not |
| bool | number | the bool becomes `1` or `0` |
| bool | string | the string is read as `true`/`false`, case-insensitively |
| null | anything else | incomparable |
| array/object | anything | equality only, structural; ordering is incomparable |

An **incomparable** pair makes `=` false, `!=` true, and every ordering operator false. That asymmetry is deliberate: two values that cannot be compared are certainly not equal.

Two details worth knowing:

- **Two strings never coerce to numbers.** `"10" < "9"` is true, because both sides are strings and text ordering applies. Coercion only happens when one side really is a number.
- **Textual `inf`, `-inf` and `nan` are not numbers.** A message reading `inf` will not compare greater than `0`.

### Truthiness

For a bare field or literal used as a condition:

| Value | Truthy |
| --- | --- |
| missing | no |
| `null` | no |
| `false` / `true` | as written |
| number | yes unless `0` |
| string | yes unless empty |
| array / object | yes unless empty |

### Input parsing

Each line is classified on its own:

- A line starting with `{` is parsed as JSON. If it is not a valid JSON object, the line is skipped.
- Anything else is parsed as logfmt: whitespace-separated `key=value` pairs, values optionally double-quoted. Inside quotes, `\"`, `\\`, `\n`, `\t` and `\r` are recognised and any other backslash pair is kept verbatim. A bare key with no `=` records `true` (logfmt's flag form). A line with no `key=value` pair at all is prose, and is skipped.
- Bare logfmt values that look like numbers, booleans, or `null` are typed as such. Quoted values stay strings — so `code=500` compares numerically and `code="500"` compares as text.
- Blank lines are ignored. Lines that are neither format, and lines that are not valid UTF-8, are counted and reported once on stderr at the end:

  ```
  logquery: skipped 3 malformed lines
  ```

  Malformed input never aborts the run. Use `-q` to silence the count.

## Examples

The commands below run against [`examples/app.log`](examples/app.log), a mixed JSON Lines and logfmt file included in the repository.

**Errors slower than 100 ms:**

```console
$ logquery 'select level, msg, duration_ms where level = "error" and duration_ms > 100' examples/app.log
level  msg               duration_ms
-----  ----------------  -----------
error  upstream timeout  3001
error  upstream timeout  3002
error  payment declined  220
logquery: skipped 1 malformed line
```

**The three slowest 5xx responses, reaching into nested fields:**

```console
$ logquery 'select ts, msg, http.status, user.id where http.status >= 500 order by duration_ms desc limit 3' examples/app.log
ts                    msg                       http.status  user.id
--------------------  ------------------------  -----------  -------
2026-07-27T09:14:44Z  upstream timeout          504          88
2026-07-27T09:14:15Z  upstream timeout          504          1042
2026-07-27T09:14:23Z  connection reset by peer  502
logquery: skipped 1 malformed line
```

The blank `user.id` on the last row is a missing field, not an empty value.

**Substring search with `like`, as CSV for a spreadsheet:**

```console
$ logquery -q -f csv 'select level, msg where msg like "%timeout%"' examples/app.log
level,msg
error,upstream timeout
error,upstream timeout
```

**JSON Lines out, ready to pipe into `jq` or another logquery:**

```console
$ logquery -q -f json 'select msg, duration_ms where duration_ms > 200 order by duration_ms limit 3' examples/app.log
{"msg":"payment declined","duration_ms":220}
{"msg":"request served","duration_ms":250}
{"msg":"slow query","duration_ms":812}
```

**Reading a pipe:**

```console
$ kubectl logs deploy/api | logquery 'select ts, msg where level = "error" limit 20'
```

**Following a growing file** — rows print as they arrive, and logquery exits once the limit is reached:

```console
$ logquery --follow 'select level, msg where level = "error" limit 2' /var/log/app.log
level  msg
-----  -------------
error  first failure
level  msg
-----  --------------
error  second failure
```

**Query errors point at the problem:**

```console
$ logquery 'select level msg' examples/app.log
logquery: query error at column 14: expected end of query, found field `msg`
  select level msg
               ^
```

```console
$ logquery 'select msg where level == "erro' examples/app.log
logquery: query error at column 27: unterminated string literal, expected closing `"`
  select msg where level == "erro
                            ^
```

## Design notes

The query language is implemented from scratch: a hand-written tokenizer (`src/lexer.rs`) feeding a recursive-descent parser (`src/parser.rs`) that produces the AST in `src/ast.rs`. Every token carries its column so parse errors can render the caret diagrams above. There is no SQL parser dependency.

The only dependencies are `serde_json` (JSON Lines parsing and the record representation) and `anyhow` (error context in the I/O layer).

Execution has two modes. Without `order by`, the engine is streaming: each line is filtered and printed as it is read, and a `limit` stops the read early — querying the first 10 errors of a 4 GB log does not read 4 GB. With `order by`, rows are buffered until end of input, because sorting cannot be done otherwise.

## Tests

```sh
cargo test
```

The suite covers the tokenizer, the parser (including precedence and every parse error path), expression evaluation for every operator, the coercion edge cases documented above, dotted and indexed path access, logfmt parsing with quoted values and escapes, the output writers, argument parsing, and end-to-end queries over in-memory log lines.

## License

MIT. See [LICENSE](LICENSE).

Copyright (c) 2026 roxiproject
