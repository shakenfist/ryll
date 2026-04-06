//! Embedded WebDAV server for SPICE folder sharing.
//!
//! Wraps `dav-server` (RFC 4918 handler with LocalFs backend) and
//! `hyper` (HTTP/1.1 framing) to serve a local directory over
//! in-process byte streams. Each mux client gets a
//! `tokio::io::DuplexStream`; one end receives demuxed HTTP bytes
//! and the other is driven by hyper's `serve_connection()`.

use std::convert::Infallible;
use std::path::PathBuf;

use anyhow::Result;
use dav_server::fakels::FakeLs;
use dav_server::localfs::LocalFs;
use dav_server::DavHandler;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::DuplexStream;

/// Embedded WebDAV server backed by a local directory.
///
/// Cheaply cloneable — the inner `DavHandler` wraps an `Arc`.
#[derive(Clone)]
pub struct WebdavServer {
    handler: DavHandler,
}

impl WebdavServer {
    /// Create a new WebDAV server serving `root`.
    ///
    /// If `read_only` is true, only GET, HEAD, OPTIONS, and
    /// PROPFIND are allowed; write methods return 403.
    pub fn new(root: PathBuf, read_only: bool) -> Result<Self> {
        let fs = LocalFs::new(root, false, false, false);

        let mut builder = DavHandler::builder()
            .filesystem(fs)
            .locksystem(FakeLs::new());

        if read_only {
            builder = builder.methods(dav_server::DavMethodSet::WEBDAV_RO);
        }

        let handler = builder.build_handler();
        Ok(WebdavServer { handler })
    }

    /// Serve one HTTP/1.1 connection over an in-process byte stream.
    ///
    /// This drives hyper's HTTP parser over the given `DuplexStream`,
    /// dispatching WebDAV requests to the embedded `DavHandler`.
    /// Returns when the connection is closed.
    pub async fn serve_client(&self, stream: DuplexStream) -> Result<()> {
        let io = TokioIo::new(stream);
        let handler = self.handler.clone();

        http1::Builder::new()
            .serve_connection(
                io,
                service_fn(move |req| {
                    let handler = handler.clone();
                    async move { Ok::<_, Infallible>(handler.handle(req).await) }
                }),
            )
            .await
            .map_err(|e| anyhow::anyhow!("webdav: hyper connection error: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Send a raw HTTP request through a DuplexStream and read the response.
    ///
    /// Adds `Connection: close` so hyper closes the connection after the
    /// first response, allowing `read_to_end` to complete.
    async fn roundtrip(server: &WebdavServer, request: &[u8]) -> Vec<u8> {
        let (client_stream, server_stream) = tokio::io::duplex(65536);

        let server = server.clone();
        let server_task = tokio::spawn(async move { server.serve_client(server_stream).await });

        let mut client = client_stream;

        // Inject Connection: close before the final \r\n\r\n so hyper
        // shuts down after the response, letting read_to_end return.
        let req_str = String::from_utf8_lossy(request);
        let patched = req_str.replacen("\r\n\r\n", "\r\nConnection: close\r\n\r\n", 1);
        client.write_all(patched.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        // Server task may return Ok or an error from the closed connection
        let _ = server_task.await;

        response
    }

    /// Parse the HTTP status code from a raw response.
    fn status_code(response: &[u8]) -> u16 {
        let text = String::from_utf8_lossy(response);
        // "HTTP/1.1 200 OK" -> 200
        let status = text
            .split_whitespace()
            .nth(1)
            .expect("no status in response");
        status.parse().expect("invalid status code")
    }

    /// Extract the body from a raw HTTP response (after \r\n\r\n).
    fn response_body(response: &[u8]) -> &[u8] {
        let separator = b"\r\n\r\n";
        for i in 0..response.len().saturating_sub(separator.len()) {
            if &response[i..i + separator.len()] == separator {
                return &response[i + separator.len()..];
            }
        }
        &[]
    }

    /// Extract a header value from a raw HTTP response.
    fn header_value<'a>(response: &'a [u8], name: &str) -> Option<String> {
        let text = String::from_utf8_lossy(response);
        let lower_name = name.to_lowercase();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix(&format!("{}:", lower_name)) {
                return Some(rest.trim().to_string());
            }
            // Case-insensitive match
            if line.to_lowercase().starts_with(&format!("{}:", lower_name)) {
                let colon = line.find(':').unwrap();
                return Some(line[colon + 1..].trim().to_string());
            }
        }
        None
    }

    #[tokio::test]
    async fn options_returns_webdav_methods() {
        let dir = tempfile::tempdir().unwrap();
        let server = WebdavServer::new(dir.path().to_path_buf(), false).unwrap();

        let request = b"OPTIONS / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let response = roundtrip(&server, request).await;

        assert_eq!(status_code(&response), 200);
        let allow = header_value(&response, "allow").expect("no Allow header");
        // Root is a collection — dav-server reports collection-applicable methods
        assert!(
            allow.contains("OPTIONS"),
            "Allow missing OPTIONS: {}",
            allow
        );
        assert!(
            allow.contains("PROPFIND"),
            "Allow missing PROPFIND: {}",
            allow
        );
    }

    #[tokio::test]
    async fn propfind_lists_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let server = WebdavServer::new(dir.path().to_path_buf(), false).unwrap();

        let request = b"PROPFIND / HTTP/1.1\r\n\
            Host: localhost\r\n\
            Depth: 1\r\n\
            Content-Length: 0\r\n\r\n";
        let response = roundtrip(&server, request).await;

        assert_eq!(status_code(&response), 207);
        let body = String::from_utf8_lossy(response_body(&response));
        assert!(
            body.contains("test.txt"),
            "PROPFIND missing test.txt: {}",
            body
        );
    }

    #[tokio::test]
    async fn get_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hello.txt"), "world").unwrap();

        let server = WebdavServer::new(dir.path().to_path_buf(), false).unwrap();

        let request = b"GET /hello.txt HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let response = roundtrip(&server, request).await;

        assert_eq!(status_code(&response), 200);
        let body = response_body(&response);
        assert_eq!(body, b"world");
    }

    #[tokio::test]
    async fn put_creates_file() {
        let dir = tempfile::tempdir().unwrap();

        let server = WebdavServer::new(dir.path().to_path_buf(), false).unwrap();

        let request = b"PUT /newfile.txt HTTP/1.1\r\n\
            Host: localhost\r\n\
            Content-Length: 11\r\n\r\n\
            hello world";
        let response = roundtrip(&server, request).await;

        let code = status_code(&response);
        assert!(code == 201 || code == 204, "unexpected status: {}", code);
        let contents = fs::read_to_string(dir.path().join("newfile.txt")).unwrap();
        assert_eq!(contents, "hello world");
    }

    #[tokio::test]
    async fn put_rejected_when_read_only() {
        let dir = tempfile::tempdir().unwrap();

        let server = WebdavServer::new(dir.path().to_path_buf(), true).unwrap();

        let request = b"PUT /blocked.txt HTTP/1.1\r\n\
            Host: localhost\r\n\
            Content-Length: 4\r\n\r\n\
            data";
        let response = roundtrip(&server, request).await;

        let code = status_code(&response);
        assert!(
            code == 403 || code == 405,
            "expected 403 or 405 for read-only PUT, got: {}",
            code,
        );
        assert!(!dir.path().join("blocked.txt").exists());
    }

    #[tokio::test]
    async fn delete_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doomed.txt");
        fs::write(&path, "bye").unwrap();
        assert!(path.exists());

        let server = WebdavServer::new(dir.path().to_path_buf(), false).unwrap();

        let request = b"DELETE /doomed.txt HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let response = roundtrip(&server, request).await;

        let code = status_code(&response);
        assert!(code == 200 || code == 204, "unexpected status: {}", code);
        assert!(!path.exists(), "file should have been deleted");
    }
}
