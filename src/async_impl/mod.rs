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


use std::borrow::Cow;
use std::marker::PhantomData;
use std::sync::Arc;
use std::{fmt, io};
use std::fmt::Display;

use url::Url;


use crate::{CratesIoProxyConsts, CratesIoProxyRequestMethod, CratesIoProxyResponsePacket, IndexResponse, ProxyConfig};
use crate::config_json::{gen_config_json_file, is_config_json_url};
use crate::crate_info::CrateInfo;
use crate::file_cache::{
    cache_fetch_crate, cache_fetch_index_entry, cache_store_crate, cache_store_index_entry,
    cache_try_find_index_entry,
};
use crate::index_entry::IndexEntry;
use crate::metadata_cache::{CratesIoProxyIndexCache};

/// This crate does not use any logging crates. Crate emists some
/// debug or info messages. If it is required to catch any implement
/// any of the functions of this trait.
#[allow(async_fn_in_trait)]
pub trait AsyncCratesIoProxyLog: std::marker::Send + 'static
{
    async fn send_debug(&self, _dbg: String) {}
    async fn send_info(&self, _info: String) {}
    async fn send_warn(&self, _warn: String) {}
    async fn send_error(&self, _err: String) {}
}

#[derive(Debug)]
pub enum AsyncCratesIoProxyHttpClientError<ERR: AsyncCratesIoProxyHttpClientErrorMarker>
{
    BodyExceedsLimit(u64),
    Io(io::Error),
    CrateError(ERR),
}

impl<ERR: AsyncCratesIoProxyHttpClientErrorMarker> fmt::Display 
for AsyncCratesIoProxyHttpClientError<ERR>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result 
    {
        match self
        {
            Self::BodyExceedsLimit(lim) => 
                write!(f, "the response body is larger than request limit: {}", lim),
            Self::Io(error) => 
                write!(f, "io: {}", error),
            Self::CrateError(e) => 
                write!(f, "{}", e),
        }
    }
}

/// Should be implemented on the error type of the client.
pub trait AsyncCratesIoProxyHttpClientErrorMarker: fmt::Debug + fmt::Display
{
    /// Should return a HTTP code
    fn get_status_code(&self) -> Option<u16>;
}

#[allow(async_fn_in_trait)]
pub trait AsyncCratesIoProxyHttpClientResponse: fmt::Debug
{
    /// Should return a `Content-Length` from headers if any.
    fn get_content_length(&self) -> Option<u64>;

    /// Should read data to buffer and return it.
    async fn read_to_vec(self) -> Result<Vec<u8>, io::Error>;

    /// Should return HTTP status code as u16
    fn get_status(&self) -> u16;

    /// Should return header by `header_name` if any.
    fn get_header(&self, header_name: &str) -> Option<&str>;
}

#[allow(async_fn_in_trait)]
pub trait AsyncCratesIoProxyHttpClient
{
    /// An error type which HTTP client which forwards request returns.
    type ErrType: AsyncCratesIoProxyHttpClientErrorMarker;
    type RetType: AsyncCratesIoProxyHttpClientResponse;

    async fn get(url: Url, headers: Vec<(&'static str, String)>) -> Result<Self::RetType, AsyncCratesIoProxyHttpClientError<Self::ErrType>>;

}

/// A trait which should be implemeted by a caller. It is required because this
/// crate generates answer during evaluation of the request. Because this crate is
/// independent from a server realization, it can generate response, but is not 
/// able to send it. 
/// 
/// For this reason, this crate is implemented on the server's `request` type.
/// 
/// See `examples` for the example.
#[allow(async_fn_in_trait)]
pub trait AsyncCratesIoProxyRequest: std::marker::Send 
{
    /// Should extract a header from the request. If header does not exist, return [Option::None].
    fn get_header(&self, name: &str) -> Option<&str>;

    /// Should return the requested URL.
    fn url<'url>(&'url self) -> Cow<'url, str>;

    /// Should return the type of the HTTP method of the request.
    fn get_method(&self) -> CratesIoProxyRequestMethod;
}

#[derive(Debug)]
pub struct AsyncCratesIoProxy<'req, LOG, REQ, HCLI>
where LOG: AsyncCratesIoProxyLog, REQ: AsyncCratesIoProxyRequest, HCLI: AsyncCratesIoProxyHttpClient
{
    cfg: Arc<ProxyConfig>,
    log: LOG,
    req: &'req REQ,
    index_cache: CratesIoProxyIndexCache,
    _p: PhantomData<HCLI>,
}

impl<'req, LOG, REQ, HCLI> AsyncCratesIoProxy<'req, LOG, REQ, HCLI>
where 
    LOG: AsyncCratesIoProxyLog, REQ: AsyncCratesIoProxyRequest, HCLI: AsyncCratesIoProxyHttpClient
{
    /// Downloads the crate file from the upstream download server
    /// (usually <https://crates.io/>).
    async 
    fn download_crate(site_url: &Url, crate_info: &CrateInfo, max_crate_size: usize) 
        -> Result<Vec<u8>, AsyncCratesIoProxyHttpClientError<HCLI::ErrType>> 
    {
        let url = 
            site_url
                .join(CratesIoProxyConsts::CRATES_API_PATH)
                .unwrap()
                .join(&crate_info.to_download_url())
                .unwrap();

        let response = HCLI::get(url, vec![]).await?;

        if let Some(content_len) = response.get_content_length()
        {
            if content_len > max_crate_size as u64
            {
                return Err(AsyncCratesIoProxyHttpClientError::BodyExceedsLimit(max_crate_size as u64));
            }

            let data = 
                response.read_to_vec().await.map_err(|e| AsyncCratesIoProxyHttpClientError::Io(e))?;

            return Ok(data);
        } 
        else 
        {
            // Got no "Content-Length" header, most likely because "Transfer-Encoding: chunked"
            // is being sent by the server (crates.io servers do not do this).
            //
            // Using an arbitrary initial estimate for the total response size...
            let data = 
                response.read_to_vec().await.map_err(|e| AsyncCratesIoProxyHttpClientError::Io(e))?;

            // Abort download here if the crate file has been truncated by
            // the `reader.take()` limit above.
            if data.len() >= max_crate_size 
            {
               return Err(AsyncCratesIoProxyHttpClientError::BodyExceedsLimit(max_crate_size as u64));
            }

            return Ok(data);
        }
    }

    /// Downloads the sparse index entry from the upstream registry.
    /// (usually <https://index.crates.io/>).
    async 
    fn download_index_entry(index_url: &Url, mut entry: IndexEntry) 
        -> Result<IndexResponse, AsyncCratesIoProxyHttpClientError<HCLI::ErrType>> 
    {
        let url = index_url.join(&entry.to_index_url()).unwrap();
        let mut headers: Vec<(&'static str, String)> = 
            Vec::with_capacity(entry.etag().map_or(0, |_| 1) + entry.last_modified().map_or(0, |_| 1));

        if let Some(etag) = entry.etag() 
        {
            headers.push(("If-None-Match", etag.to_string()));
        }
        else if let Some(last_modified) = entry.last_modified() 
        {
            headers.push(("If-Modified-Since", last_modified.to_string()));
        }

        let response = HCLI::get(url, headers).await?;

        let status = response.get_status();

        // Update the index entry metadata from the upstream response.
        if let Some(etag) = response.get_header("ETag") 
        {
            entry.set_etag(etag);
        }
        if let Some(last_modified) = response.get_header("Last-Modified") 
        {
            entry.set_last_modified(last_modified);
        }

        // Update the upstream server access timestamp.
        entry.set_last_updated();

        //let mut data: Vec<u8> = Vec::with_capacity(CratesIoProxyConsts::INDEX_ENTRY_CAPACITY);
        let data = response.read_to_vec().await.map_err(|e| AsyncCratesIoProxyHttpClientError::Io(e))?;

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

impl<'req, LOG, REQ, HCLI> AsyncCratesIoProxy<'req, LOG, REQ, HCLI>
where 
    LOG: AsyncCratesIoProxyLog, REQ: AsyncCratesIoProxyRequest, HCLI: AsyncCratesIoProxyHttpClient
{
    #[must_use]
    fn format_json_error(error: impl Display) -> String 
    {
        format!(r#"{{"errors":[{{"detail":"{error}"}}]}}"#)
    }

    async 
    fn send_fetch_error_response(self, error: AsyncCratesIoProxyHttpClientError<HCLI::ErrType>) 
        -> CratesIoProxyResponsePacket
    {
        let err_desc_json = Self::format_json_error(error.to_string());

        match error 
        {
            // Forward the HTTP error status received from the upstream server.
            AsyncCratesIoProxyHttpClientError::CrateError(cerr) => 
            {
                if let Some(code) = cerr.get_status_code()
                {
                    self.log.send_warn(format!("fetch: upstream returned HTTP status {code}: {err_desc_json}")).await;

                    return 
                        CratesIoProxyResponsePacket
                            ::create_string_response(code, vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR], err_desc_json);
                }
                else
                {
                    self.log.send_error(format!("fetch: unclassified error: {cerr}")).await;

                    return 
                        CratesIoProxyResponsePacket
                            ::create_string_response(500, vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR], err_desc_json);
                    }
            },

            AsyncCratesIoProxyHttpClientError::BodyExceedsLimit(err) => 
            {
                self.log.send_error(format!("fetch: body exceeds limit: {err}")).await;

                let err_msg = "Insufficient storage";

                return 
                    CratesIoProxyResponsePacket
                        ::create_string_response(507, vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR], Self::format_json_error(err_msg));
            },

            AsyncCratesIoProxyHttpClientError::Io(err) => 
            {
                self.log.send_error(format!("fetch: connection failed: {err}")).await;

                return 
                    CratesIoProxyResponsePacket
                        ::create_string_response(502, vec![CratesIoProxyConsts::JSON_HTTP_CTYPE_PAIR], Self::format_json_error(err));
            }
        };
    }

    /// Forwards the crate download request to the upstream server.
    ///
    /// Processes the download request in a dedicated thread.
    async 
    fn forward_download_request(self, crate_info: CrateInfo) -> CratesIoProxyResponsePacket
    {
        match Self::download_crate(&self.cfg.upstream_url, &crate_info, 20000000).await 
        {
            Ok(data) => 
            {
                self.log.send_info(format!("fetch: successfully downloaded {crate_info}")).await;
                
                if let Err(e) = 
                    cache_store_crate(&self.cfg.crates_dir, &crate_info, &data)
                {
                    self.log.send_error(format!("{}", e)).await;
                }


                return 
                    CratesIoProxyResponsePacket::create_data_response(data);
                /*    self.req.send_response(
                        CratesIoProxyResponsePacket::new_response_bytes(200, vec![Cow::Borrowed(CratesIoProxyConsts::CRATE_HTTP_CTYPE)], data)
                    );*/
            }
            Err(err) => 
                return self.send_fetch_error_response(err).await,
        }
    }


    /// Forwards the registry index entry download request to the upstream server.
    ///
    /// Processes the download request in a dedicated thread.
    ///
    /// If the requested index entry file already exists in the cache,
    /// attempts to reduce the amount of data transferred on both sides.
    async 
    fn forward_index_request(
        self,
        entry: IndexEntry,
        cached_entry: Option<IndexEntry>,
    ) -> CratesIoProxyResponsePacket
    {
        // Select where the new HTTP request headers will come from.
        let req_entry = cached_entry.unwrap_or_else(|| entry.clone());

        match Self::download_index_entry(&self.cfg.index_url, req_entry).await 
        {
            Ok(response) => 
            {
                // Check for HTTP 200 or HTTP 304 statuses.
                if response.status == 200 
                {
                    self.log.send_info(format!("fetch: successfully got index entry for {entry}")).await;
                    if let Err(e) = 
                        cache_store_index_entry(&self.cfg.index_dir, &response.entry, &response.data)
                    {
                        self.log.send_error(format!("{}", e)).await;
                    }
                } 
                else 
                {
                    self.log.send_debug(format!("fetch: cached index entry for {entry} is up to date")).await;
                }

                self.index_cache.metadata_store_index_entry(&response.entry);

                if response.entry.is_equivalent(&entry) 
                {
                    // Updated index entry file metadata matches that of the client request.
                    self.log.send_debug(format!("proxy: forwarding the up to date status for {entry}")).await;
                    
                    return CratesIoProxyResponsePacket::create_index_entry_not_modified_response(&response.entry);
                } 
                else if response.status == 200 
                {
                    // Upstream registry sent us updated index entry data.
                    self.log.send_debug(format!("proxy: forwarding new index data for {entry}")).await;
                    
                    return  CratesIoProxyResponsePacket::create_index_entry_data_response(response);
                } 
                else if let Some(data) = cache_fetch_index_entry(&self.cfg.index_dir, &entry) 
                {
                    // Upstream registry sent us 304 Not Modified,
                    // but the client does not have this file cached.
                    // Fetch the index entry file from the local filesystem cache.
                    self.log.send_debug(format!("proxy: forwarding cached index data for {entry}")).await;
                    
                    return CratesIoProxyResponsePacket::create_index_entry_file_response(response.entry, data);
                } 
                else 
                {
                    // Something went very wrong with the local filesystem cache.
                    self.log.send_error(format!("cache: lost index cache file for {entry}")).await;
                    // Invalidate the volatile metadata cache and ask the client to retry.
                    self.index_cache.metadata_invalidate_index_entry(&entry);
                    
                    return CratesIoProxyResponsePacket::create_error_response(503);
                }
            },
            Err(err) => 
            {
                let is_err = 
                    if let AsyncCratesIoProxyHttpClientError::Io(_) = err
                    {
                        true
                    }
                    else if let AsyncCratesIoProxyHttpClientError::CrateError(_) = err
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
                        self.log.send_error(format!("fetch: index connection failed: {err}")).await;

                        // The upstream registry can not be reached at the moment, likely
                        // due to an intermittent network failure.
                        // Serve a possibly stale index entry file from the local filesystem
                        // cache anyway to keep the clients running.
                        self.log.send_warn(
                            format!("proxy: forwarding possibly stale cached index data for {entry}")
                        )
                        .await;

                        return
                            CratesIoProxyResponsePacket::create_index_entry_file_response(entry, data);
                    }
                }

                // Forward non-recoverable download errors back to the clients.
                return self.send_fetch_error_response(err).await;
            }
        }
    }

    /// Processes one crate download API request.
    async 
    fn handle_download_request(self, crate_url: &str) -> CratesIoProxyResponsePacket
    {
        let Some(crate_info) = CrateInfo::try_from_download_url(crate_url)
            else 
            {
                self.log.send_warn(format!("proxy: unrecognized download API endpoint: {crate_url}")).await;
                
                return CratesIoProxyResponsePacket::create_error_response(404);
            };

        self.log.send_debug(format!("proxy: download API endpoint hit: {crate_url}")).await;

        if let Some(data) = cache_fetch_crate(&self.cfg.crates_dir, &crate_info) 
        {
            self.log.send_debug(format!("proxy: local cache hit for {crate_info}")).await;
            
            return CratesIoProxyResponsePacket::create_data_response(data);
        } 
        else 
        {
            return self.forward_download_request(crate_info).await;
        }
    }

    /// Processes one sparse registry index API request.
    async 
    fn handle_index_request(self, index_url: &str) -> CratesIoProxyResponsePacket
    {
        if is_config_json_url(index_url) 
        {
            self.log.send_debug(format!("proxy: sending registry config file")).await;

            let cfg_json_file = gen_config_json_file(&self.cfg);

            return 
                CratesIoProxyResponsePacket::create_json_response(200, cfg_json_file);
        }

        let Some(mut index_entry) = IndexEntry::try_from_index_url(index_url) 
            else 
            {
                self.log.send_warn(format!("proxy: malformed registry index path: {index_url}")).await;
                return CratesIoProxyResponsePacket::create_error_response(404);
            };

        self.log.send_debug(format!("proxy: requesting index entry for {index_entry}")).await;

        // Extract cache control headers from all index requests.
        if let Some(etag) = self.req.get_header("If-None-Match")
        {
            self.log.send_debug(
                format!("proxy: checking known index entry {index_entry} with ETag: {etag}")
            )
            .await;
            index_entry.set_etag(etag);
        }

        if let Some(last_modified) = self.req.get_header("If-Modified-Since")
        {
            self.log.send_debug(
                format!("proxy: checking known index entry {index_entry} with Last-Modified: {last_modified}")
            )
            .await;

            index_entry.set_last_modified(last_modified);
        }

        // Try to serve the request from the local index cache first.
        // NOTE: The index file cache can not be used without matching metadata.
        if let Some(cached_entry) = self.index_cache.metadata_fetch_index_entry(index_entry.name()) 
        {
            // Expired cache entries require a new request to the upstream registry.
            if cached_entry.is_expired_with_ttl(&self.cfg.cache_ttl) 
            {
                self.log.send_info(format!("proxy: index cache expired for {index_entry}, refreshing...")).await;

                return self.forward_index_request(index_entry, Some(cached_entry)).await;
            }

            // Check for the index metadata cache hit via ETag and Last-Modified fields.
            if cached_entry.is_equivalent(&index_entry) 
            {
                self.log.send_debug(format!("proxy: index metadata cache hit for {index_entry}")).await;
                return CratesIoProxyResponsePacket::create_index_entry_not_modified_response(&cached_entry);
            }

            // Check for the index file cache hit next.
            if let Some(data) = cache_fetch_index_entry(&self.cfg.index_dir, &index_entry) 
            {
                self.log.send_debug(format!("proxy: index data cache hit for {index_entry}")).await;
                return CratesIoProxyResponsePacket::create_index_entry_file_response(cached_entry, data);
            }
        }

        // Try to recreate the index entry metadata from the cached file mtime.
        let mtimed_entry = cache_try_find_index_entry(&self.cfg.index_dir, index_entry.name());

        if let Some(entry) = &mtimed_entry 
        {
            let last_modified = entry.last_modified().unwrap();

            self.log.send_info(
                format!("proxy: recreated index cache metadata for {entry} with Last-Modified: {last_modified}")
            )
            .await;
        }

        // Fall back to forwarding the request to the upstream registry.
        return
            self.forward_index_request(index_entry, mtimed_entry).await;
    }


    /// Processes one HTTP GET request.
    ///
    /// Only registry index and download API requests are supported.
    pub async 
    fn handle_get_request(self) -> CratesIoProxyResponsePacket
    {
        let url = self.req.url().to_owned();

        if let Some(index_url) = url.strip_prefix(CratesIoProxyConsts::CRATES_INDEX_PATH) 
        {
            return self.handle_index_request(index_url).await;
        } 
        else if let Some(crate_url) = url.strip_prefix(CratesIoProxyConsts::CRATES_API_PATH) 
        {
            return self.handle_download_request(crate_url).await;
        } 
        else 
        {
            self.log.send_error(format!("proxy: unknown index or download API path: {url}")).await;
            return CratesIoProxyResponsePacket::create_error_response(404);
        };
    }

    pub async 
    fn handle(cfg: Arc<ProxyConfig>, log: LOG, req: &'req REQ, index_cache: CratesIoProxyIndexCache) 
        -> CratesIoProxyResponsePacket
    {
        let this = Self{ cfg, log, req, index_cache, _p: PhantomData };
        let method = this.req.get_method();

        if this.req.get_method() != CratesIoProxyRequestMethod::GET
        {
            this.log.send_error(format!("unexpected download API method: {}", method)).await;
            return CratesIoProxyResponsePacket::create_error_response(403);
        }

        return 
            this
                .handle_get_request()
                .await;
    }
}

