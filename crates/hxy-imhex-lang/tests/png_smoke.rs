//! Run the upstream PNG hexpat against a real PNG checked into the
//! repo and assert the chunk walk reaches every chunk including IEND.
//! Skipped when the user's imhex-patterns directory isn't installed
//! (we don't vendor the GPL'd corpus).

use std::path::PathBuf;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::Value;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::extract_pragmas;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn imhex_root() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let p = PathBuf::from(home).join("Library/Application Support/hxy/imhex-patterns");
    p.is_dir().then_some(p)
}

#[test]
fn png_template_walks_all_chunks() {
    let Some(root) = imhex_root() else {
        eprintln!("skipping: imhex-patterns not installed at $HOME/Library/Application Support/hxy/imhex-patterns");
        return;
    };
    let pat_path = root.join("patterns/png.hexpat");
    let png_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("img/command_palette.png");
    if !pat_path.is_file() {
        eprintln!("skipping: missing png.hexpat at {}", pat_path.display());
        return;
    }
    assert!(png_path.is_file(), "missing test fixture {} -- the PNG is checked into the repo", png_path.display());

    let src = std::fs::read_to_string(&pat_path).expect("read pat");
    let bytes = std::fs::read(&png_path).expect("read png");
    let tokens = tokenize(&src).unwrap_or_else(|e| panic!("lex: {e}"));
    let program = parse(tokens).unwrap_or_else(|e| panic!("parse: {e}"));
    let pragmas = extract_pragmas(&src);
    let resolver = chained_resolver([root.join("includes"), root.clone()]);
    let mut interp = Interpreter::new(MemorySource::new(bytes)).with_import_resolver(resolver).with_step_limit(1_000_000);
    if let Some(e) = pragmas.endian {
        interp = interp.with_default_endian(e);
    }
    let result = interp.run(&program);

    if let Some(err) = result.terminal_error.as_ref() {
        panic!("interpreter halted: {err:?}");
    }

    // Every emitted `name` field on a chunk_t carries the 4-char chunk
    // tag. We walk those values and count distinct chunks rather than
    // asserting a specific list -- the iCCP / iTXt / eXIf / etc.
    // metadata varies per PNG.
    let chunk_names: Vec<&str> = result
        .nodes
        .iter()
        .filter(|n| n.name == "name")
        .filter_map(|n| if let Value::Str(s) = n.value.as_ref()? { Some(s.as_str()) } else { None })
        .collect();
    assert!(
        chunk_names.first() == Some(&"IHDR"),
        "first chunk should be IHDR (PNG header), got {:?}",
        chunk_names.first()
    );
    assert!(chunk_names.contains(&"IDAT"), "PNG template did not emit any IDAT chunk -- only saw {chunk_names:?}");
    assert!(chunk_names.contains(&"IEND"), "PNG template did not reach IEND -- only saw {chunk_names:?}");

    // Sanity: at least the documented backstop of "more than one"
    // chunk. The iTXt-first command_palette.png fixture used to come
    // back with exactly 1 chunk when `#pragma endian big` was being
    // dropped.
    assert!(
        chunk_names.len() > 3,
        "expected many chunks in command_palette.png, got {}: {:?}",
        chunk_names.len(),
        chunk_names
    );

    // The PNG template renames every chunk_t instance via
    // `[[name(chunkValueName(this))]]`, which evaluates the
    // `chunkValueName` user function with the chunk as `this` and
    // returns the chunk's 4-char tag. Without struct-decl-attr
    // evaluation in read_struct, that attribute is dropped and the
    // panel shows array indices ("[0]", "[1]", ...) instead of
    // "IDAT", "IEND", etc.
    let renamed_chunks: Vec<&str> = result
        .nodes
        .iter()
        .filter_map(|n| {
            n.attrs.iter().find_map(|(k, v)| (k == "hxy_name" && !v.is_empty()).then_some(v.as_str()))
        })
        .collect();
    assert!(renamed_chunks.contains(&"IDAT"), "expected hxy_name=IDAT among renamed chunks, got {renamed_chunks:?}");
    assert!(renamed_chunks.contains(&"IEND"), "expected hxy_name=IEND among renamed chunks, got {renamed_chunks:?}");

    // Every chunk's `length` field in the PNG template carries
    // `[[color("17BECF")]]`. The lang now canonicalises that to
    // `hxy_color`; spot-check that the length leaves are the ones
    // tinted, not just "some node somewhere".
    let length_color_count = result
        .nodes
        .iter()
        .filter(|n| n.name == "length")
        .filter(|n| n.attrs.iter().any(|(k, v)| k == "hxy_color" && v == "17BECF"))
        .count();
    assert!(
        length_color_count >= 4,
        "expected most chunk length fields to carry hxy_color=17BECF, got {length_color_count}"
    );

    // Comment promotion: ihdr_t has `u32 width [[comment("Image width")]]`.
    let any_comment = result
        .nodes
        .iter()
        .any(|n| n.attrs.iter().any(|(k, v)| k == "hxy_comment" && v == "Image width"));
    assert!(any_comment, "expected width field to carry hxy_comment=\"Image width\"");
}
