# This is a local POSIX-host target: Cargo provides the Rust toolchain and
# dependency resolution, while Buck2 tracks the complete Cargo input set and
# exposes the resulting desktop binary as //:hxy. A remote/hermetic build
# would need a declared Cargo toolchain and vendored registry and Git dependencies.
genrule(
    name = "hxy",
    out = "hxy",
    srcs = glob([
        "Cargo.lock",
        "Cargo.toml",
        "crates/**",
        "rust-toolchain.toml",
        "vendor/**",
    ]),
    cmd = """
        cargo build --locked --package hxy --bin hxy --target-dir \"$TMP/cargo-target\"
        cp \"$TMP/cargo-target/debug/hxy\" \"$OUT\"
    """,
)
