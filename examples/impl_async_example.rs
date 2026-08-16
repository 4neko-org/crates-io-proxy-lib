
use std::{borrow::Cow, fmt, io::{self, Cursor, ErrorKind}, sync::{Arc, LazyLock}, time::Instant};

use crates_io_proxy_fork::
{
    AsyncCratesIoProxy, AsyncCratesIoProxyHttpClient, AsyncCratesIoProxyHttpClientError, AsyncCratesIoProxyHttpClientErrorMarker, 
    AsyncCratesIoProxyHttpClientResponse, AsyncCratesIoProxyLog, AsyncCratesIoProxyRequest, CratesIoProxyConsts, CratesIoProxyRequestMethod, 
    CratesIoProxyResponsePacketData, ProxyConfig, metadata_cache::CratesIoProxyIndexCache
};

use rocket::
{
    Catcher, Config, Data, Request, Response, Route, catcher, http::{Method::{self, Get}, Status}, response::{Responder, status::Custom}, route
};

use tempfile::TempDir;

static LAZY_TEMPDIR: LazyLock<TempDir> = 
    LazyLock::new(
        ||
        {
            TempDir::new().unwrap()
        }
    );

static LAZY_INDEX: LazyLock<CratesIoProxyIndexCache> = 
    LazyLock::new(
        ||
        {
            CratesIoProxyIndexCache::new_with_capacity(100)
        }
    );

static LAZY_CFG: LazyLock<Arc<ProxyConfig>> = 
    LazyLock::new(
        ||
        {
            Arc::new(ProxyConfig::new().set_cache_dir(LAZY_TEMPDIR.path()))
        }
    );

pub struct LogErr;

impl AsyncCratesIoProxyLog for LogErr
{
    async 
    fn send_debug(&self, dbg: String) 
    {
        println!("debug: {}", dbg);
    }

    async 
    fn send_info(&self, info: String) 
    {
        println!("info: {}", info);
    }

    async 
    fn send_warn(&self, warn: String) 
    {
        println!("warn: {}", warn);
    }

    async 
    fn send_error(&self, err: String) 
    {
        println!("error: {}", err);
    }
}

#[repr(transparent)]
#[derive(Debug)]
struct ReqwestError(reqwest::Error);

impl fmt::Display for ReqwestError
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result 
    {
        write!(f, "{}", self.0)
    }
}

impl AsyncCratesIoProxyHttpClientErrorMarker for ReqwestError
{
    fn get_status_code(&self) -> Option<u16> 
    {
        self.0.status().map(|v| v.as_u16())
    }
}

#[derive(Debug)]
struct ReqwestResponse(reqwest::Response);

impl AsyncCratesIoProxyHttpClientResponse for ReqwestResponse
{
    fn get_content_length(&self) -> Option<u64> 
    {
        self.0.content_length()
    }

    async 
    fn read_to_vec(self) -> Result<Vec<u8>, std::io::Error> 
    {
        self
            .0
            .bytes()
            .await
            .map_err(|e| io::Error::new(ErrorKind::Other, e.to_string()))
            .map(|v| v.to_vec())
    }

    fn get_status(&self) -> u16 
    {
        self.0.status().as_u16()
    }

    fn get_header(&self, header_name: &str) -> Option<&str> 
    {
        self.0.headers().get(header_name).map(|v| v.to_str().unwrap())
    }
}

#[derive(Debug)]
struct ReqwestGet;

impl AsyncCratesIoProxyHttpClient for ReqwestGet
{
    type ErrType = ReqwestError;

    type RetType = ReqwestResponse;

    async 
    fn get(url: url::Url, headers: Vec<(&'static str, String)>) 
        -> Result<Self::RetType, AsyncCratesIoProxyHttpClientError<Self::ErrType>> 
    {
        let cli = reqwest::Client::new();
        let mut cli_req = cli.get(url);
        
        for (k, v) in headers
        {
            cli_req = cli_req.header(k, v);
        }
        
        cli
            .execute(cli_req.build().unwrap())
            .await
            .map(|v| ReqwestResponse(v))
            .map_err(|e| AsyncCratesIoProxyHttpClientError::CrateError(ReqwestError(e)))
    }
}

#[derive(Debug)]
struct RocketRequest<'a, 'l>(&'a Request<'l>);

impl<'a, 'l> AsyncCratesIoProxyRequest for RocketRequest<'a, 'l>
{
    fn get_header(&self, name: &str) -> Option<&str> 
    {
        self.0.headers().get_one(name)
    }

    fn url<'url>(&'url self) -> Cow<'url, str>
    {
        Cow::Owned(self.0.uri().to_string())
    }

    fn get_method(&self) -> crates_io_proxy_fork::CratesIoProxyRequestMethod 
    {
        match self.0.method()
        {
            Method::Get => 
                CratesIoProxyRequestMethod::GET,
            Method::Put => 
                CratesIoProxyRequestMethod::PUT,
            Method::Post => 
                CratesIoProxyRequestMethod::POST,
            Method::Delete => 
                CratesIoProxyRequestMethod::Other("DELETE".into()),
            Method::Options => 
                CratesIoProxyRequestMethod::Other("OPTIONS".into()),
            Method::Head => 
                CratesIoProxyRequestMethod::HEAD,
            Method::Trace => 
                CratesIoProxyRequestMethod::Other("TRACE".into()),
            Method::Connect => 
                CratesIoProxyRequestMethod::Other("CONNECT".into()),
            Method::Patch => 
                CratesIoProxyRequestMethod::Other("PATCH".into()),
        }
    }
}



fn index<'r, 'l>(req: &'r Request<'l>, _data: Data<'r>) -> route::BoxFuture<'r>
{
    Box::pin(
        async move 
        { 
            let rockreq = RocketRequest(req);

            let s = Instant::now();

            let resp = AsyncCratesIoProxy::<LogErr, RocketRequest, ReqwestGet>::handle(LAZY_CFG.clone(), LogErr, &rockreq, LAZY_INDEX.clone()).await;

            let e = s.elapsed();

            println!("main: request handling took: {:?}", e);

            let mut builder = Response::build();
        
            match resp.data
            {
                CratesIoProxyResponsePacketData::None => 
                {
                    builder.status(Status::from_code(resp.code).unwrap());

                    for header in resp.headers
                    {
                        builder.raw_header(header.0, header.1);
                    }

                    let responder = builder.finalize();
                        
                    return route::Outcome::Success(responder);
                    
                },
                CratesIoProxyResponsePacketData::StringData(s) => 
                {
                    builder.status(Status::from_code(resp.code).unwrap());

                    for header in resp.headers
                    {
                        builder.raw_header(header.0, header.1);
                    }

                    builder.sized_body(s.as_bytes().len(), Cursor::new(s));

                    let responder = builder.finalize();
                        
                    return route::Outcome::Success(responder);
                },
                CratesIoProxyResponsePacketData::ByteData(items) => 
                {
                    builder.status(Status::from_code(resp.code).unwrap());

                    for header in resp.headers
                    {
                        builder.raw_header(header.0, header.1);
                    }

                    builder.sized_body(items.len(), Cursor::new(items));
                    let responder = builder.finalize();
                        
                    return route::Outcome::Success(responder);
                },
            }
        }
    )
}

fn not_found_handler<'r>(_: Status, req: &'r Request) -> catcher::BoxFuture<'r> 
{
    let responder = Custom(Status::NotFound, format!("Couldn't find: {}", req.uri()));
    Box::pin(async move { responder.respond_to(req) })
}

#[rocket::launch]
fn rocket() -> _
{
    println!("Listening for requests at http://{}", CratesIoProxyConsts::LISTEN_ADDRESS);

    let route_index = Route::ranked(1, Get, "/<path..>", index);
    let not_found_catcher = Catcher::new(404, not_found_handler);

    let config = 
        Config
        {
            port: 3080,
            address: "127.0.0.1".parse().unwrap(),
            ..Config::debug_default()
        };

    rocket::build()
        .configure(config)
        .mount("/", vec![route_index])
        .register("/", vec![not_found_catcher])
}