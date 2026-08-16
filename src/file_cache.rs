//! Index entry and crate file cache helpers

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, DirEntry, File, create_dir_all, metadata, read, write};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;


use crate::{CratesIoProxyLog, ProxyConfig};

use super::{CrateInfo, IndexEntry};

/// A stat for each crate file.
#[derive(Debug)]
pub struct CacheStatsCrate
{
    file_name: String, 
    len_bytes: u64,
    created_mtime: SystemTime,
    modified_mtime: SystemTime
}

// todo
impl fmt::Display for CacheStatsCrate
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result 
    {
        write!(f, "{} {} bytes, c: {:?}, m: {:?}", 
            self.file_name, 
            self.len_bytes, 
            self.created_mtime.duration_since(SystemTime::UNIX_EPOCH),
            self.modified_mtime.duration_since(SystemTime::UNIX_EPOCH)
        )
    }
}

impl TryFrom<&DirEntry> for CacheStatsCrate
{
    type Error = io::Error;

    fn try_from(dir_ent: &DirEntry) -> Result<Self, Self::Error> 
    {
        let metadata = dir_ent.metadata()?;

        return Ok(
            Self
            {
                file_name: 
                    dir_ent.file_name().to_string_lossy().to_string(),
                len_bytes: 
                    metadata.size(),
                created_mtime: 
                    metadata.created()?,
                modified_mtime: 
                    metadata.modified()?
            }
        );
    }
}

/// Calculates the cache size, and returns all filenames of crates there.
#[derive(Debug, Default)]
pub struct CacheStats
{
    /// total length
    total_bytes: u64,

    /// separate file.
    file_names: Vec<CacheStatsCrate>,
}

impl fmt::Display for CacheStats
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result 
    {
        writeln!(f, "total: {}", self.total_bytes)?;

        writeln!(f, "{}", self.file_names.iter().map(|d| d.to_string()).collect::<Vec<String>>().join("\r"))
    }
}

impl CacheStats
{
    pub 
    fn cache_stat<LOG: CratesIoProxyLog>(log: &LOG, cfg: &ProxyConfig) -> io::Result<Self>
    {
        let dirs = fs::read_dir(cfg.crates_dir.as_path())?;

        let mut cs = Self::default();

        for file in dirs
        {
            if let Err(e) = file
            {
                log.send_error(format_args!("cache_stat: read dir entry error: {}", e));
                continue;
            }

            let file = file.unwrap();

            if file.path().extension() != Some(OsStr::new("crate"))
            {
                log.send_debug(format_args!("cache_stat: skipping non-crate file: {}", file.path().display()));
                continue;
            }

            let csc = CacheStatsCrate::try_from(&file);

            if let Err(e) = csc
            {
                log.send_error(
                    format_args!("cache_stat: cannot get metadata for file: {} entry error: {}", file.path().display(), e)
                );
                continue;
            }

            let csc = csc.unwrap();

            cs.total_bytes += csc.len_bytes;
            cs.file_names.push(csc);
        }

        return Ok(cs);
    }
}

/// Caches the crate package file on the local filesystem.
pub 
fn cache_store_crate(dir: &Path, crate_info: &CrateInfo, data: &[u8]) -> io::Result<()>
{
    let crate_file_path = dir.join(crate_info.to_file_path());

    // Create all parent directories first.
    create_dir_all(crate_file_path.parent().unwrap())
        .map_err(|e|
            io::Error::new(e.kind(), format!("cache: failed to create crate directory: {e}"))
        )?;

    write(crate_file_path, data)
        .map_err(|e|
            io::Error::new(e.kind(), format!("cache: failed to write crate file: {e}"))
        )?;

    return Ok(());
}

/// Fetches the cached crate package file from the local filesystem, if present.
pub 
fn cache_fetch_crate(dir: &Path, crate_info: &CrateInfo) -> Option<Vec<u8>> 
{
    read(dir.join(crate_info.to_file_path())).ok()
}

/// Caches the index entry file on the local filesystem.
pub 
fn cache_store_index_entry(dir: &Path, entry: &IndexEntry, data: &[u8]) -> io::Result<()>
{
    let entry_file_path = dir.join(entry.to_file_path());

    create_dir_all(entry_file_path.parent().unwrap()) 
        .map_err(|e|
            io::Error::new(e.kind(), format!("cache: failed to create index directory: {e}"))
        )?;

    let mut file = 
        File::create(entry_file_path) 
            .map_err(|e|
                io::Error::new(e.kind(), format!("cache: failed to create index entry file: {e}"))
            )?;

    file
        .write_all(data) 
        .map_err(|e|
            io::Error::new(e.kind(), format!("cache: failed to write index entry data: {e}"))
        )?;        

    // Set the cache file mtime according to the Last-Modified HTTP metadata.
    if let Some(mtime) = entry.mtime() 
    {
        file
            .set_modified(mtime)
            .map_err(|e|
                io::Error::new(e.kind(), format!("cache: failed to set index entry file mtime: {e}"))
            )?;
    }

    return Ok(());
}

/// Fetches the cached index entry file from the local filesystem, if present.
pub 
fn cache_fetch_index_entry(dir: &Path, entry: &IndexEntry) -> Option<Vec<u8>> 
{
    read(dir.join(entry.to_file_path())).ok()
}

/// Tries to recreate the missing index entry metadata from the cache file metadata.
pub 
fn cache_try_find_index_entry(dir: &Path, name: &str) -> Option<IndexEntry> 
{
    let mut entry = IndexEntry::new(name);

    let mtime = 
        metadata(dir.join(entry.to_file_path()))
            .ok()?
            .modified()
            .ok()?;

    entry.set_mtime(mtime);

    Some(entry)
}
