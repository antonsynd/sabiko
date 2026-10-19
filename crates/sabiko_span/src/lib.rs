//! Source files, byte spans and line/column mapping.
//!
//! A [`Span`] is a half-open byte range `[start, end)` inside one source file,
//! identified by a [`FileId`]. A [`SourceMap`] owns the files and converts byte
//! offsets into line/column positions for diagnostics.

use std::fmt;

/// Identifies a file registered in a [`SourceMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// A half-open byte range inside a source file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    #[must_use]
    pub const fn new(file: FileId, start: u32, end: u32) -> Self {
        debug_assert!(start <= end);
        Self { file, start, end }
    }

    /// An empty span at `offset`, used for "expected X here" diagnostics.
    #[must_use]
    pub const fn point(file: FileId, offset: u32) -> Self {
        Self {
            file,
            start: offset,
            end: offset,
        }
    }

    /// The smallest span covering both `self` and `other`. Both must be in the same file.
    #[must_use]
    pub fn to(self, other: Span) -> Span {
        debug_assert_eq!(
            self.file, other.file,
            "cannot join spans from different files"
        );
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    #[must_use]
    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    #[must_use]
    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}..{}", self.file.0, self.start, self.end)
    }
}

/// A value paired with the span it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub const fn new(node: T, span: Span) -> Self {
        Self { node, span }
    }
}

/// A 1-based line and column position (column counted in UTF-8 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// One registered source file.
#[derive(Debug)]
pub struct SourceFile {
    pub id: FileId,
    pub name: String,
    pub src: String,
    /// Byte offset of the start of each line.
    line_starts: Vec<u32>,
}

impl SourceFile {
    fn new(id: FileId, name: String, src: String) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(u32::try_from(i + 1).expect("source file larger than 4 GiB"));
            }
        }
        Self {
            id,
            name,
            src,
            line_starts,
        }
    }

    /// Converts a byte offset to a 1-based line/column.
    #[must_use]
    pub fn line_col(&self, offset: u32) -> LineCol {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_u32 = u32::try_from(line).expect("more than u32::MAX lines");
        LineCol {
            line: line_u32 + 1,
            col: offset - self.line_starts[line] + 1,
        }
    }

    /// The source text of the given 1-based line, without its terminator.
    #[must_use]
    pub fn line_text(&self, line: u32) -> &str {
        let idx = (line - 1) as usize;
        let start = self.line_starts[idx] as usize;
        let end = self
            .line_starts
            .get(idx + 1)
            .map_or(self.src.len(), |&e| e as usize);
        self.src[start..end].trim_end_matches(['\n', '\r'])
    }

    #[must_use]
    pub fn line_count(&self) -> u32 {
        u32::try_from(self.line_starts.len()).expect("more than u32::MAX lines")
    }

    #[must_use]
    pub fn text(&self, span: Span) -> &str {
        debug_assert_eq!(span.file, self.id);
        &self.src[span.range()]
    }
}

/// Owns all source files of a compilation session.
#[derive(Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: impl Into<String>, src: impl Into<String>) -> FileId {
        let id = FileId(u32::try_from(self.files.len()).expect("more than u32::MAX files"));
        self.files
            .push(SourceFile::new(id, name.into(), src.into()));
        id
    }

    #[must_use]
    pub fn get(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_mapping() {
        let mut sm = SourceMap::new();
        let id = sm.add("a.sabi", "fn Main() {\n    let x = 1;\n}\n");
        let f = sm.get(id);
        assert_eq!(f.line_col(0), LineCol { line: 1, col: 1 });
        assert_eq!(f.line_col(12), LineCol { line: 2, col: 1 });
        assert_eq!(f.line_col(16), LineCol { line: 2, col: 5 });
        assert_eq!(f.line_text(2), "    let x = 1;");
        assert_eq!(f.line_count(), 4);
    }

    #[test]
    fn span_join_and_text() {
        let mut sm = SourceMap::new();
        let id = sm.add("a.sabi", "hello world");
        let a = Span::new(id, 0, 5);
        let b = Span::new(id, 6, 11);
        assert_eq!(sm.get(id).text(a.to(b)), "hello world");
        assert_eq!(a.len(), 5);
        assert_eq!(Span::point(id, 3).len(), 0);
        assert!(Span::point(id, 3).is_empty(), "point spans are empty");
    }
}
