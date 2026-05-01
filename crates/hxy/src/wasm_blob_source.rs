//! `HexSource` impl over a browser `web_sys::File`.
//!
//! The browser's File API is async (`Blob.slice` + `FileReader.
//! readAsArrayBuffer`), but `HexSource::read` is sync. The bridge:
//! every read first checks an in-memory chunk cache; if every chunk
//! covering the requested range is present, the read serves bytes
//! directly. Otherwise it returns [`hxy_core::Error::NotPrimed`] so
//! the caller can decide what to do (typically: render a placeholder
//! and async-prime the missing range from a `spawn_local` task).
//!
//! Priming is the caller's job. `WasmBlobSource::prime(range).await`
//! fetches every chunk covering `range` (each via one round-trip
//! through `FileReader`) and stores it. Subsequent sync reads inside
//! that range succeed without further async work.
//!
//! Chunks are 64 KiB ([`hxy_core::CHUNK_SIZE_BYTES`]), matching the
//! [`hxy_core::ByteCache`] grain so the two cache layers don't fight
//! over alignment.

use std::collections::HashMap;
use std::sync::Arc;

use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::CHUNK_SIZE_BYTES;
use hxy_core::Error;
use hxy_core::HexSource;
use hxy_core::Result;
use parking_lot::Mutex;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use web_sys::js_sys;

pub struct WasmBlobSource {
    handle: rfd::FileHandle,
    len: ByteLen,
    chunks: Mutex<HashMap<u64, Arc<[u8]>>>,
}

// `web_sys::File` (via `rfd::FileHandle`) is `!Send + !Sync` because
// JS values can't cross thread boundaries. `wasm32-unknown-unknown`
// without `--target-feature=+atomics` is single-threaded, so this
// is sound -- there are no other threads that could observe a
// non-thread-safe `JsValue`. If hxy ever adopts wasm threads, this
// impl must be revisited (likely by routing reads through a single
// owning worker over a postMessage channel).
#[allow(unsafe_code)]
unsafe impl Send for WasmBlobSource {}
#[allow(unsafe_code)]
unsafe impl Sync for WasmBlobSource {}

impl WasmBlobSource {
    /// Wrap an `rfd::FileHandle`. Length is read synchronously from
    /// `Blob.size()` -- the file contents are *not* fetched here.
    pub fn new(handle: rfd::FileHandle) -> Self {
        let size = handle.inner().size();
        let len = if size.is_finite() && size >= 0.0 { size as u64 } else { 0 };
        Self { handle, len: ByteLen::new(len), chunks: Mutex::new(HashMap::new()) }
    }

    /// Number of chunks currently cached. For diagnostics only.
    pub fn cached_chunk_count(&self) -> usize {
        self.chunks.lock().len()
    }

    /// Fetch every chunk covering `range` and store it in the cache.
    /// Idempotent: chunks already cached are skipped. Returns
    /// `Error::OutOfBounds` if `range` extends past the file.
    pub async fn prime(&self, range: ByteRange) -> Result<()> {
        let src_end = ByteOffset::new(self.len.get());
        if range.end() > src_end {
            return Err(Error::OutOfBounds { range, len: src_end });
        }
        for chunk_idx in chunk_indices(range) {
            if self.chunks.lock().contains_key(&chunk_idx) {
                continue;
            }
            let bytes = self.fetch_chunk(chunk_idx).await?;
            self.chunks.lock().insert(chunk_idx, bytes);
        }
        Ok(())
    }

    async fn fetch_chunk(&self, chunk_idx: u64) -> Result<Arc<[u8]>> {
        let start = chunk_idx * CHUNK_SIZE_BYTES;
        let end = (start + CHUNK_SIZE_BYTES).min(self.len.get());
        let chunk_range = ByteRange::new(ByteOffset::new(start), ByteOffset::new(end))?;
        let blob = self
            .handle
            .inner()
            .slice_with_f64_and_f64(start as f64, end as f64)
            .map_err(|e| Error::Io { range: chunk_range, source: js_to_io_error(e) })?;
        let bytes = read_blob(&blob).await.map_err(|e| Error::Io { range: chunk_range, source: e })?;
        Ok(Arc::from(bytes))
    }
}

impl HexSource for WasmBlobSource {
    fn len(&self) -> ByteLen {
        self.len
    }

    fn read(&self, range: ByteRange) -> Result<Vec<u8>> {
        let src_end = ByteOffset::new(self.len.get());
        if range.end() > src_end {
            return Err(Error::OutOfBounds { range, len: src_end });
        }
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let chunks = self.chunks.lock();
        // Verify every required chunk is present before assembling the
        // result. Reporting the first missing chunk lets the caller
        // prime exactly what's needed without reading-then-erroring on
        // the next read.
        for idx in chunk_indices(range) {
            if !chunks.contains_key(&idx) {
                return Err(Error::NotPrimed { range, missing_chunk: idx });
            }
        }
        let mut out = Vec::with_capacity(range.len().get() as usize);
        let mut cursor = range.start().get();
        let end = range.end().get();
        while cursor < end {
            let idx = cursor / CHUNK_SIZE_BYTES;
            let chunk_start = idx * CHUNK_SIZE_BYTES;
            let chunk = chunks.get(&idx).expect("checked above");
            let off_in_chunk = (cursor - chunk_start) as usize;
            let take = ((end - cursor) as usize).min(chunk.len() - off_in_chunk);
            out.extend_from_slice(&chunk[off_in_chunk..off_in_chunk + take]);
            cursor += take as u64;
        }
        Ok(out)
    }
}

fn chunk_indices(range: ByteRange) -> impl Iterator<Item = u64> {
    let start = range.start().get();
    let end = range.end().get();
    if start >= end {
        return 0..0;
    }
    let first = start / CHUNK_SIZE_BYTES;
    let last = (end - 1) / CHUNK_SIZE_BYTES;
    first..last + 1
}

async fn read_blob(blob: &web_sys::Blob) -> std::io::Result<Vec<u8>> {
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        let reader = match web_sys::FileReader::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = reject.call1(&JsValue::undefined(), &e);
                return;
            }
        };
        let r_for_load = reader.clone();
        let onload: Closure<dyn FnMut()> = Closure::new(move || {
            let result = r_for_load.result().unwrap_or(JsValue::null());
            let _ = resolve.call1(&JsValue::undefined(), &result);
        });
        let r_for_err = reader.clone();
        let reject_clone = reject.clone();
        let onerror: Closure<dyn FnMut()> = Closure::new(move || {
            let err = r_for_err.error().map(JsValue::from).unwrap_or(JsValue::null());
            let _ = reject_clone.call1(&JsValue::undefined(), &err);
        });
        reader.set_onload(Some(onload.as_ref().unchecked_ref()));
        reader.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        // Closures live for the FileReader's lifetime; `forget` is the
        // documented pattern for one-shot wasm-bindgen callbacks
        // (rfd uses the same trick in its own File reader code).
        onload.forget();
        onerror.forget();
        if let Err(e) = reader.read_as_array_buffer(blob) {
            let _ = reject.call1(&JsValue::undefined(), &e);
        }
    });
    let result = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(|e| std::io::Error::other(format!("FileReader failed: {e:?}")))?;
    let buffer = js_sys::Uint8Array::new(&result);
    let mut out = vec![0u8; buffer.length() as usize];
    buffer.copy_to(&mut out[..]);
    Ok(out)
}

fn js_to_io_error(err: JsValue) -> std::io::Error {
    std::io::Error::other(format!("{err:?}"))
}
