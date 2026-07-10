//! Source spans and module identity.

/// Identifies a loaded module within an instance's module table.
///
/// Assigned from the host resolver's canonical module id (engine spec E§6);
/// the front end and machine reference modules by this `Copy` handle rather
/// than by name (machine-design §2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ModuleId(pub u32);

/// A half-open byte range `[start, end)` into a module's NFC-normalized source.
///
/// Offsets are byte positions (not code-point or grapheme indices) into the
/// source the engine has already normalized to NFC (S-1). Diagnostics
/// translate them to 1-based line/column at the boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    /// Byte offset of the first byte of the span.
    pub start: u32,
    /// Byte offset one past the last byte of the span.
    pub end: u32,
}

impl Span {
    /// A zero-width span at offset 0, for synthesized nodes with no source.
    pub const DUMMY: Span = Span { start: 0, end: 0 };

    /// Creates a span covering the half-open byte range `[start, end)`.
    pub fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }

    /// The span's length in bytes.
    pub fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the span is zero-width.
    pub fn is_empty(self) -> bool {
        self.end <= self.start
    }
}
