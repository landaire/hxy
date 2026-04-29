//! Byte-range cache shared across hex views, template runs, and any
//! other consumer that reads from a [`HexSource`].
//!
//! The cache is keyed by `(SourceId, ChunkIndex)`: every read request
//! is rounded out to fixed-size chunks, served from the in-memory map
//! when present, and otherwise fetched once from the underlying
//! source. Eviction is byte-budget LRU: once cached bytes would
//! exceed [`CacheLimit`], the oldest chunk is dropped until the
//! incoming chunk fits.
//!
//! Attribution carries no semantic weight for the read itself -- it's
//! only used for the debug "memory by view" panel. Attribution is
//! recorded the first time a chunk lands in the cache; subsequent hits
//! by other consumers don't re-attribute. So a 100 MiB chunk fetched
//! by the hex view and later read by a template run still shows up
//! under the hex view's bucket.
//!
//! Sharing across views: callers obtain a [`SourceId`] from
//! [`ByteCache::alloc_source_id`] once per logical source (typically
//! per open-file id) and pass that same id through every
//! [`CachedSource`] that wraps the source, regardless of which
//! attribution they use. Two consumers with the same id share chunks;
//! two with different ids do not.

use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use lru::LruCache;
use rustc_hash::FxHashMap;

use crate::error::Error;
use crate::error::Result;
use crate::geometry::ByteLen;
use crate::geometry::ByteOffset;
use crate::geometry::ByteRange;
use crate::source::HexSource;

/// Process-wide identifier for a logical byte source. Allocated by
/// [`ByteCache::alloc_source_id`]; opaque to callers, who hand it back
/// to the cache on every read so chunks can be keyed by source.
///
/// The same id can be reused across multiple [`CachedSource`] handles
/// that wrap the same underlying bytes (e.g. a hex view and a
/// template run against the same file). `NonZeroU64` so [`Option`]
/// niches the discriminant to zero overhead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct SourceId(NonZeroU64);

impl SourceId {
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Index of a chunk within a source. Chunk `n` covers bytes
/// `[n * CHUNK_SIZE, (n + 1) * CHUNK_SIZE)`, clamped at the source's
/// length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ChunkIndex(u64);

impl ChunkIndex {
    pub fn get(self) -> u64 {
        self.0
    }

    fn start_offset(self, chunk_size: ChunkSize) -> u64 {
        self.0 * chunk_size.get()
    }
}

/// Cache chunk size in bytes. Fixed at build time so cache keys don't
/// have to carry it; tuned for SSDs and the hex view's row reads.
pub const CHUNK_SIZE_BYTES: u64 = 64 * 1024;

/// Newtype wrapper for the chunk size so it can't be confused with
/// other byte counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct ChunkSize(u64);

impl ChunkSize {
    pub const DEFAULT: Self = Self(CHUNK_SIZE_BYTES);

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Cache size budget in bytes.
///
/// Stored as MiB at the user-facing layer (settings file, debug
/// panel) and converted to bytes on the way in. The newtype keeps
/// "MiB" and "bytes" from getting swapped at call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheLimit {
    bytes: u64,
}

impl CacheLimit {
    pub const MIN_MIB: u32 = 20;
    pub const DEFAULT_MIB: u32 = 500;

    pub fn from_mib(mib: u32) -> Self {
        let mib = mib.max(Self::MIN_MIB);
        Self { bytes: u64::from(mib) * 1024 * 1024 }
    }

    pub fn as_bytes(self) -> u64 {
        self.bytes
    }

    pub fn as_mib(self) -> u32 {
        (self.bytes / (1024 * 1024)) as u32
    }
}

impl Default for CacheLimit {
    fn default() -> Self {
        Self::from_mib(Self::DEFAULT_MIB)
    }
}

/// Opaque per-file key used to attribute hex-view bytes back to the
/// originating tab in the debug panel. The host wraps a `FileId` here
/// when wiring up the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct HexViewKey(pub u64);

/// Same idea for an in-flight template run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct TemplateKey(pub u64);

/// Stable per-plugin attribution. Kept separately from
/// [`HexViewKey`] / [`TemplateKey`] so the debug panel can roll all
/// plugin reads up under a single "Plugins" header without losing
/// the per-plugin breakdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PluginKey(pub u32);

/// Who brought a cached chunk into memory. Used only for the debug
/// "memory by view" panel; reads succeed regardless of attribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Attribution {
    HexView(HexViewKey),
    Template(TemplateKey),
    Plugin(PluginKey),
}

/// Snapshot of cache state for the debug panel. Cheap to compute --
/// the cache builds it under the lock and returns a owned struct.
#[derive(Clone, Debug, Default)]
pub struct CacheStats {
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub chunk_count: usize,
    /// Bytes attributed per (Attribution, SourceId) tuple. The host
    /// is free to roll these up however it wants in the UI; the
    /// cache itself doesn't know about file names or plugin labels.
    pub by_attribution: Vec<AttributionBytes>,
    /// Cumulative reads that hit a chunk already in the cache.
    pub hits: u64,
    /// Cumulative reads that triggered a fetch from the underlying
    /// source.
    pub misses: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributionBytes {
    pub attribution: Attribution,
    pub source: SourceId,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ChunkKey {
    source: SourceId,
    chunk: ChunkIndex,
}

struct ChunkSlot {
    bytes: Arc<[u8]>,
    /// Attribution recorded when this chunk was first inserted. Used
    /// to debit the right bucket on eviction.
    attribution: Attribution,
}

struct CacheInner {
    limit: CacheLimit,
    used_bytes: u64,
    chunks: LruCache<ChunkKey, ChunkSlot>,
    bytes_by_attribution: FxHashMap<(Attribution, SourceId), u64>,
    hits: u64,
    misses: u64,
}

/// LRU byte cache shared across views. Wrap with `Arc` so multiple
/// [`CachedSource`] handles can share the same backing store.
pub struct ByteCache {
    next_source_id: AtomicU64,
    inner: Mutex<CacheInner>,
}

impl ByteCache {
    pub fn new(limit: CacheLimit) -> Arc<Self> {
        let cap = lru_capacity_for(limit);
        Arc::new(Self {
            next_source_id: AtomicU64::new(1),
            inner: Mutex::new(CacheInner {
                limit,
                used_bytes: 0,
                chunks: LruCache::new(cap),
                bytes_by_attribution: FxHashMap::default(),
                hits: 0,
                misses: 0,
            }),
        })
    }

    /// Allocate a fresh [`SourceId`]. Hosts should call this once per
    /// logical source and reuse the id across every [`CachedSource`]
    /// that reads from the same bytes.
    pub fn alloc_source_id(&self) -> SourceId {
        let raw = self.next_source_id.fetch_add(1, Ordering::Relaxed);
        let nz = NonZeroU64::new(raw).expect("source-id counter never reaches zero");
        SourceId(nz)
    }

    /// Replace the cache budget. Evicts LRU chunks immediately until
    /// the new budget is honoured.
    pub fn set_limit(&self, limit: CacheLimit) {
        let mut g = self.inner.lock().expect("byte cache lock poisoned");
        g.limit = limit;
        let new_budget = limit.as_bytes();
        while g.used_bytes > new_budget {
            let Some((key, slot)) = g.chunks.pop_lru() else {
                break;
            };
            let n = slot.bytes.len() as u64;
            g.used_bytes = g.used_bytes.saturating_sub(n);
            decrement_attribution(&mut g.bytes_by_attribution, slot.attribution, key.source, n);
        }
        g.chunks.resize(lru_capacity_for(limit));
    }

    /// Drop every chunk associated with `source_id`. Use when a file
    /// is closed or its bytes have been replaced (reload from disk,
    /// patch flush, etc.) so stale chunks don't linger or get served
    /// to a freshly-opened source that happened to reuse the id.
    pub fn drop_source(&self, source_id: SourceId) {
        let mut g = self.inner.lock().expect("byte cache lock poisoned");
        let keys: Vec<ChunkKey> =
            g.chunks.iter().filter_map(|(k, _)| if k.source == source_id { Some(*k) } else { None }).collect();
        for key in keys {
            let Some(slot) = g.chunks.pop(&key) else {
                continue;
            };
            let n = slot.bytes.len() as u64;
            g.used_bytes = g.used_bytes.saturating_sub(n);
            decrement_attribution(&mut g.bytes_by_attribution, slot.attribution, key.source, n);
        }
    }

    /// Snapshot stats for the debug panel.
    pub fn stats(&self) -> CacheStats {
        let g = self.inner.lock().expect("byte cache lock poisoned");
        let mut by_attribution: Vec<AttributionBytes> = g
            .bytes_by_attribution
            .iter()
            .map(|(&(attribution, source), &bytes)| AttributionBytes { attribution, source, bytes })
            .collect();
        by_attribution.sort_by(|a, b| b.bytes.cmp(&a.bytes));
        CacheStats {
            used_bytes: g.used_bytes,
            limit_bytes: g.limit.as_bytes(),
            chunk_count: g.chunks.len(),
            by_attribution,
            hits: g.hits,
            misses: g.misses,
        }
    }

    /// Read `range` from `inner_source`, going through the cache.
    /// Out-of-range reads bypass the cache and surface
    /// [`Error::OutOfBounds`] verbatim.
    pub fn read_with(
        &self,
        range: ByteRange,
        source_id: SourceId,
        attribution: Attribution,
        inner_source: &dyn HexSource,
    ) -> Result<Vec<u8>> {
        let source_len = inner_source.len();
        let source_end = ByteOffset::new(source_len.get());
        if range.end() > source_end {
            return Err(Error::OutOfBounds { range, len: source_end });
        }
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let chunk_size = ChunkSize::DEFAULT;
        let start_chunk = ChunkIndex(range.start().get() / chunk_size.get());
        let end_offset = range.end().get();
        let last_chunk = ChunkIndex((end_offset - 1) / chunk_size.get());
        let mut out = Vec::with_capacity(range.len().get() as usize);
        let mut chunk = start_chunk;
        loop {
            let chunk_bytes = self.fetch_chunk(source_id, chunk, attribution, inner_source, source_len, chunk_size)?;
            let chunk_start = chunk.start_offset(chunk_size);
            let chunk_end = chunk_start + chunk_bytes.len() as u64;
            let take_start = range.start().get().max(chunk_start) - chunk_start;
            let take_end = range.end().get().min(chunk_end) - chunk_start;
            out.extend_from_slice(&chunk_bytes[take_start as usize..take_end as usize]);
            if chunk == last_chunk {
                break;
            }
            chunk = ChunkIndex(chunk.get() + 1);
        }
        Ok(out)
    }

    fn fetch_chunk(
        &self,
        source_id: SourceId,
        chunk: ChunkIndex,
        attribution: Attribution,
        inner_source: &dyn HexSource,
        source_len: ByteLen,
        chunk_size: ChunkSize,
    ) -> Result<Arc<[u8]>> {
        let key = ChunkKey { source: source_id, chunk };
        {
            let mut g = self.inner.lock().expect("byte cache lock poisoned");
            if let Some(slot) = g.chunks.get(&key) {
                let bytes = Arc::clone(&slot.bytes);
                g.hits += 1;
                return Ok(bytes);
            }
        }
        let chunk_start = chunk.start_offset(chunk_size);
        let chunk_end = (chunk_start + chunk_size.get()).min(source_len.get());
        let read_range = ByteRange::new(ByteOffset::new(chunk_start), ByteOffset::new(chunk_end))?;
        let bytes: Arc<[u8]> = inner_source.read(read_range)?.into();
        let chunk_bytes_count = bytes.len() as u64;
        let mut g = self.inner.lock().expect("byte cache lock poisoned");
        g.misses += 1;
        if let Some(slot) = g.chunks.get(&key) {
            return Ok(Arc::clone(&slot.bytes));
        }
        let budget = g.limit.as_bytes();
        while g.used_bytes + chunk_bytes_count > budget {
            let Some((evicted_key, evicted_slot)) = g.chunks.pop_lru() else {
                break;
            };
            let n = evicted_slot.bytes.len() as u64;
            g.used_bytes = g.used_bytes.saturating_sub(n);
            decrement_attribution(&mut g.bytes_by_attribution, evicted_slot.attribution, evicted_key.source, n);
        }
        let slot = ChunkSlot { bytes: Arc::clone(&bytes), attribution };
        g.chunks.put(key, slot);
        g.used_bytes += chunk_bytes_count;
        *g.bytes_by_attribution.entry((attribution, source_id)).or_insert(0) += chunk_bytes_count;
        Ok(bytes)
    }
}

/// LRU entry-count bound sized for the byte budget. The cache evicts
/// by bytes, but the underlying [`LruCache`] is always entry-bounded;
/// we keep a small slack on top of the byte budget so a transient
/// over-fill while inserting + evicting can still complete.
fn lru_capacity_for(limit: CacheLimit) -> NonZeroUsize {
    let by_bytes = (limit.as_bytes() / CHUNK_SIZE_BYTES) as usize;
    let with_slack = by_bytes.saturating_add(16);
    NonZeroUsize::new(with_slack.max(1)).expect("non-zero by max")
}

fn decrement_attribution(
    map: &mut FxHashMap<(Attribution, SourceId), u64>,
    attribution: Attribution,
    source: SourceId,
    amount: u64,
) {
    let key = (attribution, source);
    let std::collections::hash_map::Entry::Occupied(mut e) = map.entry(key) else {
        return;
    };
    let v = e.get_mut();
    *v = v.saturating_sub(amount);
    if *v == 0 {
        e.remove();
    }
}

/// Thin [`HexSource`] adapter that routes every read through a shared
/// [`ByteCache`] under one attribution.
///
/// The same underlying bytes can be wrapped multiple times with
/// different attributions so the debug panel can break apart
/// "this file's hex view" from "this file's template run". Pass the
/// same [`SourceId`] to each handle so they share chunks instead of
/// double-caching.
pub struct CachedSource {
    cache: Arc<ByteCache>,
    source_id: SourceId,
    attribution: Attribution,
    inner: Arc<dyn HexSource>,
    len: ByteLen,
}

impl CachedSource {
    pub fn new(
        cache: Arc<ByteCache>,
        source_id: SourceId,
        attribution: Attribution,
        inner: Arc<dyn HexSource>,
    ) -> Arc<Self> {
        let len = inner.len();
        Arc::new(Self { cache, source_id, attribution, inner, len })
    }

    pub fn source_id(&self) -> SourceId {
        self.source_id
    }

    pub fn attribution(&self) -> Attribution {
        self.attribution
    }

    pub fn cache(&self) -> &Arc<ByteCache> {
        &self.cache
    }

    pub fn inner(&self) -> &Arc<dyn HexSource> {
        &self.inner
    }
}

impl HexSource for CachedSource {
    fn len(&self) -> ByteLen {
        self.len
    }

    fn read(&self, range: ByteRange) -> Result<Vec<u8>> {
        self.cache.read_with(range, self.source_id, self.attribution, self.inner.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemorySource;

    fn cache_with_limit_mib(mib: u32) -> Arc<ByteCache> {
        ByteCache::new(CacheLimit::from_mib(mib))
    }

    fn range(start: u64, end: u64) -> ByteRange {
        ByteRange::new(ByteOffset::new(start), ByteOffset::new(end)).expect("valid range")
    }

    #[test]
    fn round_trip_single_chunk() {
        let bytes: Vec<u8> = (0..200u8).collect();
        let inner: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes.clone()));
        let cache = cache_with_limit_mib(20);
        let id = cache.alloc_source_id();
        let attribution = Attribution::HexView(HexViewKey(7));
        let cached = CachedSource::new(cache.clone(), id, attribution, inner);
        assert_eq!(cached.read(range(10, 30)).expect("read"), bytes[10..30]);
        assert_eq!(cached.read(range(0, 200)).expect("read"), bytes);
    }

    #[test]
    fn read_spans_multiple_chunks() {
        let len = (CHUNK_SIZE_BYTES * 3) as usize;
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let inner: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes.clone()));
        let cache = cache_with_limit_mib(20);
        let id = cache.alloc_source_id();
        let cached = CachedSource::new(cache.clone(), id, Attribution::HexView(HexViewKey(1)), inner);
        let want = (CHUNK_SIZE_BYTES - 100)..(CHUNK_SIZE_BYTES * 2 + 50);
        let got = cached.read(range(want.start, want.end)).expect("read");
        assert_eq!(got, bytes[want.start as usize..want.end as usize]);
    }

    #[test]
    fn second_read_is_a_hit() {
        let bytes: Vec<u8> = vec![42u8; (CHUNK_SIZE_BYTES + 100) as usize];
        let inner: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let cache = cache_with_limit_mib(20);
        let id = cache.alloc_source_id();
        let cached = CachedSource::new(cache.clone(), id, Attribution::HexView(HexViewKey(1)), inner);
        let _ = cached.read(range(0, CHUNK_SIZE_BYTES + 50)).expect("read");
        let _ = cached.read(range(10, CHUNK_SIZE_BYTES + 50)).expect("read");
        let stats = cache.stats();
        assert!(stats.hits > 0);
        assert_eq!(stats.misses, 2);
    }

    #[test]
    fn budget_evicts_lru() {
        let inner_bytes = vec![1u8; (CHUNK_SIZE_BYTES * 4) as usize];
        let inner: Arc<dyn HexSource> = Arc::new(MemorySource::new(inner_bytes));
        let cache = ByteCache::new(CacheLimit { bytes: CHUNK_SIZE_BYTES * 2 });
        let id = cache.alloc_source_id();
        let cached =
            CachedSource::new(cache.clone(), id, Attribution::HexView(HexViewKey(1)), inner.clone());
        // Read all four chunks; cache holds at most two.
        for c in 0..4u64 {
            let s = c * CHUNK_SIZE_BYTES;
            let e = s + CHUNK_SIZE_BYTES;
            cached.read(range(s, e)).expect("read");
        }
        let stats = cache.stats();
        assert_eq!(stats.chunk_count, 2);
        assert!(stats.used_bytes <= CHUNK_SIZE_BYTES * 2);
    }

    #[test]
    fn shared_source_id_means_shared_chunks() {
        let bytes = vec![3u8; (CHUNK_SIZE_BYTES + 10) as usize];
        let inner: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let cache = cache_with_limit_mib(20);
        let id = cache.alloc_source_id();
        let view_a = CachedSource::new(cache.clone(), id, Attribution::HexView(HexViewKey(1)), inner.clone());
        let view_b = CachedSource::new(cache.clone(), id, Attribution::Template(TemplateKey(2)), inner);
        let _ = view_a.read(range(0, 50)).expect("read");
        let _ = view_b.read(range(0, 50)).expect("read");
        let stats = cache.stats();
        // One miss for the first reader, then hits for the rest.
        assert_eq!(stats.misses, 1);
        assert!(stats.hits >= 1);
    }

    #[test]
    fn drop_source_releases_chunks() {
        let bytes = vec![5u8; (CHUNK_SIZE_BYTES + 10) as usize];
        let inner: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let cache = cache_with_limit_mib(20);
        let id = cache.alloc_source_id();
        let cached = CachedSource::new(cache.clone(), id, Attribution::HexView(HexViewKey(9)), inner);
        cached.read(range(0, 100)).expect("read");
        assert!(cache.stats().used_bytes > 0);
        cache.drop_source(id);
        let stats = cache.stats();
        assert_eq!(stats.used_bytes, 0);
        assert_eq!(stats.chunk_count, 0);
    }

    #[test]
    fn shrinking_limit_evicts_immediately() {
        let bytes = vec![9u8; (CHUNK_SIZE_BYTES * 4) as usize];
        let inner: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let cache = cache_with_limit_mib(20);
        let id = cache.alloc_source_id();
        let cached = CachedSource::new(cache.clone(), id, Attribution::HexView(HexViewKey(1)), inner);
        cached.read(range(0, CHUNK_SIZE_BYTES * 4)).expect("read");
        assert_eq!(cache.stats().chunk_count, 4);
        cache.set_limit(CacheLimit { bytes: CHUNK_SIZE_BYTES });
        let stats = cache.stats();
        assert!(stats.chunk_count <= 1);
        assert!(stats.used_bytes <= CHUNK_SIZE_BYTES);
    }
}
