//! Import resolution for ImHex pattern files.
//!
//! ImHex spells imports two ways: `import std.sys;` (language-level)
//! and `#include <std/sys.pat>` (preprocessor-style). The lexer
//! drops the `#include` form as trivia; the language form lands in
//! [`crate::ast::Stmt::Import`] and the interpreter resolves it
//! through an [`ImportResolver`].
//!
//! Two resolvers ship in this crate:
//! - [`NoImportResolver`] -- the default when the host hasn't
//!   wired one up. Returns `None` for every path so the runtime
//!   surfaces `undefined name` / `unknown type` for std references
//!   instead of looking like a successful zero-import run.
//! - [`DirImportResolver`] -- looks paths up under a base
//!   directory by translating `import a.b.c` into `<base>/a/b/c.pat`
//!   (or `.hexpat`). Used by the application after fetching the
//!   ImHex patterns repo into `.imhex-patterns/` at runtime.
//!
//! The lang crate does not assume a filesystem layout. Hosts that
//! need a different lookup strategy (in-memory bundles, network
//! fetch) implement the trait directly.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// Wraps a path-to-source lookup. Implementations should be
/// idempotent; the interpreter caches the resulting AST per
/// resolved path to avoid re-parsing on repeated imports.
pub trait ImportResolver: Send + Sync {
    /// Resolve a `import a.b.c;` path (segments `["a", "b", "c"]`)
    /// to the source bytes of the imported file. Returns `None` if
    /// the resolver doesn't know about the path.
    fn resolve(&self, segments: &[String]) -> Option<String>;
}

/// Stub resolver. Every lookup misses; useful as a default when
/// the host hasn't supplied a real resolver.
pub struct NoImportResolver;

impl ImportResolver for NoImportResolver {
    fn resolve(&self, _segments: &[String]) -> Option<String> {
        None
    }
}

/// Disk-backed resolver. `import a.b.c` maps to `<base>/a/b/c.pat`,
/// then `<base>/a/b/c.hexpat` if the first miss. Both extensions
/// are pattern-language source -- the upstream `std/` library uses
/// `.pat`, application templates tend to use `.hexpat`.
pub struct DirImportResolver {
    base: PathBuf,
}

impl DirImportResolver {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }
}

impl ImportResolver for DirImportResolver {
    fn resolve(&self, segments: &[String]) -> Option<String> {
        if segments.is_empty() {
            return None;
        }
        let mut path = self.base.clone();
        for seg in segments {
            path.push(seg);
        }
        for ext in ["pat", "hexpat"] {
            let candidate = path.with_extension(ext);
            if let Ok(s) = fs::read_to_string(&candidate) {
                return Some(s);
            }
        }
        // Some patterns ship the leaf as `<seg>/<seg>.pat` -- a
        // directory + same-named entry point. Try that shape too.
        let last = segments.last().unwrap();
        let nested = path.join(last);
        for ext in ["pat", "hexpat"] {
            let candidate = nested.with_extension(ext);
            if let Ok(s) = fs::read_to_string(&candidate) {
                return Some(s);
            }
        }
        None
    }
}

/// Convenience: a resolver wrapped in an [`Arc`] so it can be
/// cheaply cloned into the interpreter without forcing the host
/// to manage lifetimes manually.
pub type SharedResolver = Arc<dyn ImportResolver>;

/// Build a resolver that walks each base directory in turn,
/// returning the first hit. Lets a host point at the application
/// data dir and a per-template directory simultaneously.
pub fn chained_resolver(bases: impl IntoIterator<Item = impl Into<PathBuf>>) -> SharedResolver {
    let resolvers: Vec<Box<dyn ImportResolver>> =
        bases.into_iter().map(|p| Box::new(DirImportResolver::new(p)) as Box<dyn ImportResolver>).collect();
    Arc::new(ChainResolver { resolvers })
}

struct ChainResolver {
    resolvers: Vec<Box<dyn ImportResolver>>,
}

impl ImportResolver for ChainResolver {
    fn resolve(&self, segments: &[String]) -> Option<String> {
        for r in &self.resolvers {
            if let Some(s) = r.resolve(segments) {
                return Some(s);
            }
        }
        None
    }
}

// `Path` is intentionally only re-exported for `DirImportResolver::new`'s
// generics doc-comment; the linker doesn't actually need a use.
#[allow(dead_code)]
fn _path_marker(_: &Path) {}
