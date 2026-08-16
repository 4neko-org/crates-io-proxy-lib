use std::{fmt, fs, io::{self, ErrorKind}, path::{Path, PathBuf}, time::Duration};

use url::{ParseError, Url};

use crate::CratesIoProxyConsts;

/// Proxy server configuration
#[derive(Debug, Clone)]
pub struct ProxyConfig 
{
    /// Upstream registry index URL (defaults to [`INDEX_CRATES_IO_URL`])
    pub(crate) index_url: Url,

    /// Upstream crate download URL (defaults to [`CRATES_IO_URL`])
    pub(crate) upstream_url: Url,

    /// External URL of this proxy server (defaults to [`DEFAULT_PROXY_URL`])
    pub(crate) proxy_url: Url,

    /// Registry index cache directory (defaults to [`DEFAULT_CACHE_DIR`])
    pub(crate) index_dir: PathBuf,

    /// Crate files cache directory (defaults to [`DEFAULT_CACHE_DIR`])
    pub(crate) crates_dir: PathBuf,

    /// Index entry cache Time-to-Live (defaults to [`DEFAULT_CACHE_TTL_SECS`])
    pub(crate) cache_ttl: Duration,
}

impl fmt::Display for ProxyConfig
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result 
    {
        write!(f, "index_url: {}, upstream_url: {}, proxy_url: {}, index_dir: {} \
            crates_dir: {}, cache_ttl: {}sec", self.index_url, self.upstream_url, 
            self.proxy_url, self.index_dir.display(), self.crates_dir.display(),
            self.cache_ttl.as_secs())
    }
}

impl ProxyConfig
{
    pub 
    fn new() -> Self
    {
        let cache_dir = Path::new(CratesIoProxyConsts::DEFAULT_CACHE_DIR);

        Self
        {
            index_url: 
                Url::parse(&CratesIoProxyConsts::INDEX_CRATES_IO_URL).unwrap(),
            upstream_url: 
                Url::parse(&CratesIoProxyConsts::CRATES_IO_URL).unwrap(),
            proxy_url: 
                Url::parse(&CratesIoProxyConsts::DEFAULT_PROXY_URL).unwrap(),
            index_dir: 
                cache_dir.join("index"),
            crates_dir:
                cache_dir.join("crates"),
            cache_ttl: 
                CratesIoProxyConsts::DEFAULT_CACHE_TTL_SECS
        }
    }

    pub 
    fn set_index_url<P: AsRef<str>>(mut self, index_url: P) -> Result<Self, ParseError>
    {
        self.index_url = Url::parse(index_url.as_ref())?;

        return Ok(self);
    }

    pub 
    fn set_upstream_url<P: AsRef<str>>(mut self, upstream_url: P) -> Result<Self, ParseError>
    {
        self.upstream_url = Url::parse(upstream_url.as_ref())?;

        return Ok(self);
    }

    pub 
    fn set_proxy_url<P: AsRef<str>>(mut self, proxy_url: P) -> Result<Self, ParseError>
    {
        self.proxy_url = Url::parse(proxy_url.as_ref())?;

        return Ok(self);
    }

    pub 
    fn set_cache_dir<P: AsRef<Path>>(mut self, cache_dir: P) -> Self
    {
        let cache_dir = cache_dir.as_ref();
        self.index_dir = cache_dir.join("index");
        self.crates_dir = cache_dir.join("crates");
        
        return self;
    }

    pub 
    fn set_cache_ttl(mut self, cache_ttl: Duration) -> Self
    {
        self.cache_ttl = cache_ttl;

        return self;
    }

    /// Checks if path exists.
    pub 
    fn test(&self) -> io::Result<()>
    {
        if self.index_dir.exists() == false
        {
            return Err(io::Error::new(ErrorKind::NotFound, self.index_dir.display().to_string()));
        }

        if self.crates_dir.exists() == false
        {
            return Err(io::Error::new(ErrorKind::NotFound, self.crates_dir.display().to_string()));
        }

        return Ok(());
    }

    /// Attempts to create directories if does not exist.
    pub 
    fn create_dirs(&self) -> io::Result<()>
    {
        if self.index_dir.exists() == false
        {
            fs::create_dir_all(self.index_dir.as_path())?;
        }

        if self.crates_dir.exists() == false
        {
            fs::create_dir_all(self.crates_dir.as_path())?;
        }

        return Ok(());
    }
}

