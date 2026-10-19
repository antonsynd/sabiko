//! Golden tests over `tests/ui/**/*.sabi`. See `tests/ui/README.md` for the conventions.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sabiko_diagnostics::{Format, Severity};
use sabiko_driver::{compile_str, Emit, Options, OutputKind, Stage};

#[derive(Debug)]
struct Annotation {
    line: u32,
    severity: Severity,
    code: Option<String>,
    text: String,
}

fn parse_annotations(src: &str) -> Vec<Annotation> {
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let Some(idx) = line.find("//~") else {
            continue;
        };
        let rest = &line[idx + 3..];
        let carets = rest.chars().take_while(|&c| c == '^').count();
        let rest = rest[carets..].trim_start();
        let (severity, rest) = if let Some(r) = rest.strip_prefix("ERROR") {
            (Severity::Error, r)
        } else if let Some(r) = rest.strip_prefix("WARN") {
            (Severity::Warning, r)
        } else {
            panic!(
                "line {}: annotation must be `//~ ERROR` or `//~ WARN`: {line}",
                i + 1
            );
        };
        let rest = rest.trim();
        let (code, text) = match rest.split_once(char::is_whitespace) {
            Some((first, tail)) if is_code(first) => {
                (Some(first.to_owned()), tail.trim().to_owned())
            }
            _ if is_code(rest) => (Some(rest.to_owned()), String::new()),
            _ => (None, rest.to_owned()),
        };
        let line_no = u32::try_from(i + 1 - carets).unwrap();
        out.push(Annotation {
            line: line_no,
            severity,
            code,
            text,
        });
    }
    out
}

fn is_code(s: &str) -> bool {
    s.len() == 5 && s.starts_with('E') && s[1..].chars().all(|c| c.is_ascii_digit())
}

fn ui_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/ui")
        .canonicalize()
        .unwrap()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "sabi") {
            out.push(path);
        }
    }
}

/// Runs one UI test and returns a failure description, if any.
fn run_one(root: &Path, path: &Path, bless: bool) -> Option<String> {
    let rel = path
        .strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let src = std::fs::read_to_string(path).unwrap();
    // The directory names the stage under test; the pipeline stops after it so tests of
    // early stages are not disturbed by later ones (a parser test may call undefined
    // functions, for instance).
    // `codegen/` also runs the IL backend (`--emit=il`), which the pipeline otherwise
    // skips, so its `E05xx` diagnostics can be tested; `codegen/exe/` asks for an
    // executable, which is what checks the `Main` signature.
    let (stop_after, emit, output_kind) = match rel.split('/').next().unwrap_or("") {
        "parse" => (Stage::Parse, Emit::None, OutputKind::Library),
        "resolve" => (Stage::Resolve, Emit::None, OutputKind::Library),
        "typeck" => (Stage::Typeck, Emit::None, OutputKind::Library),
        "codegen" => {
            let kind = if rel.starts_with("codegen/exe/") {
                OutputKind::Executable
            } else {
                OutputKind::Library
            };
            (Stage::Full, Emit::Il, kind)
        }
        _ => (Stage::Full, Emit::None, OutputKind::Library),
    };
    let options = Options {
        emit,
        stop_after,
        output_kind,
        ..Options::default()
    };
    let compilation = compile_str(&format!("tests/ui/{rel}"), &src, options);
    let mut failures = String::new();

    // 1. annotations vs diagnostics
    let annotations = parse_annotations(&src);
    let file_name = path.file_name().unwrap().to_string_lossy();
    if file_name.starts_with("ok_") && !compilation.diagnostics.is_empty() {
        let _ = writeln!(failures, "  expected no diagnostics (file is `ok_*`)");
    }
    let sm = &compilation.source_map;
    let mut matched = vec![false; compilation.diagnostics.len()];
    for ann in &annotations {
        let found = compilation.diagnostics.iter().enumerate().find(|(i, d)| {
            !matched[*i]
                && d.severity == ann.severity
                && ann.code.as_ref().is_none_or(|c| c == &d.code.to_string())
                && d.message.contains(&ann.text)
                && d.primary_span()
                    .is_some_and(|s| sm.get(s.file).line_col(s.start).line == ann.line)
        });
        match found {
            Some((i, _)) => matched[i] = true,
            None => {
                let _ = writeln!(
                    failures,
                    "  unmatched annotation at line {}: {:?} {} {:?}",
                    ann.line,
                    ann.severity,
                    ann.code.clone().unwrap_or_default(),
                    ann.text
                );
            }
        }
    }
    for (i, d) in compilation.diagnostics.iter().enumerate() {
        if !matched[i] {
            let line = d
                .primary_span()
                .map(|s| sm.get(s.file).line_col(s.start).line);
            let _ = writeln!(
                failures,
                "  unannotated diagnostic at line {line:?}: {} {}",
                d.code, d.message
            );
        }
    }

    // 2. golden stderr
    let rendered = compilation.render_diagnostics(Format::HumanPlain);
    let stderr_path = path.with_extension("stderr");
    if bless {
        if rendered.is_empty() {
            let _ = std::fs::remove_file(&stderr_path);
        } else {
            std::fs::write(&stderr_path, &rendered).unwrap();
        }
    } else {
        let expected = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        if expected != rendered {
            let _ = writeln!(
                failures,
                "  stderr differs from {} (run with BLESS=1 to update)",
                stderr_path.display()
            );
            let _ = writeln!(
                failures,
                "  --- expected ---\n{expected}  --- actual ---\n{rendered}"
            );
        }
    }

    if failures.is_empty() {
        None
    } else {
        Some(format!("{rel}:\n{failures}"))
    }
}

#[test]
fn ui() {
    let root = ui_root();
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "no UI tests found under {}",
        root.display()
    );
    let bless = std::env::var_os("BLESS").is_some();
    let failures: Vec<String> = files
        .iter()
        .filter_map(|p| run_one(&root, p, bless))
        .collect();
    assert!(
        failures.is_empty(),
        "{} UI test(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("{} UI tests passed", files.len());
}
