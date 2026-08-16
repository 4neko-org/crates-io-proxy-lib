//! Caching HTTP proxy server for the `crates.io` registry
//! ======================================================
//!
//! `crates-io-proxy` implements transparent caching for both
//! the sparse registry index at <https://index.crates.io/> and
//! the static crate file download server.
//!
//! Two independent HTTP proxy endpoints are implemented:
//!
//! 1. Listens to HTTP GET requests at `/index/.../{crate}`,
//!    forwards them to <https://index.crates.io/> and caches the downloaded registry
//!    index entries as JSON text files on the local filesystem.
//!
//! 2. Listens to HTTP GET requests at `/api/v1/crates/{crate}/{version}/download`,
//!    forwards them to <https://crates.io/> and caches the downloaded crates as
//!    `.crate` files on the local filesystem.
//!
//! Subsequent sparse registry index and crate download API hits are serviced
//! using the locally cached index entry and crate files.
//!
//! As a convenience feature, the download requests for the `config.json` file
//! found at the sparse index root are served with a replacement file,
//! which changes the crate download URL to point to this same proxy server.

pub extern crate url;

mod config_json;
mod crate_info;
mod file_cache;
mod index_entry;

#[cfg(feature = "async_client")]
pub mod async_impl;

/// Index entry file metadata cache helpers
pub mod metadata_cache;

/// Configuration of the CratesIoProxy
pub mod config;

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;
use std::{env, fmt};
use std::fmt::Display;

use url::Url;

pub use config::ProxyConfig;
pub use file_cache::CacheStats;

#[cfg(feature = "async_client")]
pub use async_impl::
{
    AsyncCratesIoProxyLog, AsyncCratesIoProxyHttpClientError, AsyncCratesIoProxyHttpClientErrorMarker,
    AsyncCratesIoProxyHttpClientResponse, AsyncCratesIoProxyHttpClient, AsyncCratesIoProxyRequest,
    AsyncCratesIoProxy
};

use crate::config_json::{gen_config_json_file, is_config_json_url};
use crate::crate_info::CrateInfo;
use crate::file_cache::{
    cache_fetch_crate, cache_fetch_index_entry, cache_store_crate, cache_store_index_entry,
    cache_try_find_index_entry,
};
use crate::index_entry::IndexEntry;
use crate::metadata_cache::{CratesIoProxyIndexCache};

#[derive(Debug)]
pub struct CratesIoProxyConsts;

impl CratesIoProxyConsts
{
    /// Default listen address and port
    pub const LISTEN_ADDRESS: &str = "0.0.0.0:3080";

    /// Upstream `crates.io` registry index URL
    pub const INDEX_CRATES_IO_URL: &str = "https://index.crates.io/";

    /// Upstream `crates.io` registry URL
    pub const CRATES_IO_URL: &str = "https://crates.io/";

    /// Default external URL of this proxy server
    pub const DEFAULT_PROXY_URL: &str = "http://localhost:3080/";

    /// Sparse registry index access path
    pub const CRATES_INDEX_PATH: &str = "/index/";

    /// Crates download API path
    pub const CRATES_API_PATH: &str = "/api/v1/crates/";

    /// Default crate files cache directory path
    pub const DEFAULT_CACHE_DIR: &str = "/var/cache/crates-io-proxy";

    /// Default index cache entry Time-to-Live in seconds
    pub const DEFAULT_CACHE_TTL_SECS: Duration = Duration::from_secs(3600);

    /// Default index entry download buffer capacity
    pub const INDEX_ENTRY_CAPACITY: usize = 0x10000;

    /// Limit the download item size to 16 MiB
    pub const MAX_CRATE_SIZE: usize = 0x100_0000;

    /// HTTP Content-Type of the registry index entry JSON file
    pub const INDEX_HTTP_CTYPE: &str = "Content-Type: text/plain";

    /// HTTP Content-Type of the crate package file
    pub const CRATE_HTTP_CTYPE: &str = "Content-Type: application/x-tar";

    /// HTTP Content-Type of the crates API JSON response
    pub const JSON_HTTP_CTYPE: &str = "Content-Type: application/json; charset=utf-8";

    /// HTTP Content-Type of the crates API JSON response header name
    pub const JSON_HTTP_CTYPE_HNAME: &str = "Content-Type";

    /// HTTP Content-Type of the crates API JSON response value
    pub const JSON_HTTP_CTYPE_HVAL: &str = "application/json; charset=utf-8";

    pub const JSON_HTTP_CTYPE_PAIR: (Cow<'static, str>, Cow<'static, str>) = 
        (Cow::Borrowed(Self::JSON_HTTP_CTYPE_HNAME), Cow::Borrowed(Self::JSON_HTTP_CTYPE_HVAL));

    /// Program version tag: `"<major>.<minor>.<patch>"`
    pub const VERSION: &str = env!("CARGO_PKG_VERSION");

    /// HTTP client User Agent string
    pub const HTTP_USER_AGENT: &str = concat!("crates-io-proxy/", env!("CARGO_PKG_VERSION"));
}



/// Registry index entry download response
#[derive(Debug)]
struct IndexResponse 
{
    /// Index entry requested + response metadata
    entry: IndexEntry,

    /// HTTP response status code
    status: u16,

    /// HTTP response data
    data: Vec<u8>,
}


/// Makes boxed custom ureq status errors for `download_crate()`.
fn ureq_status_error(max_pkt_size: u64) -> ureq::Error
{
    ureq::Error::BodyExceedsLimit(max_pkt_size)
}


/// This crate does not use any logging crates. Crate emists some
/// debug or info messages. If it is required to catch any implement
/// any of the functions of this trait.
pub trait CratesIoProxyLog: std::marker::Send + 'static
{
    fn send_debug(&self, _args: fmt::Arguments<'_>) {}
    fn send_info(&self, _args: fmt::Arguments<'_>) {}
    fn send_warn(&self, _args: fmt::Arguments<'_>) {}
    fn send_error(&self, _args: fmt::Arguments<'_>) {}
}

/// An instance
#[derive(Debug)]
pub enum CratesIoProxyResponsePacketData
{
    /// A response does not contain body. The HTTP response code must always present.
    None,

    /// A response is a text.
    StringData(String),

    /// A response is a binary.
    ByteData(Vec<u8>),
}

/// A response which contains all necessary information to form a response.
/// 
/// Because crate has no idea what realization of HTTP server is used, it forms
/// this structure.
/// 
/// * For the [CratesIoProxyResponsePacketData::None] an empty response should be 
/// created.
/// 
/// * For the [CratesIoProxyResponsePacketData::StringData] an response `from_string`
/// should be created.
/// 
/// * For the [CratesIoProxyResponsePacketData::ByteData] an respinse `from_data` 
/// should be created.
#[derive(Debug)]
pub struct CratesIoProxyResponsePacket
{
    /// HTTP response code
    pub code: u16,

    /// A headers in format "header_name: header_data" (\r\n is not included)
    pub headers: Vec<(Cow<'static, str>, Cow<'static, str>)>,

    /// A data type attached.
    pub data: CratesIoProxyResponsePacketData
}

impl CratesIoProxyResponsePacket
{
    #[inline]
    pub(crate) 
    fn create_string_response(code: u16, headers: Vec<(Cow<'static, str>, Cow<'static, str>)>, body: String) -> Self
    {
        Self
        {
            code, 
            headers,
            data: 
                CratesIoProxyResponsePacketData::StringData(body),
        }
    }

    #[inline]
    fn create_data_response(data: Vec<u8>) -> Self
    {
        return 
            Self
            {
                code: 
                    200, 
                headers: 
                    vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR],
                data: 
                    CratesIoProxyResponsePacketData::ByteData(data),
            };
    }

    #[inline] 
    pub(crate) 
    fn create_json_response(code: u16, json: String) -> Self
    {
        return 
            Self
            {
                code, 
                headers:
                    vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR],
                data: 
                    CratesIoProxyResponsePacketData::StringData(json),
            };
    }

    #[inline]
    pub(crate) 
    fn create_error_response(code: u16) -> Self
    {
        return 
            Self
            {
                code, 
                headers: 
                    vec![],
                data:
                    CratesIoProxyResponsePacketData::None
            };
    }

    #[inline]
    pub(crate) 
    fn create_index_entry_not_modified_response(entry: &IndexEntry) -> Self
    {
        let mut headers: Vec<(Cow<'_, str>, Cow<'_, str>)> = Vec::with_capacity(2);

        entry.set_index_response_headers(&mut headers);

        return 
            Self
            {
                code: 
                    304, 
                headers,
                data:
                    CratesIoProxyResponsePacketData::None
            };
    }

    /// Sends the registry index entry download response.
    #[inline] 
    pub(crate) 
    fn create_index_entry_data_response(index_response: IndexResponse) -> Self
    {
        let mut headers: Vec<(Cow<'_, str>, Cow<'_, str>)>  = Vec::with_capacity(3);
        headers.push(CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR);

        index_response.entry.set_index_response_headers(&mut headers);
        
        return 
            Self
            {
                code: 
                    index_response.status,
                headers,
                data: 
                    CratesIoProxyResponsePacketData::ByteData(index_response.data),
            };
    }

    #[inline] 
    pub(crate) 
    fn create_index_entry_file_response(entry: IndexEntry, data: Vec<u8>) -> Self
    {
        // HTTP 200 OK
        let status = 200;

        let response = 
            IndexResponse 
            {
                entry,
                status,
                data,
            };

        return 
            Self::create_index_entry_data_response(response);
    }
}

/// A HTTP server crate which is used by program outside the crate
/// should map the request type to this type. Actually, only 'GET'
/// method is used. User can filter out rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CratesIoProxyRequestMethod
{
    GET, HEAD, POST, PUT, Other(String)
}

impl fmt::Display for CratesIoProxyRequestMethod
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result 
    {
        match self
        {
            Self::GET => 
                write!(f, "GET"),
            Self::HEAD => 
                write!(f, "HEAD"),
            Self::POST => 
                write!(f, "POST"),
            Self::PUT => 
                write!(f, "PUT"),
            Self::Other(m) => 
                write!(f, "{}", m),
        }
    }
}

/// A mapped error. A crate returns either `internal` error or 
/// an error which may be emmited by the HTTP server crate when
/// the response is sent.
/// 
/// A `RE` is an error type which a HTTP server emits.
#[derive(Debug)]
pub enum CratesIoProxyRequesError<RE: fmt::Debug>
{
    /// Internal error
    Internal(String),

    /// An external error of the crate outside this crate.
    Request(RE)
}

/// A trait which should be implemeted by a caller. It is required because this
/// crate generates answer during evaluation of the request. Because this crate is
/// independent from a server realization, it can generate response, but is not 
/// able to send it. 
/// 
/// For this reason, this crate is implemented on the server's `request` type.
/// 
/// See `examples` for the example.
pub trait CratesIoProxyRequest: std::marker::Send 
{    
    /// Should extract a header from the request. If header does not exist, return [Option::None].
    fn get_header(&self, name: &str) -> Option<&str>;

    /// Should return the requested URL.
    fn url(&self) -> &str;

    /// Should return the type of the HTTP method of the request.
    fn get_method(&self) -> CratesIoProxyRequestMethod;
}

#[derive(Debug)]
pub struct CratesIoProxy<'req, LOG: CratesIoProxyLog, REQ: CratesIoProxyRequest>
{
    cfg: Arc<ProxyConfig>,
    log: LOG,
    req: &'req REQ,
    index_cache: CratesIoProxyIndexCache
}

impl<'req, LOG: CratesIoProxyLog, REQ: CratesIoProxyRequest> CratesIoProxy<'req, LOG, REQ>
{
    /// Downloads the crate file from the upstream download server
    /// (usually <https://crates.io/>).
    fn download_crate(site_url: &Url, crate_info: &CrateInfo, max_crate_size: usize) -> Result<Vec<u8>, ureq::Error> 
    {
        let url = 
            site_url
                .join(CratesIoProxyConsts::CRATES_API_PATH)
                .unwrap()
                .join(&crate_info.to_download_url())
                .unwrap();

        let response = 
            ureq::get(url.as_str())
                .call()?;

        if let Some(content_len) = response.body().content_length()
        {
            if content_len > max_crate_size as u64
            {
                return Err(ureq_status_error(max_crate_size as u64));
            }

            let data = 
                response
                    .into_body()
                    .with_config()
                    .limit(max_crate_size as u64)
                    .read_to_vec()?;

            return Ok(data);
        } 
        else 
        {
            // Got no "Content-Length" header, most likely because "Transfer-Encoding: chunked"
            // is being sent by the server (crates.io servers do not do this).
            //
            // Using an arbitrary initial estimate for the total response size...
            let data = 
                response
                    .into_body()
                    .with_config()
                    .limit(max_crate_size as u64)
                    .read_to_vec()?;

            // Abort download here if the crate file has been truncated by
            // the `reader.take()` limit above.
            if data.len() >= max_crate_size 
            {
               return Err(ureq_status_error(max_crate_size as u64));
            }

            return Ok(data);
        }
    }

    /// Downloads the sparse index entry from the upstream registry.
    /// (usually <https://index.crates.io/>).
    fn download_index_entry(index_url: &Url, mut entry: IndexEntry) -> Result<IndexResponse, ureq::Error>
    {
        let url = index_url.join(&entry.to_index_url()).unwrap();

        let mut request = ureq::get(url.as_str());
        if let Some(etag) = entry.etag() 
        {
            request = request.header("If-None-Match", etag);
        }
        else if let Some(last_modified) = entry.last_modified() 
        {
            request = request.header("If-Modified-Since", last_modified);
        }

        let response = request.call()?;

        let status = response.status().as_u16();

        // Update the index entry metadata from the upstream response.
        if let Some(etag) = response.headers().get("ETag") 
        {
            entry.set_etag(etag.to_str().unwrap());
        }
        if let Some(last_modified) = response.headers().get("Last-Modified") 
        {
            entry.set_last_modified(last_modified.to_str().unwrap());
        }

        // Update the upstream server access timestamp.
        entry.set_last_updated();

        //let mut data: Vec<u8> = Vec::with_capacity(CratesIoProxyConsts::INDEX_ENTRY_CAPACITY);
        let data = response.into_body().read_to_vec()?;

        return 
            Ok(
                IndexResponse 
                {
                    entry,
                    status,
                    data,
                }
            );
    }
}

impl<'req, LOG: CratesIoProxyLog, REQ: CratesIoProxyRequest> CratesIoProxy<'req, LOG, REQ>
{
    #[must_use]
    fn format_json_error(error: impl Display) -> String 
    {
        format!(r#"{{"errors":[{{"detail":"{error}"}}]}}"#)
    }

    fn send_fetch_error_response(self, error: ureq::Error) -> CratesIoProxyResponsePacket
    {
        let err_desc_json = Self::format_json_error(error.to_string());

        match error 
        {
            // Forward the HTTP error status received from the upstream server.
            ureq::Error::StatusCode(code) => 
            {
                self.log.send_warn(format_args!("fetch: upstream returned HTTP status {code}: {err_desc_json}"));

                return 
                    CratesIoProxyResponsePacket
                        ::create_string_response(code, vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR], 
                            err_desc_json);
            },

            ureq::Error::BodyExceedsLimit(err) => 
            {
                self.log.send_error(format_args!("fetch: body exceeds limit: {err}"));

                let err_msg = "Insufficient storage";

                return 
                    CratesIoProxyResponsePacket
                        ::create_string_response(507, vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR], 
                            Self::format_json_error(err_msg));
            },

            // Return HTTP 502 Bad Gateway for client connection errors.
            ureq::Error::ConnectionFailed => 
            {
                self.log.send_error(format_args!("fetch: connection failed"));

                let err_msg = "Bad Gateway";
                
                return 
                    CratesIoProxyResponsePacket
                        ::create_string_response(502, vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR], 
                            Self::format_json_error(err_msg));
            },

            ureq::Error::Io(err) => 
            {
                self.log.send_error(format_args!("fetch: connection failed: {err}"));

                return 
                    CratesIoProxyResponsePacket
                            ::create_string_response(502, vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR], 
                                Self::format_json_error(err));
            },
            
            err => 
            {
                self.log.send_error(format_args!("fetch: unclassified error: {err}"));

                return 
                    CratesIoProxyResponsePacket
                        ::create_string_response(500, vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR], 
                            err_desc_json);
                    
            }
        };
    }

    /// Forwards the crate download request to the upstream server.
    ///
    /// Processes the download request in a dedicated thread.
    fn forward_download_request(self, crate_info: CrateInfo) -> CratesIoProxyResponsePacket
    {
        match Self::download_crate(&self.cfg.upstream_url, &crate_info, 20000000) 
        {
            Ok(data) => 
            {
                self.log.send_info(format_args!("fetch: successfully downloaded {crate_info}"));

                if let Err(e) = 
                    cache_store_crate(&self.cfg.crates_dir, &crate_info, &data)
                {
                    self.log.send_error(format_args!("{}", e));
                }

                return 
                    CratesIoProxyResponsePacket::create_data_response(data);
                /*    self.req.send_response(
                        CratesIoProxyResponsePacket::new_response_bytes(200, vec![Cow::Borrowed(CratesIoProxyConsts::CRATE_HTTP_CTYPE)], data)
                    );*/
            }
            Err(err) => 
                return self.send_fetch_error_response(err),
        }
    }

    /// Forwards the registry index entry download request to the upstream server.
    ///
    /// Processes the download request in a dedicated thread.
    ///
    /// If the requested index entry file already exists in the cache,
    /// attempts to reduce the amount of data transferred on both sides.
    fn forward_index_request(
        self,
        entry: IndexEntry,
        cached_entry: Option<IndexEntry>,
    ) -> CratesIoProxyResponsePacket
    {
        // Select where the new HTTP request headers will come from.
        let req_entry = cached_entry.unwrap_or_else(|| entry.clone());

        match Self::download_index_entry(&self.cfg.index_url, req_entry) 
        {
            Ok(response) => 
            {
                // Check for HTTP 200 or HTTP 304 statuses.
                if response.status == 200 
                {
                    self.log.send_info(format_args!("fetch: successfully got index entry for {entry}"));
                    if let Err(e) = 
                        cache_store_index_entry(&self.cfg.index_dir, &response.entry, &response.data)
                    {
                        self.log.send_error(format_args!("{}", e));                        
                    }
                } 
                else 
                {
                    self.log.send_debug(format_args!("fetch: cached index entry for {entry} is up to date"));
                }

                self.index_cache.metadata_store_index_entry(&response.entry);

                if response.entry.is_equivalent(&entry) 
                {
                    // Updated index entry file metadata matches that of the client request.
                    self.log.send_debug(format_args!("proxy: forwarding the up to date status for {entry}"));
                    
                    return CratesIoProxyResponsePacket::create_index_entry_not_modified_response(&response.entry);
                } 
                else if response.status == 200 
                {
                    // Upstream registry sent us updated index entry data.
                    self.log.send_debug(format_args!("proxy: forwarding new index data for {entry}"));
                    
                    return CratesIoProxyResponsePacket::create_index_entry_data_response(response);
                } 
                else if let Some(data) = cache_fetch_index_entry(&self.cfg.index_dir, &entry) 
                {
                    // Upstream registry sent us 304 Not Modified,
                    // but the client does not have this file cached.
                    // Fetch the index entry file from the local filesystem cache.
                    self.log.send_debug(format_args!("proxy: forwarding cached index data for {entry}"));
                    
                    return CratesIoProxyResponsePacket::create_index_entry_file_response(response.entry, data);
                } 
                else 
                {
                    // Something went very wrong with the local filesystem cache.
                    self.log.send_error(format_args!("cache: lost index cache file for {entry}"));
                    // Invalidate the volatile metadata cache and ask the client to retry.
                    self.index_cache.metadata_invalidate_index_entry(&entry);
                    
                    return CratesIoProxyResponsePacket::create_error_response(503);
                }
            },
            Err(err) => 
            {
                let is_err = 
                    if let ureq::Error::Io(_) = err
                    {
                        true
                    }
                    else if let ureq::Error::ConnectionFailed = err
                    {
                        true
                    }
                    else
                    {
                        false
                    };

                if is_err == true
                {
                    if let Some(data) = cache_fetch_index_entry(&self.cfg.index_dir, &entry) 
                    {
                        self.log.send_error(format_args!("fetch: index connection failed: {err}"));

                        // The upstream registry can not be reached at the moment, likely
                        // due to an intermittent network failure.
                        // Serve a possibly stale index entry file from the local filesystem
                        // cache anyway to keep the clients running.
                        self.log.send_warn(format_args!("proxy: forwarding possibly stale cached index data for {entry}"));

                        return
                            CratesIoProxyResponsePacket::create_index_entry_file_response(entry, data);
                    }
                }

                // Forward non-recoverable download errors back to the clients.
                return self.send_fetch_error_response(err);
            }
        }
    }

    /// Processes one crate download API request.
    fn handle_download_request(self, crate_url: &str) -> CratesIoProxyResponsePacket
    {
        let Some(crate_info) = CrateInfo::try_from_download_url(crate_url)
            else 
            {
                self.log.send_warn(format_args!("proxy: unrecognized download API endpoint: {crate_url}"));
                
                return CratesIoProxyResponsePacket::create_error_response(404);
            };

        self.log.send_debug(format_args!("proxy: download API endpoint hit: {crate_url}"));

        if let Some(data) = cache_fetch_crate(&self.cfg.crates_dir, &crate_info) 
        {
            self.log.send_debug(format_args!("proxy: local cache hit for {crate_info}"));
            return CratesIoProxyResponsePacket::create_data_response(data);
        } 
        else 
        {
            return self.forward_download_request(crate_info);
        }
    }

    /// Processes one sparse registry index API request.
    fn handle_index_request(self, index_url: &str) -> CratesIoProxyResponsePacket
    {
        if is_config_json_url(index_url) 
        {
            self.log.send_debug(format_args!("proxy: sending registry config file"));

            let cfg_json_file = gen_config_json_file(&self.cfg);
            return 
                CratesIoProxyResponsePacket::create_json_response(200, cfg_json_file);
        }

        let Some(mut index_entry) = IndexEntry::try_from_index_url(index_url) 
            else 
            {
                self.log.send_warn(format_args!("proxy: malformed registry index path: {index_url}"));
                return CratesIoProxyResponsePacket::create_error_response(404);
            };

        self.log.send_debug(format_args!("proxy: requesting index entry for {index_entry}"));

        // Extract cache control headers from all index requests.
        if let Some(etag) = self.req.get_header("If-None-Match")
        {
            self.log.send_debug(format_args!("proxy: checking known index entry {index_entry} with ETag: {etag}"));
            index_entry.set_etag(etag);
        }

        if let Some(last_modified) = self.req.get_header("If-Modified-Since")
        {
            self.log.send_debug(format_args!("proxy: checking known index entry {index_entry} with Last-Modified: {last_modified}"));
            index_entry.set_last_modified(last_modified);
        }

        // Try to serve the request from the local index cache first.
        // NOTE: The index file cache can not be used without matching metadata.
        if let Some(cached_entry) = self.index_cache.metadata_fetch_index_entry(index_entry.name()) 
        {
            // Expired cache entries require a new request to the upstream registry.
            if cached_entry.is_expired_with_ttl(&self.cfg.cache_ttl) 
            {
                self.log.send_info(format_args!("proxy: index cache expired for {index_entry}, refreshing..."));

                return self.forward_index_request(index_entry, Some(cached_entry));
            }

            // Check for the index metadata cache hit via ETag and Last-Modified fields.
            if cached_entry.is_equivalent(&index_entry) 
            {
                self.log.send_debug(format_args!("proxy: index metadata cache hit for {index_entry}"));
                return CratesIoProxyResponsePacket::create_index_entry_not_modified_response(&cached_entry);
            }

            // Check for the index file cache hit next.
            if let Some(data) = cache_fetch_index_entry(&self.cfg.index_dir, &index_entry) 
            {
                self.log.send_debug(format_args!("proxy: index data cache hit for {index_entry}"));
                return CratesIoProxyResponsePacket::create_index_entry_file_response(cached_entry, data);
            }
        }

        // Try to recreate the index entry metadata from the cached file mtime.
        let mtimed_entry = cache_try_find_index_entry(&self.cfg.index_dir, index_entry.name());

        if let Some(entry) = &mtimed_entry 
        {
            let last_modified = entry.last_modified().unwrap();

            self.log.send_info(
                format_args!("proxy: recreated index cache metadata for {entry} with Last-Modified: {last_modified}")
            );
        }

        // Fall back to forwarding the request to the upstream registry.
        return
            self.forward_index_request(index_entry, mtimed_entry);
    }


    /// Processes one HTTP GET request.
    ///
    /// Only registry index and download API requests are supported.
    pub 
    fn handle_get_request(self) -> CratesIoProxyResponsePacket
    {
        let url = self.req.url().to_owned();

        if let Some(index_url) = url.strip_prefix(CratesIoProxyConsts::CRATES_INDEX_PATH) 
        {
            return self.handle_index_request(index_url);
        } 
        else if let Some(crate_url) = url.strip_prefix(CratesIoProxyConsts::CRATES_API_PATH) 
        {
            return self.handle_download_request(crate_url);
        } 
        else 
        {
            self.log.send_error(format_args!("proxy: unknown index or download API path: {url}"));
            return CratesIoProxyResponsePacket::create_error_response(404);
        };
    }

    pub 
    fn handle(cfg: Arc<ProxyConfig>, log: LOG, req: &'req REQ, index_cache: CratesIoProxyIndexCache) 
        -> CratesIoProxyResponsePacket
    {
        let this = Self{ cfg, log, req, index_cache };
        let method = this.req.get_method();

        if this.req.get_method() != CratesIoProxyRequestMethod::GET
        {
            this.log.send_error(format_args!("unexpected download API method: {}", method));
            
            return CratesIoProxyResponsePacket::create_error_response(403);
        }

        return this.handle_get_request();
    }
}

