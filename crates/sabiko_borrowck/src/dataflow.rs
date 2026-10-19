//! A small forward dataflow framework over MIR bodies.
//!
//! An [`Analysis`] describes a lattice of [`BitSet`]s (one bit per local, or per anything
//! else indexed densely) together with its transfer functions; [`iterate_to_fixpoint`]
//! runs a worklist over the CFG until the entry state of every block stabilises, and a
//! [`ResultsCursor`] replays the transfer functions inside a block to recover the state
//! before any individual statement or terminator.
//!
//! The design is a cut-down version of `rustc_mir_dataflow`: enough for the
//! initialisation analyses of this phase and the move analysis of the next.

use std::collections::VecDeque;

use sabiko_mir::{BasicBlock, Body, Statement, Terminator};

/// A fixed-size set of small integers, stored as 64-bit words.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BitSet {
    words: Vec<u64>,
    len: usize,
}

impl BitSet {
    /// The empty set over a domain of `len` elements.
    #[must_use]
    pub fn new_empty(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    /// The full set over a domain of `len` elements.
    #[must_use]
    pub fn new_filled(len: usize) -> Self {
        let mut set = Self::new_empty(len);
        set.insert_all();
        set
    }

    /// The number of elements in the domain (not the number of members).
    #[must_use]
    pub fn domain_size(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn contains(&self, index: usize) -> bool {
        assert!(index < self.len, "index {index} out of domain {}", self.len);
        self.words[index / 64] & (1 << (index % 64)) != 0
    }

    /// Adds `index`; returns whether the set changed.
    pub fn insert(&mut self, index: usize) -> bool {
        assert!(index < self.len, "index {index} out of domain {}", self.len);
        let word = &mut self.words[index / 64];
        let before = *word;
        *word |= 1 << (index % 64);
        *word != before
    }

    /// Removes `index`; returns whether the set changed.
    pub fn remove(&mut self, index: usize) -> bool {
        assert!(index < self.len, "index {index} out of domain {}", self.len);
        let word = &mut self.words[index / 64];
        let before = *word;
        *word &= !(1 << (index % 64));
        *word != before
    }

    pub fn insert_all(&mut self) {
        for word in &mut self.words {
            *word = u64::MAX;
        }
        self.clear_excess_bits();
    }

    pub fn clear(&mut self) {
        for word in &mut self.words {
            *word = 0;
        }
    }

    /// `self |= other`; returns whether `self` changed.
    pub fn union_with(&mut self, other: &BitSet) -> bool {
        self.combine(other, |a, b| a | b)
    }

    /// `self &= other`; returns whether `self` changed.
    pub fn intersect_with(&mut self, other: &BitSet) -> bool {
        self.combine(other, |a, b| a & b)
    }

    /// `self -= other`; returns whether `self` changed.
    pub fn subtract(&mut self, other: &BitSet) -> bool {
        self.combine(other, |a, b| a & !b)
    }

    /// The members, in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.len).filter(|&i| self.contains(i))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    fn combine(&mut self, other: &BitSet, op: impl Fn(u64, u64) -> u64) -> bool {
        assert_eq!(self.len, other.len, "bitsets over different domains");
        let mut changed = false;
        for (a, &b) in self.words.iter_mut().zip(&other.words) {
            let new = op(*a, b);
            changed |= new != *a;
            *a = new;
        }
        changed
    }

    /// Zeroes the bits past `len` in the last word so that whole-word comparisons stay
    /// meaningful.
    fn clear_excess_bits(&mut self) {
        let excess = self.len % 64;
        if excess != 0 {
            if let Some(last) = self.words.last_mut() {
                *last &= (1u64 << excess) - 1;
            }
        }
    }
}

impl std::fmt::Debug for BitSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// A position in a body: the statement at `statement_index` of `block`, or the block's
/// terminator when `statement_index == statements.len()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Location {
    pub block: BasicBlock,
    pub statement_index: usize,
}

impl Location {
    #[must_use]
    pub fn new(block: BasicBlock, statement_index: usize) -> Self {
        Self {
            block,
            statement_index,
        }
    }

    /// The location of `block`'s terminator.
    #[must_use]
    pub fn terminator_of(body: &Body, block: BasicBlock) -> Self {
        Self::new(block, body.block(block).statements.len())
    }
}

/// A forward dataflow problem over bitsets.
///
/// Implementations supply the lattice (`bottom_value`, `join`) and the transfer
/// functions. Transfer functions must be monotone for the fixpoint to terminate.
pub trait Analysis {
    /// Number of bits in the domain; the same for every state of this analysis.
    fn domain_size(&self, body: &Body) -> usize;

    /// The state of a block no predecessor has flowed into yet: the lattice's bottom
    /// (empty for a "maybe" analysis, full for a "definitely" analysis).
    fn bottom_value(&self, body: &Body) -> BitSet;

    /// Sets up the state on entry to the start block, starting from `bottom_value`.
    fn initialize_start_block(&self, body: &Body, state: &mut BitSet);

    /// Merges the state at the end of a predecessor into a block's entry state. Returns
    /// whether `into` changed.
    fn join(&self, into: &mut BitSet, from: &BitSet) -> bool;

    /// Applies the effect of a statement.
    fn apply_statement(&self, state: &mut BitSet, statement: &Statement, location: Location);

    /// Applies the effect of a terminator, as seen by every successor.
    fn apply_terminator(&self, state: &mut BitSet, terminator: &Terminator, location: Location);
}

/// The fixpoint of an analysis: the state on entry to every block.
#[derive(Clone, Debug)]
pub struct Results {
    entry_sets: Vec<BitSet>,
}

impl Results {
    /// The state on entry to `block`.
    #[must_use]
    pub fn entry_set(&self, block: BasicBlock) -> &BitSet {
        &self.entry_sets[block.index()]
    }
}

/// Runs `analysis` over `body` to a fixpoint with a worklist.
pub fn iterate_to_fixpoint<A: Analysis>(body: &Body, analysis: &A) -> Results {
    let bottom = analysis.bottom_value(body);
    let mut entry_sets = vec![bottom; body.blocks.len()];
    analysis.initialize_start_block(body, &mut entry_sets[BasicBlock::START.index()]);

    let mut queue = VecDeque::from([BasicBlock::START]);
    let mut queued = vec![false; body.blocks.len()];
    queued[BasicBlock::START.index()] = true;

    while let Some(bb) = queue.pop_front() {
        queued[bb.index()] = false;
        let mut state = entry_sets[bb.index()].clone();
        apply_block(
            body,
            analysis,
            bb,
            &mut state,
            body.block(bb).statements.len(),
        );
        let Some(terminator) = &body.block(bb).terminator else {
            continue;
        };
        analysis.apply_terminator(&mut state, terminator, Location::terminator_of(body, bb));
        for succ in terminator.kind.successors() {
            if analysis.join(&mut entry_sets[succ.index()], &state) && !queued[succ.index()] {
                queued[succ.index()] = true;
                queue.push_back(succ);
            }
        }
    }
    Results { entry_sets }
}

/// Applies the statements `0..upto` of `bb` to `state`.
fn apply_block<A: Analysis>(
    body: &Body,
    analysis: &A,
    bb: BasicBlock,
    state: &mut BitSet,
    upto: usize,
) {
    for (i, statement) in body.block(bb).statements[..upto].iter().enumerate() {
        analysis.apply_statement(state, statement, Location::new(bb, i));
    }
}

/// Replays an analysis inside a block, exposing the state before any location.
///
/// Seeking forward within one block is incremental; any other seek restarts from the
/// block's entry state.
#[derive(Debug)]
pub struct ResultsCursor<'a, A: Analysis> {
    body: &'a Body,
    analysis: &'a A,
    results: &'a Results,
    state: BitSet,
    /// The block the cursor is in and how many of its statements have been applied.
    position: Option<(BasicBlock, usize)>,
}

impl<'a, A: Analysis> ResultsCursor<'a, A> {
    #[must_use]
    pub fn new(body: &'a Body, analysis: &'a A, results: &'a Results) -> Self {
        Self {
            body,
            analysis,
            results,
            state: analysis.bottom_value(body),
            position: None,
        }
    }

    /// Positions the cursor so that [`get`](Self::get) is the state just before the
    /// statement or terminator at `location`.
    pub fn seek_before(&mut self, location: Location) {
        let Location {
            block,
            statement_index,
        } = location;
        let applied = match self.position {
            Some((bb, applied)) if bb == block && applied <= statement_index => applied,
            _ => {
                self.state.clone_from(self.results.entry_set(block));
                0
            }
        };
        for (i, statement) in self.body.block(block).statements[applied..statement_index]
            .iter()
            .enumerate()
        {
            self.analysis.apply_statement(
                &mut self.state,
                statement,
                Location::new(block, applied + i),
            );
        }
        self.position = Some((block, statement_index));
    }

    /// The state at the position last sought to.
    #[must_use]
    pub fn get(&self) -> &BitSet {
        &self.state
    }

    #[must_use]
    pub fn results(&self) -> &'a Results {
        self.results
    }

    #[must_use]
    pub fn analysis(&self) -> &'a A {
        self.analysis
    }
}

#[cfg(test)]
mod tests {
    use super::BitSet;

    #[test]
    fn bitset_basics() {
        let mut a = BitSet::new_empty(70);
        assert!(a.is_empty());
        assert!(a.insert(3));
        assert!(!a.insert(3));
        assert!(a.insert(69));
        assert!(a.contains(3) && a.contains(69) && !a.contains(4));
        assert_eq!(a.iter().collect::<Vec<_>>(), vec![3, 69]);
        assert!(a.remove(3));
        assert!(!a.remove(3));

        let full = BitSet::new_filled(70);
        assert_eq!(full.iter().count(), 70);
        let mut b = full.clone();
        assert!(b.intersect_with(&a));
        assert_eq!(b, a);
        assert!(!b.union_with(&a));
        let mut c = full.clone();
        assert!(c.subtract(&a));
        assert!(!c.contains(69) && c.contains(0));
        assert_eq!(format!("{a:?}"), "{69}");
    }
}
