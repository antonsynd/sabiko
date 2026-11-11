//! The region model of an `impl` header (decision 95): only the `core` prelude writes
//! references in a header, and the borrow checker gives all the references of one header
//! one region, which is right only for a block that names one lifetime in all of them, as
//! std's `impl<'a, T> From<&'a Option<T>> for Option<&'a T>` does. This test lists every
//! prelude block whose header holds a reference and requires exactly the known ones, each
//! with one reference in its type and one in its trait's arguments: a new such block must
//! be reviewed against that model (and added here) before it can be compiled.

use sabiko_diagnostics::{DiagnosticSink, SourceMap};
use sabiko_resolve::DefKind;

fn count_refs(ty: &sabiko_types::Ty) -> usize {
    use sabiko_types::Ty;
    match ty {
        Ty::Ref(_, inner) => 1 + count_refs(inner),
        Ty::Array(inner, _) | Ty::Slice(inner) => count_refs(inner),
        Ty::Tuple(elems) => elems.iter().map(count_refs).sum(),
        Ty::Adt(_, args) => args.iter().map(count_refs).sum(),
        _ => 0,
    }
}

#[test]
fn prelude_headers_with_references_name_one_lifetime() {
    let mut sm = SourceMap::new();
    let mut sink = DiagnosticSink::new();
    let front = sabiko_driver::parse_and_resolve("empty.sabi", "", &mut sm, &mut sink);
    let (ast, resolve) = front.program.expect("the prelude resolves");
    let typeck = sabiko_types::typeck(&ast, &resolve, &mut sink);
    assert!(!sink.has_errors(), "{:?}", sink.diagnostics());
    let mut found = Vec::new();
    for tr in resolve.defs_of_kind(DefKind::Trait) {
        for imp in typeck.items.impls_of(tr) {
            let in_type = count_refs(&imp.self_ty);
            let in_args: usize = imp.trait_args.iter().map(count_refs).sum();
            if in_type + in_args == 0 {
                continue;
            }
            assert!(
                resolve.is_prelude(imp.def),
                "only the prelude may write a reference in a header"
            );
            assert_eq!(
                (in_type, in_args),
                (1, 1),
                "{}: one lifetime, written once in the type and once in the trait's arguments",
                resolve.def(imp.def).name
            );
            found.push(resolve.def(imp.def).name.clone());
        }
    }
    found.sort();
    assert_eq!(
        found,
        [
            "<Option<&T> as From<&Option<T>>>",
            "<Option<&mut T> as From<&mut Option<T>>>",
        ]
    );
}
