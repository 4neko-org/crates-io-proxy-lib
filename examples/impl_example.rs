use std::{fmt, io::{self, ErrorKind}, sync::Arc, time::Instant};

use crates_io_proxy_fork::
{
    CratesIoProxy, CratesIoProxyConsts, CratesIoProxyLog, CratesIoProxyRequest, CratesIoProxyRequestMethod, 
    CratesIoProxyResponsePacketData, ProxyConfig, metadata_cache::CratesIoProxyIndexCache
};

use tempfile::TempDir;
use tiny_http_fork::{Header, HeaderError, Method, Request, Response, Server};

extern crate crates_io_proxy_fork;

#[derive(Debug)]
pub struct LogErr;

impl CratesIoProxyLog for LogErr
{
    fn send_debug(&self, args: fmt::Arguments<'_>) 
    {
        println!("debug: {}", args);
    }

    fn send_info(&self, args: fmt::Arguments<'_>) 
    {
        println!("info: {}", args);
    }

    fn send_warn(&self, args: fmt::Arguments<'_>) 
    {
        println!("warn: {}", args);
    }

    fn send_error(&self, args: fmt::Arguments<'_>) 
    {
        println!("error: {}", args);
    }
}

/// A wrapper arount the tiny_http_fork::request::Request
#[repr(transparent)]
#[derive(Debug)]
pub struct CipRequst(Request);

impl CratesIoProxyRequest for CipRequst
{
    fn get_header(&self, name: &str) -> Option<&str> 
    {
        self.0.headers().get(name).map(|v| v.as_str())
    }

    fn url(&self) -> &str 
    {
        self.0.url()
    }

    fn get_method(&self) -> CratesIoProxyRequestMethod 
    {
        match self.0.method()
        {
            Method::Get => 
                CratesIoProxyRequestMethod::GET,
            Method::Head => 
                CratesIoProxyRequestMethod::HEAD,
            Method::Post => 
                CratesIoProxyRequestMethod::POST,
            Method::Put => 
                CratesIoProxyRequestMethod::PUT,
            Method::Delete => 
                CratesIoProxyRequestMethod::Other("DELETE".into()),
            Method::Connect => 
                CratesIoProxyRequestMethod::Other("CONNECT".into()),
            Method::Options => 
                CratesIoProxyRequestMethod::Other("OPTIONS".into()),
            Method::Trace => 
                CratesIoProxyRequestMethod::Other("TRACE".into()),
            Method::Patch => 
                CratesIoProxyRequestMethod::Other("PATCH".into()),
            Method::NonStandard(ascii_string) => 
                CratesIoProxyRequestMethod::Other(ascii_string.to_string()),
        }
    }
}

impl CipRequst
{
    fn send_response(self, resp: crates_io_proxy_fork::CratesIoProxyResponsePacket) ->  Result<(), io::Error>
    {
        let headers = resp.headers.into_iter().map(|v| Header::from_str(v.0, v.1)).collect::<Result<Vec<Header>, HeaderError>>().unwrap();

        match resp.data
        {
            CratesIoProxyResponsePacketData::None =>
            { 
                let response = 
                    Response::empty(resp.code)
                        .with_headers(headers)
                        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e.to_string()))?;
                

                return self.0.respond(response);
            },
            CratesIoProxyResponsePacketData::StringData(d) => 
            {
                let response = 
                    Response::from_string(d)
                        .with_headers(headers)
                        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e.to_string()))?
                        .with_status_code(resp.code);

                return self.0.respond(response);
            },
            CratesIoProxyResponsePacketData::ByteData(items) => 
            {
                let response = 
                    Response::from_data(items)
                        .with_headers(headers)
                        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e.to_string()))?
                        .with_status_code(resp.code);


                return self.0.respond(response);
            }
        }
    }
}



/// Runs HTTP proxy server forever.
fn main() 
{
    let temp_dir = TempDir::new().unwrap();

    // A HHTP server is tiny_http_fork
    let server = Server::http(CratesIoProxyConsts::LISTEN_ADDRESS).expect("failed to start the HTTP server");
    println!("proxy: starting HTTP server at: {}", server.server_addr());

    // creating index cache
    let index_cache = CratesIoProxyIndexCache::new_with_capacity(100);

    // creating configuration
    let proxy_cfg = Arc::new(ProxyConfig::new().set_cache_dir(temp_dir.path()));

    // create directories if necessary
    proxy_cfg.create_dirs().unwrap();

    // Main HTTP request accept loop.
    loop 
    {
        let request = CipRequst(server.recv().expect("failed to accept new HTTP requests").unwrap());

        let s = Instant::now();
        
        let resp = CratesIoProxy::<LogErr, CipRequst>::handle(proxy_cfg.clone(), LogErr, &request, index_cache.clone());

        let e = s.elapsed();

        println!("main: request handling took: {:?}", e);

        let res = request.send_response(resp);

        assert_eq!(res.is_ok(), true, "{:?}", res.err().unwrap());
    }
}