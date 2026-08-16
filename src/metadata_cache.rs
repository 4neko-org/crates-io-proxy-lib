//! Index entry file metadata cache helpers

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use super::IndexEntry;

// Volatile registry index entry metadata cache
/*
static INDEX_CACHE: LazyLock<Mutex<HashMap<String, IndexEntry>>> = 
    LazyLock::new(|| Mutex::new(HashMap::new()));
*/

#[derive(Debug, Clone)]
pub struct CratesIoProxyIndexCache
{
    cache: Arc<Mutex<HashMap<String, IndexEntry>>>
}

impl fmt::Display for CratesIoProxyIndexCache
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result 
    {
        match self.cache.try_lock()
        {
            Ok(v) => 
                write!(f, "cache: {}", v.iter().map(|(k,v)| format!("{}:{}", k, v)).collect::<Vec<String>>().join(", ")),
            Err(e) => 
                write!(f, "cache: {}", e),
        }
    }
}

impl CratesIoProxyIndexCache
{
    pub 
    fn new() -> Self
    {
        Self{ cache: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub 
    fn new_with_capacity(cap: usize) -> Self
    {
        Self{ cache: Arc::new(Mutex::new(HashMap::with_capacity(cap))) }
    }

    /// Caches the index entry metadata in memory.
    pub 
    fn metadata_store_index_entry(&self, entry: &IndexEntry) 
    {
        let name = entry.name().to_owned();

        self.cache.lock().unwrap().insert(name, entry.clone());
    }

    /// Fetches the cached index entry metadata from memory.
    pub 
    fn metadata_fetch_index_entry(&self, name: &str) -> Option<IndexEntry> 
    {
        self.cache.lock().unwrap().get(name).map(ToOwned::to_owned)
    }

    /// Erases the cached index entry metadata from memory.
    pub 
    fn metadata_invalidate_index_entry(&self, entry: &IndexEntry) 
    {
        self.cache.lock().unwrap().remove(entry.name());
    }
}
