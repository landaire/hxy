# fuzz

Fuzz targets for the workspace. Uses [`cargo-fuzz`](https://rust-fuzz.github.io/book/cargo-fuzz.html), which requires nightly.

## Targets

| Target | What it hits |
| --- | --- |
| `lang_lexer` | `hxy_010_lang::tokenize` on arbitrary UTF-8. |
| `lang_parser` | `tokenize` + `parse` on arbitrary UTF-8. |
| `lang_interpreter` | Tokenize + parse + `Interpreter::run` on an arbitrary `(template, data)` pair. Step-budget-limited to 50 000 statements. |
| `lang_structured` | `arbitrary`-derived `FuzzProgram` (see `hxy-010-lang/src/fuzz.rs`, gated on the `arbitrary` feature) → `emit()` → tokenize + parse + run. Explores paths that random bytes can't reach cheaply. |

## Running

```sh
cargo +nightly fuzz run lang_lexer -- -max_total_time=60
cargo +nightly fuzz run lang_interpreter -- -max_total_time=60
cargo +nightly fuzz run lang_structured -- -max_total_time=60
```

Fuzz-grown corpus files are gitignored. If you find coverage worth sharing, minimize and commit:

```sh
cargo +nightly fuzz cmin lang_parser
```

Crash reproducers land in `fuzz/artifacts/<target>/` — those **are** tracked so we don't lose them across runs. Reproduce with `cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>`.
