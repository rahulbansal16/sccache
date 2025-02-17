// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::cache::{Cache, CacheRead, CacheWrite, Storage};
use crate::lru_disk_cache::Error as LruError;
use crate::lru_disk_cache::LruDiskCache;
use crate::util::SpawnExt;
use futures_03::executor::ThreadPool;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::errors::*;

enum LazyDiskCache {
    Uninit { root: OsString, max_size: u64 },
    Init(LruDiskCache),
}

impl LazyDiskCache {
    fn get_or_init(&mut self) -> Result<&mut LruDiskCache> {
        match self {
            LazyDiskCache::Uninit { root, max_size } => {
                *self = LazyDiskCache::Init(LruDiskCache::new(&root, *max_size)?);
                self.get_or_init()
            }
            LazyDiskCache::Init(d) => Ok(d),
        }
    }

    fn get(&mut self) -> Option<&mut LruDiskCache> {
        match self {
            LazyDiskCache::Uninit { .. } => None,
            LazyDiskCache::Init(d) => Some(d),
        }
    }

    fn path(&self) -> &Path {
        match self {
            LazyDiskCache::Uninit { root, .. } => root.as_ref(),
            LazyDiskCache::Init(d) => d.path(),
        }
    }
}

/// A cache that stores entries at local disk paths.
pub struct DiskCache {
    /// `LruDiskCache` does all the real work here.
    lru: Arc<Mutex<LazyDiskCache>>,
    /// Thread pool to execute disk I/O
    pool: ThreadPool,
}

impl DiskCache {
    /// Create a new `DiskCache` rooted at `root`, with `max_size` as the maximum cache size on-disk, in bytes.
    pub fn new<T: AsRef<OsStr>>(root: T, max_size: u64, pool: &ThreadPool) -> DiskCache {
        DiskCache {
            //TODO: change this function to return a Result
            lru: Arc::new(Mutex::new(LazyDiskCache::Uninit {
                root: root.as_ref().to_os_string(),
                max_size,
            })),
            pool: pool.clone(),
        }
    }
}

/// Make a path to the cache entry with key `key`.
fn make_key_path(key: &str) -> PathBuf {
    Path::new(&key[0..1]).join(&key[1..2]).join(key)
}

impl Storage for DiskCache {
    fn get(&self, key: &str) -> SFuture<Cache> {
        trace!("DiskCache::get({})", key);
        let path = make_key_path(key);
        let lru = self.lru.clone();
        let key = key.to_owned();
        Box::new(self.pool.spawn_fn(move || {
            let mut lru = lru.lock().unwrap();
            let f = match lru.get_or_init()?.get(&path) {
                Ok(f) => f,
                Err(LruError::FileNotInCache) => {
                    trace!("DiskCache::get({}): FileNotInCache", key);
                    return Ok(Cache::Miss);
                }
                Err(LruError::Io(e)) => {
                    trace!("DiskCache::get({}): IoError: {:?}", key, e);
                    return Err(e.into());
                }
                Err(_) => unreachable!(),
            };
            let hit = CacheRead::from(f)?;
            Ok(Cache::Hit(hit))
        }))
    }

    fn put(&self, key: &str, entry: CacheWrite) -> SFuture<Duration> {
        // We should probably do this on a background thread if we're going to buffer
        // everything in memory...
        trace!("DiskCache::finish_put({})", key);
        let lru = self.lru.clone();
        let key = make_key_path(key);
        Box::new(self.pool.spawn_fn(move || {
            let start = Instant::now();
            let v = entry.finish()?;
            lru.lock().unwrap().get_or_init()?.insert_bytes(key, &v)?;
            Ok(start.elapsed())
        }))
    }

    fn location(&self) -> String {
        format!("Local disk: {:?}", self.lru.lock().unwrap().path())
    }

    fn current_size(&self) -> SFuture<Option<u64>> {
        f_ok(self.lru.lock().unwrap().get().map(|l| l.size()))
    }
    fn max_size(&self) -> SFuture<Option<u64>> {
        f_ok(self.lru.lock().unwrap().get().map(|l| l.capacity()))
    }
}
