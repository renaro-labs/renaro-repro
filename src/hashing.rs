//! Streaming BLAKE3 hashing (bounded memory, multi-threaded) and the on-disk hash cache.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Read buffer per hashing thread. Memory use is `BUF * threads`, independent of file size.
const BUF: usize = 1 << 20;
/// Above this chunk size BLAKE3's own multi-threading pays off.
const RAYON_MIN: usize = 128 * 1024;
/// Files smaller than this are not worth caching.
const CACHE_MIN_SIZE: u64 = 1 << 20;

/// BLAKE3 hasher that can be used as an `io::Write` sink.
pub struct HashWriter {
    hasher: blake3::Hasher,
    len: u64,
}

impl HashWriter {
    pub fn new() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            len: 0,
        }
    }

    pub fn finish(&self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }
}

impl Default for HashWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() >= RAYON_MIN {
            self.hasher.update_rayon(buf);
        } else {
            self.hasher.update(buf);
        }
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Hashes a file with streaming reads. Returns `(size, hex digest)`.
pub fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut buf = vec![0u8; BUF];
    let mut w = HashWriter::new();
    loop {
        let n = read_full(&mut f, &mut buf)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
    }
    Ok((w.len, w.finish()))
}

pub fn hash_bytes(b: &[u8]) -> String {
    blake3::hash(b).to_hex().to_string()
}

/// Runs `f` over `items` on a few worker threads, keeping the input order in the result.
pub fn parallel_map<T, R, F>(items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 4)
        .min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= items.len() {
                    break;
                }
                let r = f(&items[i]);
                results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .map(|r| r.expect("every item is processed"))
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct CacheEntry {
    size: u64,
    mtime_ns: u128,
    blake3: String,
}

/// Cache of file hashes keyed by `(absolute path, size, mtime)`, stored in
/// `.repro-cache/hashes.json` so reruns over large unchanged inputs are fast.
pub struct HashCache {
    path: Option<PathBuf>,
    entries: Mutex<BTreeMap<String, CacheEntry>>,
    dirty: Mutex<bool>,
}

impl HashCache {
    /// A cache stored under `root/.repro-cache/`.
    pub fn open(root: &Path) -> Self {
        let path = root.join(".repro-cache").join("hashes.json");
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries: Mutex::new(entries),
            dirty: Mutex::new(false),
        }
    }

    /// A cache that never persists anything.
    pub fn disabled() -> Self {
        Self {
            path: None,
            entries: Mutex::new(BTreeMap::new()),
            dirty: Mutex::new(false),
        }
    }

    pub fn hash(&self, path: &Path) -> Result<(u64, String)> {
        if self.path.is_none() {
            return hash_file(path);
        }
        let meta =
            std::fs::metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
        let size = meta.len();
        let mtime = meta.modified().ok();
        let key = path.to_string_lossy().to_string();
        let mtime_ns = mtime
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos());
        if let (Some(mtime_ns), true) = (mtime_ns, size >= CACHE_MIN_SIZE) {
            if let Some(e) = self.lock_entries().get(&key) {
                if e.size == size && e.mtime_ns == mtime_ns {
                    return Ok((size, e.blake3.clone()));
                }
            }
        }
        let (n, digest) = hash_file(path)?;
        // Skip files modified in the last 2 s: their mtime may not change on the next write.
        let settled = mtime
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > Duration::from_secs(2));
        if let (Some(mtime_ns), true, true) = (mtime_ns, n >= CACHE_MIN_SIZE, settled) {
            self.lock_entries().insert(
                key,
                CacheEntry {
                    size: n,
                    mtime_ns,
                    blake3: digest.clone(),
                },
            );
            *self.dirty.lock().unwrap_or_else(|e| e.into_inner()) = true;
        }
        Ok((n, digest))
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, CacheEntry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes the cache back if it changed. Failures are ignored: the cache is an optimization.
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if !*self.dirty.lock().unwrap_or_else(|e| e.into_inner()) {
            return;
        }
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let Ok(json) = serde_json::to_vec_pretty(&*self.lock_entries()) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_hash_matches_one_shot() {
        let dir = std::env::temp_dir().join(format!("repro-hash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("big.bin");
        let data: Vec<u8> = (0..(3 * BUF + 12345)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let (n, h) = hash_file(&p).unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(h, hash_bytes(&data));
        assert_eq!(h.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_map_keeps_order() {
        let v: Vec<u32> = (0..100).collect();
        let r = parallel_map(&v, |x| x * 2);
        assert_eq!(r, (0..100).map(|x| x * 2).collect::<Vec<_>>());
    }
}
