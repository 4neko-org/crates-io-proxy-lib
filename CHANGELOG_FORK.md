## 0.2.4 (15.08.2026)

- Code was refactored to be used as a lib. A trait CratesIoProxyLog for logging, CratesIoProxyResponsePacket for crates
response, CratesIoProxyRequest trait for response generation.
- Removed thread spawning, user should spawn new thread for each request (at the moment).
- IndexCache in metadata_cache.rs was instantiated struct CratesIoProxyIndexCache.
- Added CacheStats
- Updated crates versions
- Added ProxyConfig for instance configuration
- Added examples
- Added async version.
