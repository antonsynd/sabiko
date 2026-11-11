//! Origins ("regions") for the loan check.
//!
//! Sabiko has no lifetime syntax yet, so every reference type gets an inferred origin:
//! a [`RegionTy`] mirrors a [`Ty`] and carries one [`Origin`] per `Ref` node. Struct
//! fields never write a reference in this phase, so origins appear along chains of `Ref`s
//! (`&&T`, `&mut &T`), in the elements of tuples (`(&T, &mut U)`), in the element of an
//! array or slice, represented as a one-element tuple (`[&T; 3]`), and in the generic
//! arguments of a struct or enum instance (`Opt<&T>`), whose fields share the arguments'
//! origins (#115).
//!
//! Function signatures use *universal* origins: one per reference in the parameter types.
//! The return type follows Rust's elision rules: in a method taking `&self` or `&mut self`
//! every reference in the return type has `self`'s origin; otherwise, when exactly one
//! origin occurs across all parameters, every reference in the return type uses it; failing
//! both, the signature is unusable and the caller reports E0416.
//!
//! This module also defines the atom newtypes the Polonius engine indexes its facts with.

use polonius_engine::Atom;
use sabiko_mir::{Mutability, PlaceElem, Ty};

use sabiko_resolve::DefId;
use sabiko_types::{FnSig, ItemTypes, SelfKind};

macro_rules! atom {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub usize);

        impl $name {
            #[must_use]
            pub const fn index(self) -> usize {
                self.0
            }
        }

        impl From<usize> for $name {
            fn from(index: usize) -> Self {
                Self(index)
            }
        }

        impl From<$name> for usize {
            fn from(atom: $name) -> usize {
                atom.0
            }
        }

        impl Atom for $name {
            fn index(self) -> usize {
                self.0
            }
        }
    };
}

atom!(
    /// A region: the set of loans a reference may hold.
    Origin
);
atom!(
    /// A borrow, `&place` or `&mut place`, in order of appearance in the body. Universal
    /// origins get placeholder loans numbered after the real ones.
    Loan
);
atom!(
    /// A point in the CFG: `Start` or `Mid` of a MIR location (see `facts::LocationTable`).
    Point
);
atom!(
    /// A MIR local.
    Variable
);
atom!(
    /// A move path. Unused: the move checker is a separate dataflow analysis.
    Path
);

/// Hands out fresh origins.
#[derive(Debug, Default)]
pub struct OriginSupply {
    next: usize,
}

impl OriginSupply {
    pub fn fresh(&mut self) -> Origin {
        let origin = Origin(self.next);
        self.next += 1;
        origin
    }
}

/// A type with an origin on every reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionTy {
    Ref(Mutability, Origin, Box<RegionTy>),
    /// A tuple with references among its elements.
    Tuple(Vec<RegionTy>),
    /// A struct or enum instance with references among its generic arguments: the region
    /// types of its arguments, and those of its fields (by variant, a struct's at 0),
    /// which hold the arguments' origins.
    Adt {
        args: Vec<RegionTy>,
        fields: Vec<Vec<RegionTy>>,
    },
    /// Anything without references: primitives, structs, `()`, `!`, errors.
    Other,
}

impl RegionTy {
    /// `ty` with a fresh origin on every reference.
    pub fn fresh(ty: &Ty, items: &ItemTypes, supply: &mut OriginSupply) -> Self {
        Params::none(items).fresh(ty, supply)
    }

    /// `ty` with the same origin on every reference (the elided return type).
    #[must_use]
    pub fn uniform(ty: &Ty, origin: Origin, items: &ItemTypes) -> Self {
        Params::none(items).uniform(ty, Some(origin))
    }

    /// The region type of an instance of the struct or enum `def` whose arguments have the
    /// region types `args`: each field's, with the type's parameters replaced by them.
    fn adt(def: DefId, args: Vec<RegionTy>, items: &ItemTypes) -> Self {
        if args.iter().all(|a| *a == RegionTy::Other) {
            return RegionTy::Other;
        }
        let adt = items.adt(def);
        let params = Params {
            defs: &adt.generics,
            args: &args,
            items,
        };
        let variants: Vec<&[sabiko_types::FieldDef]> = if adt.is_enum() {
            adt.variants().iter().map(|v| v.fields.as_slice()).collect()
        } else {
            vec![adt.fields.as_slice()]
        };
        // Field types write no reference (Phase 9), so nothing here takes a new origin.
        let mut unused = OriginSupply::default();
        let fields = variants
            .iter()
            .map(|fs| {
                fs.iter()
                    .map(|f| params.fresh(&f.ty, &mut unused))
                    .collect()
            })
            .collect();
        RegionTy::Adt { args, fields }
    }

    /// Every origin in the type, depth first, outermost first along each chain of
    /// references, tuple elements left to right.
    #[must_use]
    pub fn origins(&self) -> Vec<Origin> {
        let mut out = Vec::new();
        self.sites(&mut Vec::new(), &mut |origin, _: &[SiteElem]| {
            out.push(origin);
        });
        out
    }

    /// Calls `f` with every origin in the type and the projection from a value of this
    /// type to the reference carrying it (`[]` for the outermost reference, `[Deref]` for
    /// the one it points to, `[Field(1)]` for the second element of a tuple).
    pub fn sites(&self, path: &mut Vec<SiteElem>, f: &mut impl FnMut(Origin, &[SiteElem])) {
        match self {
            Self::Ref(_, origin, inner) => {
                f(*origin, path);
                path.push(SiteElem::Deref);
                inner.sites(path, f);
                path.pop();
            }
            Self::Tuple(elems) => {
                for (i, e) in elems.iter().enumerate() {
                    path.push(SiteElem::Field(i));
                    e.sites(path, f);
                    path.pop();
                }
            }
            Self::Adt { args, .. } => {
                for (i, a) in args.iter().enumerate() {
                    path.push(SiteElem::Arg(i));
                    a.sites(path, f);
                    path.pop();
                }
            }
            Self::Other => {}
        }
    }

    /// The type of `*self`; `Other` when `self` is not a reference.
    #[must_use]
    pub fn referent(&self) -> &RegionTy {
        match self {
            Self::Ref(_, _, inner) => inner,
            Self::Tuple(_) | Self::Adt { .. } | Self::Other => &Self::Other,
        }
    }

    /// The type of a place built from `self` by `projection`. A field of a struct is
    /// `Other` (struct fields hold no references); a tuple element has its own origins.
    #[must_use]
    pub fn project(&self, projection: &[PlaceElem]) -> RegionTy {
        let mut cur = self;
        let mut variant = None;
        for elem in projection {
            match (elem, cur) {
                (PlaceElem::Deref, _) => cur = cur.referent(),
                (PlaceElem::Field(index, _), Self::Tuple(elems)) => match elems.get(*index) {
                    Some(e) => cur = e,
                    None => return Self::Other,
                },
                // A field of an instance holds its arguments' origins.
                (PlaceElem::Field(index, _), Self::Adt { fields, .. }) => {
                    match fields.get(variant.unwrap_or(0)).and_then(|f| f.get(*index)) {
                        Some(f) => cur = f,
                        None => return Self::Other,
                    }
                }
                // An array holding references is a one-element tuple of its element.
                (PlaceElem::Index(_), Self::Tuple(elems)) => match elems.first() {
                    Some(e) => cur = e,
                    None => return Self::Other,
                },
                (PlaceElem::Field(..) | PlaceElem::Index(_), _) => return Self::Other,
                (PlaceElem::Downcast(v), _) => {
                    variant = Some(*v);
                    continue;
                }
            }
            variant = None;
        }
        cur.clone()
    }
}

/// One step of the path from a parameter to a reference in its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteElem {
    Deref,
    Field(usize),
    /// Inside the generic argument at this index of a struct or enum instance.
    Arg(usize),
}

/// The origins of one function signature.
#[derive(Clone, Debug)]
pub struct SigRegions {
    pub inputs: Vec<RegionTy>,
    /// `None` when the return type contains a reference but elision cannot pick its
    /// origin (E0416).
    pub output: Option<RegionTy>,
    /// How many reference origins the parameters introduce, for the E0416 note.
    pub input_origins: usize,
    /// The region types of the call's generic arguments.
    pub generic: Vec<RegionTy>,
}

impl SigRegions {
    /// `(origin, parameter index, path)` for every parameter origin: `path` leads from
    /// the parameter to the reference with this origin (`[]` for `x: &T`, `[Deref]` for
    /// the inner reference of `x: &mut &T`, `[Field(1)]` for `t.1` of `t: (i32, &T)`).
    #[must_use]
    pub fn input_origin_sites(&self) -> Vec<(Origin, usize, Vec<SiteElem>)> {
        let mut out = Vec::new();
        for (param, ty) in self.inputs.iter().enumerate() {
            ty.sites(&mut Vec::new(), &mut |origin, path| {
                out.push((origin, param, path.to_vec()));
            });
        }
        out
    }

    /// Implied bounds: inside `&'a &'b T` (or `&'a (&'b T, U)`) the referent outlives the
    /// reference, `'b: 'a`, returned as `(longer, shorter)` pairs, i.e. Polonius'
    /// `known_placeholder_subset`.
    pub fn implied_bounds(&self) -> Vec<(Origin, Origin)> {
        fn walk(ty: &RegionTy, out: &mut Vec<(Origin, Origin)>) {
            match ty {
                RegionTy::Ref(_, outer, inner) => {
                    for inner_origin in inner.origins() {
                        out.push((inner_origin, *outer));
                    }
                    walk(inner, out);
                }
                RegionTy::Tuple(elems) | RegionTy::Adt { args: elems, .. } => {
                    for e in elems {
                        walk(e, out);
                    }
                }
                RegionTy::Other => {}
            }
        }
        let mut out = Vec::new();
        for ty in &self.inputs {
            walk(ty, &mut out);
        }
        out
    }
}

/// Assigns fresh origins to the parameters of `sig` and elides the return type, by Rust's
/// rules: a method taking `&self` or `&mut self` returns references with `self`'s origin,
/// whatever its other parameters; otherwise the one origin of the parameters, if there is
/// exactly one.
///
/// `generic_args` are a call's generic arguments (empty for a function's own signature,
/// whose type parameters hold no reference of its body). Each argument gets its origins
/// once, and every occurrence of its parameter shares them, as rustc's `T = &'1 i32` does:
/// `Id::<&i32>(&y)` returns a reference with the argument's origin. Elision counts only
/// the references the signature writes, not those of the arguments.
pub fn signature_regions(
    sig: &FnSig,
    generic_args: &[Ty],
    items: &ItemTypes,
    supply: &mut OriginSupply,
) -> SigRegions {
    let args: Vec<RegionTy> = generic_args
        .iter()
        .map(|t| RegionTy::fresh(t, items, supply))
        .collect();
    let params = Params {
        defs: if args.is_empty() { &[] } else { &sig.generics },
        args: &args,
        items,
    };
    let inputs: Vec<RegionTy> = sig
        .inputs
        .iter()
        .map(|ty| params.fresh(ty, supply))
        .collect();
    let mut input_origins = Vec::new();
    for (ty, region) in sig.inputs.iter().zip(&inputs) {
        written_origins(ty, region, &mut input_origins);
    }
    let self_origin = match (sig.self_kind, inputs.first()) {
        (Some(SelfKind::Ref { .. }), Some(RegionTy::Ref(_, origin, _))) => Some(*origin),
        _ => None,
    };
    let output = if !sig.output.has_ref() {
        Some(params.uniform(&sig.output, None))
    } else if let Some(origin) = self_origin {
        Some(params.uniform(&sig.output, Some(origin)))
    } else if let [single] = input_origins[..] {
        Some(params.uniform(&sig.output, Some(single)))
    } else {
        None
    };
    SigRegions {
        inputs,
        output,
        input_origins: input_origins.len(),
        generic: args,
    }
}

/// How deep the bounds of `impl` blocks are followed for [`bound_region_links`]: exactly
/// as deep as the type checker proves them, so a proof is never cut short here.
const LINK_DEPTH: usize = sabiko_types::traits::DEPTH_LIMIT;

/// The region types a call must relate invariantly to prove the callee's bounds, as
/// rustc's type checking of a call proves each where-clause of the callee instantiated at
/// the call's generic arguments, regions included (decision 95). `generics` and `env`
/// are the callee's type parameters and bounds (a trait member's include `Self:
/// Trait<..>`), `generic_args` and `generic_regions` the call's generic arguments and
/// their region types; `caller_env` the bounds in scope at the call.
///
/// Each bound is proved by the `impl` block selection picks at the call's types (with
/// the caller's bounds in scope), whose header relates the region types: a type
/// parameter the header names twice stands for one type, references and all (`impl<T>
/// From<T> for T` makes `From::from(r)`'s result, or `G(r)`'s for `fn G<T, U: From<T>>`,
/// hold `r`'s loans; `impl<T> From<T> for Option<T>` puts them in the `Some`). The
/// region model of a header is rustc's with named lifetimes: one region per lifetime the
/// header names. Only the prelude writes references in a header, and each of its blocks
/// that does names one lifetime in all of them (`impl<'a, T> From<&'a Option<T>> for
/// Option<&'a T>` in std), so every reference a header writes shares one region; a
/// program's block may not write one (E0107). The block's own bounds are proved the same
/// way, recursively (`impl<T, U: From<T>> Into<U> for T` relates what `U: From<T>`'s
/// block relates). A bound proved by a bound of the caller relates nothing, as in rustc,
/// where a where-clause's types carry no region of the call.
///
/// # Errors
///
/// When a bound the type checker proved cannot be followed (no block and no bound in
/// scope proves it here, the proof is deeper than the recursion limit, or the generic
/// arguments do not fit the callee): an internal error the caller reports (E0900), never
/// a silent "no relation", which would let a loan go.
#[allow(clippy::too_many_arguments)]
pub fn bound_region_links(
    items: &ItemTypes,
    generics: &[DefId],
    env: &[sabiko_types::traits::Predicate],
    generic_args: &[Ty],
    generic_regions: &[RegionTy],
    caller_env: &[sabiko_types::traits::Predicate],
    supply: &mut OriginSupply,
) -> Result<Vec<(RegionTy, RegionTy)>, String> {
    let mut out = Vec::new();
    if env.is_empty() {
        return Ok(out);
    }
    if generics.len() != generic_args.len() || generic_args.len() != generic_regions.len() {
        return Err(format!(
            "the call has {} generic arguments ({} region types) for {} parameters",
            generic_args.len(),
            generic_regions.len(),
            generics.len()
        ));
    }
    let params = Params {
        defs: generics,
        args: generic_regions,
        items,
    };
    for pred in env {
        let (pred_ty, pred_args) =
            sabiko_types::traits::instantiate_pred(pred, generics, generic_args);
        let mut tys = vec![pred_ty];
        tys.extend(pred_args);
        let mut regions = vec![params.arg(pred.param)];
        regions.extend(pred.args.iter().map(|a| params.fresh(a, supply)));
        links_at(
            items,
            pred.trait_,
            &tys,
            &regions,
            caller_env,
            supply,
            &mut out,
            0,
        )?;
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn links_at(
    items: &ItemTypes,
    tr: DefId,
    tys: &[Ty],
    regions: &[RegionTy],
    caller_env: &[sabiko_types::traits::Predicate],
    supply: &mut OriginSupply,
    out: &mut Vec<(RegionTy, RegionTy)>,
    depth: usize,
) -> Result<(), String> {
    let Some((self_ty, args)) = tys.split_first() else {
        return Err("a bound without a type".to_owned());
    };
    if depth >= LINK_DEPTH {
        return Err(format!(
            "the proof of a bound of `{}` is deeper than the recursion limit",
            items.trait_def(tr).map_or("?", |t| t.name.as_str())
        ));
    }
    let Some((imp, impl_args)) = items.select_impl(tr, self_ty, args, caller_env) else {
        // Proved by a bound in scope (a where-clause's types carry no region of the
        // call), by the built-in `Copy`/`Clone` (which relate nothing), or for a type
        // that holds an error (reported).
        if items.proved_without_impl(self_ty, tr, args, caller_env) {
            return Ok(());
        }
        return Err(format!(
            "no `impl` block proves the bound on `{:?}` of trait `{}` here",
            self_ty,
            items.trait_def(tr).map_or("?", |t| t.name.as_str())
        ));
    };
    let mut bound: Vec<Option<RegionTy>> = vec![None; imp.generics.len()];
    let mut header: Option<Origin> = None;
    let pats = std::iter::once(&imp.self_ty).chain(&imp.trait_args);
    for (pat, region) in pats.zip(regions) {
        link_pattern(pat, region, &imp.generics, &mut bound, &mut header, out);
    }
    let bound: Vec<RegionTy> = bound
        .into_iter()
        .map(|b| b.unwrap_or(RegionTy::Other))
        .collect();
    let params = Params {
        defs: &imp.generics,
        args: &bound,
        items,
    };
    for pred in &imp.preds {
        let (pred_ty, pred_args) =
            sabiko_types::traits::instantiate_pred(pred, &imp.generics, &impl_args);
        let mut pred_tys = vec![pred_ty];
        pred_tys.extend(pred_args);
        let mut pred_regions = vec![params.arg(pred.param)];
        pred_regions.extend(pred.args.iter().map(|a| params.fresh(a, supply)));
        links_at(
            items,
            pred.trait_,
            &pred_tys,
            &pred_regions,
            caller_env,
            supply,
            out,
            depth + 1,
        )?;
    }
    Ok(())
}

/// Matches a block's header pattern `pat` against the region type of the call's type at
/// its place: a parameter's second occurrence is related to its first, and a written
/// reference's origin to the header's one origin.
fn link_pattern(
    pat: &Ty,
    region: &RegionTy,
    params: &[DefId],
    bound: &mut [Option<RegionTy>],
    header: &mut Option<Origin>,
    out: &mut Vec<(RegionTy, RegionTy)>,
) {
    let origin_only = |o: Origin| RegionTy::Ref(Mutability::Not, o, Box::new(RegionTy::Other));
    match (pat, region) {
        (Ty::Param(p), _) => {
            if let Some(i) = params.iter().position(|q| q == p) {
                match &bound[i] {
                    Some(first) => out.push((first.clone(), region.clone())),
                    None => bound[i] = Some(region.clone()),
                }
            }
        }
        (Ty::Ref(_, inner), RegionTy::Ref(_, origin, rinner)) => {
            match header {
                Some(h) => out.push((origin_only(*h), origin_only(*origin))),
                None => *header = Some(*origin),
            }
            link_pattern(inner, rinner, params, bound, header, out);
        }
        (Ty::Tuple(elems), RegionTy::Tuple(relems)) => {
            for (e, r) in elems.iter().zip(relems) {
                link_pattern(e, r, params, bound, header, out);
            }
        }
        (Ty::Array(elem, _) | Ty::Slice(elem), RegionTy::Tuple(relems)) => {
            if let Some(r) = relems.first() {
                link_pattern(elem, r, params, bound, header, out);
            }
        }
        (Ty::Adt(_, targs), RegionTy::Adt { args, .. }) => {
            for (t, r) in targs.iter().zip(args) {
                link_pattern(t, r, params, bound, header, out);
            }
        }
        // A part without references: the parameters in it hold none either.
        (pat, RegionTy::Other) => {
            let _ = pat.map_params(&mut |p| {
                if let Some(i) = params.iter().position(|&q| q == p) {
                    bound[i].get_or_insert(RegionTy::Other);
                }
                None
            });
        }
        _ => {}
    }
}

/// The origins of the references written in `ty` (not those a type parameter stands
/// for), given the region type built for it.
fn written_origins(ty: &Ty, region: &RegionTy, out: &mut Vec<Origin>) {
    match (ty, region) {
        (Ty::Ref(_, inner), RegionTy::Ref(_, origin, rinner)) => {
            out.push(*origin);
            written_origins(inner, rinner, out);
        }
        (Ty::Tuple(elems), RegionTy::Tuple(relems)) => {
            for (e, r) in elems.iter().zip(relems) {
                written_origins(e, r, out);
            }
        }
        (Ty::Array(elem, _) | Ty::Slice(elem), RegionTy::Tuple(relems)) => {
            if let Some(r) = relems.first() {
                written_origins(elem, r, out);
            }
        }
        (Ty::Adt(_, targs), RegionTy::Adt { args, .. }) => {
            for (t, r) in targs.iter().zip(args) {
                written_origins(t, r, out);
            }
        }
        _ => {}
    }
}

/// A call's type parameters and the region types of its generic arguments.
struct Params<'a> {
    defs: &'a [DefId],
    args: &'a [RegionTy],
    items: &'a ItemTypes,
}

impl<'a> Params<'a> {
    /// No parameters: a type parameter holds no reference.
    fn none(items: &'a ItemTypes) -> Self {
        Params {
            defs: &[],
            args: &[],
            items,
        }
    }

    fn arg(&self, param: sabiko_resolve::DefId) -> RegionTy {
        self.defs
            .iter()
            .position(|&d| d == param)
            .map_or(RegionTy::Other, |i| self.args[i].clone())
    }

    /// Whether `ty` holds a reference, counting the arguments' references.
    fn holds_ref(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Param(p) => !self.arg(*p).origins().is_empty(),
            Ty::Ref(..) => true,
            Ty::Tuple(elems) => elems.iter().any(|e| self.holds_ref(e)),
            Ty::Adt(_, args) => args.iter().any(|a| self.holds_ref(a)),
            Ty::Array(elem, _) | Ty::Slice(elem) => self.holds_ref(elem),
            _ => false,
        }
    }

    /// [`RegionTy::fresh`], with each type parameter's argument region type.
    fn fresh(&self, ty: &Ty, supply: &mut OriginSupply) -> RegionTy {
        match ty {
            Ty::Param(p) => self.arg(*p),
            Ty::Ref(m, inner) => {
                let origin = supply.fresh();
                RegionTy::Ref(*m, origin, Box::new(self.fresh(inner, supply)))
            }
            Ty::Tuple(elems) if self.holds_ref(ty) => {
                RegionTy::Tuple(elems.iter().map(|e| self.fresh(e, supply)).collect())
            }
            Ty::Array(elem, _) | Ty::Slice(elem) if self.holds_ref(ty) => {
                RegionTy::Tuple(vec![self.fresh(elem, supply)])
            }
            Ty::Adt(def, args) if self.holds_ref(ty) => {
                let args = args.iter().map(|a| self.fresh(a, supply)).collect();
                RegionTy::adt(*def, args, self.items)
            }
            _ => RegionTy::Other,
        }
    }

    /// [`RegionTy::uniform`] with `origin` on every written reference (none when `None`:
    /// the type writes none), and each type parameter's argument region type.
    fn uniform(&self, ty: &Ty, origin: Option<Origin>) -> RegionTy {
        match ty {
            Ty::Param(p) => self.arg(*p),
            Ty::Ref(m, inner) => match origin {
                Some(o) => RegionTy::Ref(*m, o, Box::new(self.uniform(inner, origin))),
                None => RegionTy::Other,
            },
            Ty::Tuple(elems) if self.holds_ref(ty) => {
                RegionTy::Tuple(elems.iter().map(|e| self.uniform(e, origin)).collect())
            }
            Ty::Array(elem, _) | Ty::Slice(elem) if self.holds_ref(ty) => {
                RegionTy::Tuple(vec![self.uniform(elem, origin)])
            }
            Ty::Adt(def, args) if self.holds_ref(ty) => {
                let args = args.iter().map(|a| self.uniform(a, origin)).collect();
                RegionTy::adt(*def, args, self.items)
            }
            _ => RegionTy::Other,
        }
    }
}
