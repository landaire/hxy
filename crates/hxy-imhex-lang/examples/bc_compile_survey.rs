//! Walk every `.hexpat` file under the user's corpus checkout, try
//! to lex+parse+`bc::compile` it, and tally how many succeed vs.
//! the breakdown of failure reasons. Used to decide what feature
//! to lower next: each improvement should bump the success count.
//!
//! Run with:
//! ```
//! cargo run --release --example bc_compile_survey -p hxy-imhex-lang -- \
//!     ~/src/ImHex-Patterns/patterns
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use hxy_imhex_lang::DirImportResolver;
use hxy_imhex_lang::bc;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn main() {
    let mut args = std::env::args().skip(1);
    let root: PathBuf =
        args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/Users/lander/src/ImHex-Patterns/patterns"));

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read_dir {root:?}: {e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("hexpat"))
        .collect();
    entries.sort();

    // Imports look up under `<corpus>/../includes` (where the
    // upstream `std/` library lives) and the corpus root itself.
    let includes = root.parent().map(|p| p.join("includes")).unwrap_or_else(|| root.clone());
    let resolver = MultiResolver { a: DirImportResolver::new(includes), b: DirImportResolver::new(root.clone()) };

    let mut compiled = 0usize;
    let mut parse_failed = 0usize;
    let mut lex_failed = 0usize;
    let mut errors_by_reason: BTreeMap<String, usize> = BTreeMap::new();
    let compiled_names: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());

    for path in &entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let tokens = match tokenize(&src) {
            Ok(t) => t,
            Err(_) => {
                lex_failed += 1;
                continue;
            }
        };
        let ast = match parse(tokens) {
            Ok(a) => a,
            Err(_) => {
                parse_failed += 1;
                continue;
            }
        };
        match bc::compile_with_resolver(&ast, &resolver) {
            Ok(_) => {
                compiled += 1;
                compiled_names.borrow_mut().push(name);
            }
            Err(e) => {
                let key = format!("{e}");
                *errors_by_reason.entry(key).or_insert(0) += 1;
            }
        }
    }

    let total = entries.len();
    println!("Corpus survey: {root:?}");
    println!("  total templates:  {total}");
    println!("  lex failed:       {lex_failed}");
    println!("  parse failed:     {parse_failed}");
    println!("  bc compiled:      {compiled} / {total}");
    println!();
    println!("  compiled templates:");
    for n in compiled_names.borrow().iter() {
        println!("    {n}");
    }
    println!();
    println!("  top compile-failure reasons:");
    let mut by_count: Vec<(String, usize)> = errors_by_reason.into_iter().collect();
    by_count.sort_by(|a, b| b.1.cmp(&a.1));
    for (reason, count) in by_count.iter().take(15) {
        println!("    {count:4}  {reason}");
    }
}

/// Two-base import resolver. `DirImportResolver` itself only takes
/// one base; the survey wants both the `includes/` (std lib) and
/// the corpus root so corpus-internal imports resolve too.
struct MultiResolver {
    a: DirImportResolver,
    b: DirImportResolver,
}

impl hxy_imhex_lang::ImportResolver for MultiResolver {
    fn resolve(&self, segments: &[String]) -> Option<String> {
        self.a.resolve(segments).or_else(|| self.b.resolve(segments))
    }
}
