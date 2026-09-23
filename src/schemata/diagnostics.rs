// Copyright 2026 Martin Pool

//! Extract compile errors and their locations from `cargo --message-format=json` output.

#![warn(clippy::pedantic)]

use serde_json::Value;

/// A location in a source file, as a path relative to the workspace and a byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Location {
    pub file: String,
    pub byte: usize,
    /// 1-based line, for people reading reports.
    pub line: usize,
    /// 1-based column, in characters.
    pub column: usize,
}

/// One compile error reported by rustc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompileError {
    /// The rustc error code, like `E0277`, if the error has one.
    pub code: Option<String>,
    /// The one-line error message.
    pub message: String,
    /// Candidate locations, most specific first.
    ///
    /// For each primary span, this is the span itself followed by the call sites of
    /// any macro expansions it came from, so that an error inside a macro expansion
    /// can still be attributed to the code that invoked the macro.
    pub locations: Vec<Location>,
}

/// Parse the compile errors out of cargo's JSON message stream.
///
/// Lines that are not JSON compiler messages are ignored, so the input can be a log
/// that also contains other output.
pub(crate) fn compile_errors(cargo_output: &str) -> Vec<CompileError> {
    cargo_output
        .lines()
        .filter(|line| line.starts_with('{'))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["reason"] == "compiler-message")
        .filter_map(|value| {
            let diagnostic = &value["message"];
            let level = diagnostic["level"].as_str()?;
            let message = diagnostic["message"].as_str()?;
            if !level.starts_with("error") || message.starts_with("aborting due to") {
                return None;
            }
            let locations = diagnostic["spans"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|span| span["is_primary"] == true)
                .flat_map(expansion_chain)
                .collect();
            Some(CompileError {
                code: diagnostic["code"]["code"].as_str().map(str::to_owned),
                message: message.to_owned(),
                locations,
            })
        })
        .collect()
}

/// The location of a span, followed by the call sites of the macros it was expanded from.
fn expansion_chain(span: &Value) -> Vec<Location> {
    let mut locations = Vec::new();
    let mut span = Some(span);
    while let Some(s) = span {
        if let (Some(file), Some(byte)) = (s["file_name"].as_str(), s["byte_start"].as_u64()) {
            let number = |key: &str| {
                s[key]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(0)
            };
            locations.push(Location {
                file: file.to_owned(),
                byte: usize::try_from(byte).expect("byte offset fits in usize"),
                line: number("line_start"),
                column: number("column_start"),
            });
        }
        span = s["expansion"].get("span");
    }
    locations
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn span(file: &str, byte_start: usize, is_primary: bool, expansion: &Value) -> Value {
        json!({
            "file_name": file,
            "byte_start": byte_start,
            "byte_end": byte_start + 1,
            "line_start": byte_start / 10 + 1,
            "column_start": byte_start % 10 + 1,
            "is_primary": is_primary,
            "expansion": expansion,
        })
    }

    fn compiler_message(level: &str, message: &str, spans: &[Value]) -> String {
        json!({
            "reason": "compiler-message",
            "package_id": "x",
            "message": {
                "$message_type": "diagnostic",
                "level": level,
                "message": message,
                "spans": spans,
                "children": [],
                "rendered": format!("{level}: {message}"),
            }
        })
        .to_string()
    }

    #[test]
    fn compile_errors_keeps_primary_spans_of_errors_only() {
        let output = [
            "*** some log header".to_owned(),
            compiler_message(
                "error",
                "mismatched types",
                &[
                    span("src/lib.rs", 10, false, &Value::Null),
                    span("src/lib.rs", 20, true, &Value::Null),
                ],
            ),
            compiler_message(
                "warning",
                "unused variable",
                &[span("src/lib.rs", 5, true, &Value::Null)],
            ),
            compiler_message("error", "aborting due to 1 previous error", &[]),
            json!({"reason": "build-finished", "success": false}).to_string(),
        ]
        .join("\n");
        assert_eq!(
            compile_errors(&output),
            [CompileError {
                code: None,
                message: "mismatched types".to_owned(),
                locations: vec![Location {
                    file: "src/lib.rs".to_owned(),
                    byte: 20,
                    line: 3,
                    column: 1,
                }],
            }]
        );
    }

    #[test]
    fn compile_errors_reads_error_code_and_line() {
        let mut message: Value = serde_json::from_str(&compiler_message(
            "error",
            "the trait bound `Instant: Default` is not satisfied",
            &[span("src/lib.rs", 23, true, &Value::Null)],
        ))
        .unwrap();
        message["message"]["code"] = json!({"code": "E0277", "explanation": "..."});
        assert_eq!(
            compile_errors(&message.to_string()),
            [CompileError {
                code: Some("E0277".to_owned()),
                message: "the trait bound `Instant: Default` is not satisfied".to_owned(),
                locations: vec![Location {
                    file: "src/lib.rs".to_owned(),
                    byte: 23,
                    line: 3,
                    column: 4,
                }],
            }]
        );
    }

    #[test]
    fn compile_errors_follows_macro_expansions_to_call_site() {
        let call_site = span("src/lib.rs", 42, false, &Value::Null);
        let inner = span(
            "/rustc/library/core/src/macros/mod.rs",
            7,
            true,
            &json!({"span": call_site, "macro_decl_name": "assert_eq!"}),
        );
        let output = compiler_message("error", "binary operation cannot be applied", &[inner]);
        assert_eq!(
            compile_errors(&output)[0].locations,
            [
                Location {
                    file: "/rustc/library/core/src/macros/mod.rs".to_owned(),
                    byte: 7,
                    line: 1,
                    column: 8,
                },
                Location {
                    file: "src/lib.rs".to_owned(),
                    byte: 42,
                    line: 5,
                    column: 3,
                },
            ]
        );
    }

    #[test]
    fn compile_errors_keeps_errors_without_spans() {
        let output = compiler_message("error", "linking with `cc` failed", &[]);
        assert_eq!(
            compile_errors(&output),
            [CompileError {
                code: None,
                message: "linking with `cc` failed".to_owned(),
                locations: Vec::new(),
            }]
        );
    }
}
