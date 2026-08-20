//! Clipboard-copy format identity shared by the frontends. The
//! formatting helpers themselves stay app-side; this is just the
//! kind enum so framework-neutral events (template panel copy
//! actions) can name a format without depending on a UI crate.

/// Every format the app knows how to render a selection / field as.
/// The Value-prefixed variants only make sense for scalar nodes
/// (known integer width + signedness) -- the byte variants work on
/// any span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyKind {
    BytesLossyUtf8,
    BytesHexSpaced,
    BytesHexCompact,
    BytesDecimalCsv,
    BytesOctalCsv,
    BytesCArray,
    BytesRustArray,
    /// Standard base64 (RFC 4648) over the raw selection bytes.
    BytesBase64,
    /// Lossy UTF-8 decode of the selection, then base64 over that
    /// text. Non-UTF-8 bytes become U+FFFD before encoding, so the
    /// output round-trips cleanly through "base64 -d" as valid UTF-8.
    TextBase64,
    ValueHex,
    ValueDecimal,
    ValueOctal,
    /// Render a parsed struct node (from the template panel) as a
    /// Rust struct literal with inline `field: value` initialisers.
    /// Requires tree context, so it's handled outside the plain
    /// byte formatter (see the app's `format_template_struct`).
    StructRust,
    /// Same idea, but as a C99 designated initialiser block
    /// (`.field = value`).
    StructC,
}

impl CopyKind {
    pub fn is_value(self) -> bool {
        matches!(self, Self::ValueHex | Self::ValueDecimal | Self::ValueOctal)
    }

    /// True for the struct-literal variants -- those need the
    /// template tree and are handled outside the plain byte /
    /// scalar formatters.
    pub fn is_struct(self) -> bool {
        matches!(self, Self::StructRust | Self::StructC)
    }
}
