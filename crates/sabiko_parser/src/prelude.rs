//! The `core` prelude (decision 90): Sabiko source under `library/core`, compiled into the
//! compiler and parsed with every program, after the user's file. Its public items
//! (`Option`, `Some`, `Result`, ...) are visible in every program unless a user item of
//! the same name shadows them (`sabiko_resolve::resolve_program`); its generic items are
//! only instantiated where a program reaches them. Each file is registered in the source
//! map under its path in the repository, so a diagnostic pointing into it (a "defined
//! here" label) names it.
//!
//! The prelude is one library file in all but name: its files share a namespace, may
//! contain `use` declarations and write `!`, and their items keep the order below.

use sabiko_ast::SourceFile;
use sabiko_diagnostics::{DiagnosticSink, SourceMap};

use crate::{parse_file_with, ParseOptions};

/// The prelude's files, as `(path, source)`, in the order their items are appended.
pub const CORE: &[(&str, &str)] = &[
    (
        "library/core/option.sabi",
        include_str!("../../../library/core/option.sabi"),
    ),
    (
        "library/core/result.sabi",
        include_str!("../../../library/core/result.sabi"),
    ),
    (
        "library/core/fmt.sabi",
        include_str!("../../../library/core/fmt.sabi"),
    ),
];

/// Parses the prelude's files into `source_map`, appends their items to `file` (the
/// user's) and returns the index of the first. Node ids continue from `next_node_id`, the
/// first the user's file did not use ([`parse_file_with`]), so they stay distinct.
pub fn add_core_prelude(
    file: &mut SourceFile,
    mut next_node_id: u32,
    source_map: &mut SourceMap,
    sink: &mut DiagnosticSink,
) -> usize {
    let start = file.items.len();
    for (name, src) in CORE {
        let id = source_map.add(*name, *src);
        let options = ParseOptions {
            first_node_id: next_node_id,
            library: true,
        };
        let (parsed, next) = parse_file_with(id, src, options, sink);
        next_node_id = next;
        file.items.extend(parsed.items);
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelude_parses_without_diagnostics_and_after_the_users_nodes() {
        let mut sm = SourceMap::new();
        let mut sink = DiagnosticSink::new();
        let src = "fn Main() {}";
        let id = sm.add("main.sabi", src);
        let (mut file, next) = parse_file_with(id, src, ParseOptions::default(), &mut sink);
        let user_items = file.items.len();
        let start = add_core_prelude(&mut file, next, &mut sm, &mut sink);
        assert!(sink.diagnostics().is_empty(), "{:?}", sink.diagnostics());
        assert_eq!(start, user_items);
        assert!(file.items.len() > start);
        assert!(file.items[start..].iter().all(|i| i.id.0 >= next));
    }

    #[test]
    fn use_and_never_are_library_only() {
        let mut sink = DiagnosticSink::new();
        let src = "use Option::{None, Some};\nfn F() -> ! { loop {} }\n";
        let _ = crate::parse_file(sabiko_span::FileId(0), src, &mut sink);
        // `use` and `!` are each E0107 in a user file (recovery adds an E0101 after the
        // first).
        let unsupported = sink
            .diagnostics()
            .iter()
            .filter(|d| d.code == sabiko_diagnostics::ErrorCode::UNSUPPORTED_SYNTAX)
            .count();
        assert_eq!(unsupported, 2, "{:?}", sink.diagnostics());
        let mut sink = DiagnosticSink::new();
        let options = ParseOptions {
            first_node_id: 0,
            library: true,
        };
        let (file, _) = parse_file_with(sabiko_span::FileId(0), src, options, &mut sink);
        assert!(sink.diagnostics().is_empty());
        assert_eq!(file.items.len(), 2);
    }
}
