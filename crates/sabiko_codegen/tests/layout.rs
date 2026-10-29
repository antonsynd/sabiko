//! Block layout: the only backward branches in emitted IL are the back edges of loops.
//!
//! The JIT compiles a method with a backward branch as a loop (instrumented Tier0, or OSR),
//! so a method whose MIR has no cycle must have no backward branch at all, and in a method
//! with loops every backward branch must jump to the start of a block on a cycle. Checked
//! for every program in the IL fixtures, the run-pass and run-time differential corpora and
//! the benchmarks, in debug and release builds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sabiko_codegen::{Options, OutputKind};
use sabiko_diagnostics::{DiagnosticSink, SourceMap};
use sabiko_mir::{BasicBlock, Body};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "sabi") {
            out.push(path);
        }
    }
}

fn corpus() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in [
        root.join("tests/fixtures"),
        root.join("../../tests/run-pass"),
        root.join("../../tests/differential/run"),
        root.join("../../benchmarks"),
    ] {
        collect(&dir, &mut files);
    }
    files.sort();
    files
}

/// Compiles `src` to MIR (after simplification) and IL, or `None` if it does not compile.
fn compile(path: &Path, overflow_checks: bool) -> Option<(Vec<Body>, String)> {
    let src = std::fs::read_to_string(path).unwrap();
    let mut sm = SourceMap::new();
    let id = sm.add(path.display().to_string(), src.clone());
    let mut sink = DiagnosticSink::new();
    let ast = sabiko_parser::parse_file(id, &src, &mut sink);
    if sink.has_errors() {
        return None;
    }
    let resolve = sabiko_resolve::resolve(&ast, &mut sink);
    let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
    if sink.has_errors() {
        return None;
    }
    let mut bodies = sabiko_mir::build::build_bodies(&ast, &resolve, &typeck, &mut sink);
    sabiko_mir::simplify::simplify_all(&mut bodies);
    sabiko_borrowck::check_all(&bodies, &resolve, &typeck, &mut sink);
    if sink.has_errors() {
        return None;
    }
    sabiko_mir::forward::forward_all(&mut bodies);
    let options = Options {
        assembly_name: "layout".to_owned(),
        kind: OutputKind::Library,
        overflow_checks,
    };
    let il = sabiko_codegen::emit_il(&bodies, &resolve, &typeck, &sm, &options, &mut sink)?;
    Some((bodies, il))
}

/// Blocks that lie on a cycle of the control-flow graph (reach themselves).
fn blocks_on_cycles(body: &Body) -> Vec<bool> {
    let n = body.blocks.len();
    (0..n)
        .map(|start| {
            let mut seen = vec![false; n];
            let mut stack: Vec<BasicBlock> = body.blocks[start].terminator().kind.successors();
            while let Some(bb) = stack.pop() {
                if bb.index() == start {
                    return true;
                }
                if !std::mem::replace(&mut seen[bb.index()], true) {
                    stack.extend(body.block(bb).terminator().kind.successors());
                }
            }
            false
        })
        .collect()
}

/// The methods of an IL module: name and body lines.
fn methods(il: &str) -> Vec<(String, Vec<&str>)> {
    let mut out: Vec<(String, Vec<&str>)> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in il.lines() {
        let t = line.trim();
        if t.starts_with(".method ") {
            if let Some(m) = current.take() {
                out.push(m);
            }
            // `... 'Name'(params) cil managed`: the quoted name right before `(`; the
            // constructor of a generated tuple type is `.ctor`.
            // Generated tuple types are quoted names with parentheses (`'(&i32, u8)'`) and
            // may appear in the return and parameter types, so the parameter list is found
            // as the balanced parentheses ending the line.
            let head = t.trim_end_matches(" cil managed");
            let mut depth = 0;
            let open = head
                .char_indices()
                .rev()
                .find(|&(_, c)| {
                    match c {
                        ')' => depth += 1,
                        '(' => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                })
                .map(|(i, _)| i)
                .expect("parameter list");
            let name = if head[..open].ends_with(" .ctor") {
                ".ctor".to_owned()
            } else {
                let end = open - 1;
                let start = head[..end].rfind('\'').expect("quoted method name") + 1;
                head[start..end].to_owned()
            };
            current = Some((name, Vec::new()));
        } else if let Some((_, lines)) = &mut current {
            lines.push(t);
        }
    }
    out.extend(current);
    out
}

/// `(line index, target label)` of every branch, and the line index of every label.
fn branches<'a>(lines: &[&'a str]) -> (Vec<(usize, &'a str)>, HashMap<&'a str, usize>) {
    let mut jumps = Vec::new();
    let mut labels = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(label) = line.strip_suffix(':') {
            labels.insert(label, i);
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(op), Some(target), None) = (words.next(), words.next(), words.next()) else {
            continue;
        };
        if (op.starts_with('b') && op != "box") || op.starts_with("leave") {
            jumps.push((i, target));
        }
    }
    (jumps, labels)
}

#[test]
fn only_loop_back_edges_branch_backwards() {
    let mut checked = 0;
    for path in corpus() {
        for overflow_checks in [true, false] {
            let Some((bodies, il)) = compile(&path, overflow_checks) else {
                continue;
            };
            let by_name: HashMap<&str, &Body> =
                bodies.iter().map(|b| (b.name.as_str(), b)).collect();
            for (name, lines) in methods(&il) {
                let Some(body) = by_name.get(name.as_str()) else {
                    continue;
                };
                checked += 1;
                let on_cycle = blocks_on_cycles(body);
                let (jumps, labels) = branches(&lines);
                for (at, target) in jumps {
                    let to = labels[target];
                    if to > at {
                        continue;
                    }
                    let block = target
                        .strip_prefix("BB")
                        .and_then(|n| n.parse::<usize>().ok());
                    assert!(
                        block.is_some_and(|b| on_cycle[b]),
                        "{} ({}): `{}` in `{name}` branches backwards to {target}, which is not \
                         the start of a block on a loop\n{}",
                        path.display(),
                        if overflow_checks { "debug" } else { "release" },
                        lines[at],
                        lines.join("\n")
                    );
                }
            }
        }
    }
    assert!(
        checked > 100,
        "only {checked} methods checked; corpus missing?"
    );
}
