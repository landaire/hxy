// One-off diagnostic: run a .bt template against a real binary file.
// `cargo run --example run_bt --release -- <template.bt> <input>`

use std::cell::Cell;
use std::env;
use std::fs;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use hxy_010_lang::HexSource;
use hxy_010_lang::Interpreter;
use hxy_010_lang::SourceError;
use hxy_010_lang::parse;
use hxy_010_lang::tokenize;

#[derive(Default)]
struct Stats {
    reads: Cell<u64>,
    bytes_read: Cell<u64>,
    max_offset: Cell<u64>,
    last_print: Cell<Option<Instant>>,
    started: Cell<Option<Instant>>,
}

struct CountingSource {
    bytes: Vec<u8>,
    stats: Rc<Stats>,
}

impl HexSource for CountingSource {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, SourceError> {
        let end = offset.saturating_add(length);
        if end > self.len() {
            return Err(SourceError::OutOfBounds { offset, end, len: self.len() });
        }
        self.stats.reads.set(self.stats.reads.get() + 1);
        self.stats.bytes_read.set(self.stats.bytes_read.get() + length);
        if end > self.stats.max_offset.get() {
            self.stats.max_offset.set(end);
        }
        let now = Instant::now();
        let started = self.stats.started.get().unwrap_or(now);
        if self.stats.started.get().is_none() {
            self.stats.started.set(Some(started));
        }
        let last = self.stats.last_print.get().unwrap_or(started);
        if now.duration_since(last) >= Duration::from_secs(2) {
            self.stats.last_print.set(Some(now));
            eprintln!(
                "  [{:?}] reads={} bytes={} max_offset={} (cur read at {})",
                now.duration_since(started),
                self.stats.reads.get(),
                self.stats.bytes_read.get(),
                self.stats.max_offset.get(),
                offset,
            );
        }
        Ok(self.bytes[offset as usize..end as usize].to_vec())
    }
}

fn main() {
    let mut args = env::args().skip(1);
    let template_path = args.next().expect("usage: run_bt <template.bt> <input>");
    let input_path = args.next().expect("usage: run_bt <template.bt> <input>");

    let src = fs::read_to_string(&template_path).expect("read template");
    let bytes = fs::read(&input_path).expect("read input");
    println!("template={template_path} input={input_path} input_len={}", bytes.len());

    let tokens = tokenize(&src).expect("tokenize");
    let program = parse(tokens).expect("parse");

    let stats = Rc::new(Stats::default());
    let source = CountingSource { bytes, stats: stats.clone() };
    let len = source.len();

    let started = Instant::now();
    let result = Interpreter::new(source).run(&program);
    let elapsed = started.elapsed();

    println!("---");
    println!("elapsed: {:?}", elapsed);
    println!("nodes emitted: {}", result.nodes.len());
    println!("reads: {}", stats.reads.get());
    println!("bytes_read (sum): {}", stats.bytes_read.get());
    println!("max_offset reached: {} / {}", stats.max_offset.get(), len);
    println!("diagnostics: {}", result.diagnostics.len());
    for d in result.diagnostics.iter().take(20) {
        println!("  {:?}", d);
    }
}
