//! A small blocking HTTP client on top of WinHTTP, the HTTP stack that ships
//! with Windows. It gives http and https without bundling a TLS library.
//!
//! Calls block, so they are made from a worker thread, never the UI thread.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Networking::WinHttp::{
    URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY,
    WINHTTP_FLAG_SECURE, WINHTTP_INTERNET_SCHEME_HTTP, WINHTTP_INTERNET_SCHEME_HTTPS,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect,
    WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
};

use crate::util::wide;

/// Responses larger than this are cut off; transcripts are a few kilobytes.
const MAX_BODY: usize = 8 * 1024 * 1024;
const CONNECT_TIMEOUT_MS: i32 = 10_000;

pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
}

/// Closes a WinHTTP handle when it goes out of scope.
struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a handle returned by WinHTTP, closed exactly once.
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

struct Target {
    host: String,
    port: u16,
    path: String,
    secure: bool,
}

fn crack(url: &str) -> Result<Target, String> {
    let url_wide: Vec<u16> = url.encode_utf16().collect();
    // SAFETY: the structure asks for pointers into `url_wide`, which outlives
    // every read below; lengths are returned alongside the pointers.
    unsafe {
        let mut parts: URL_COMPONENTS = std::mem::zeroed();
        parts.dwStructSize = size_of::<URL_COMPONENTS>() as u32;
        parts.dwSchemeLength = u32::MAX;
        parts.dwHostNameLength = u32::MAX;
        parts.dwUrlPathLength = u32::MAX;
        parts.dwExtraInfoLength = u32::MAX;
        if WinHttpCrackUrl(url_wide.as_ptr(), url_wide.len() as u32, 0, &mut parts) == 0
            || parts.lpszHostName.is_null()
            || parts.dwHostNameLength == 0
            || !matches!(
                parts.nScheme,
                WINHTTP_INTERNET_SCHEME_HTTP | WINHTTP_INTERNET_SCHEME_HTTPS
            )
        {
            return Err(format!("Not a valid http(s) address: {url}"));
        }
        let piece = |pointer: *const u16, length: u32| {
            if pointer.is_null() || length == 0 {
                String::new()
            } else {
                String::from_utf16_lossy(std::slice::from_raw_parts(pointer, length as usize))
            }
        };
        let mut path = piece(parts.lpszUrlPath, parts.dwUrlPathLength);
        path.push_str(&piece(parts.lpszExtraInfo, parts.dwExtraInfoLength));
        if path.is_empty() {
            path.push('/');
        }
        Ok(Target {
            host: piece(parts.lpszHostName, parts.dwHostNameLength),
            port: parts.nPort,
            path,
            secure: parts.nScheme == WINHTTP_INTERNET_SCHEME_HTTPS,
        })
    }
}

/// Loopback and private-network hosts are reached directly. A system proxy
/// cannot reach them, and sending dictation to one would be a surprise.
fn is_local(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if host == "localhost" || host == "::1" || !host.contains('.') {
        return true;
    }
    let octets: Vec<u8> = host.split('.').filter_map(|p| p.parse().ok()).collect();
    if octets.len() != 4 || host.split('.').count() != 4 {
        return false;
    }
    matches!(
        (octets[0], octets[1]),
        (127, _) | (10, _) | (192, 168) | (169, 254) | (172, 16..=31)
    )
}

/// Send one request and read the whole response.
///
/// `timeout_secs` is how long to wait for the server to answer once the
/// request is sent, which for these servers is the processing time.
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout_secs: u32,
) -> Result<Response, String> {
    let target = crack(url)?;
    let timeout_ms = (timeout_secs.clamp(1, 3600) * 1000) as i32;
    let access = if is_local(&target.host) {
        WINHTTP_ACCESS_TYPE_NO_PROXY
    } else {
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
    };
    let fail = |step: &str| format!("{} ({step}, {})", describe(last_error()), target.host);

    let agent = wide(concat!("saydrop/", env!("CARGO_PKG_VERSION")));
    let host = wide(&target.host);
    let verb = wide(method);
    let path = wide(&target.path);
    let header_text: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let header_wide = wide(&header_text);

    // SAFETY: every pointer passed is a NUL-terminated buffer or a slice that
    // lives until the call returns; handles are closed by `Handle`.
    unsafe {
        let session = Handle(WinHttpOpen(agent.as_ptr(), access, null(), null(), 0));
        if session.0.is_null() {
            return Err(fail("open"));
        }
        WinHttpSetTimeouts(
            session.0,
            CONNECT_TIMEOUT_MS,
            CONNECT_TIMEOUT_MS,
            timeout_ms,
            timeout_ms,
        );

        let connection = Handle(WinHttpConnect(session.0, host.as_ptr(), target.port, 0));
        if connection.0.is_null() {
            return Err(fail("connect"));
        }

        let request = Handle(WinHttpOpenRequest(
            connection.0,
            verb.as_ptr(),
            path.as_ptr(),
            null(),
            null(),
            null(),
            if target.secure {
                WINHTTP_FLAG_SECURE
            } else {
                0
            },
        ));
        if request.0.is_null() {
            return Err(fail("request"));
        }

        let (header_pointer, header_length) = if header_text.is_empty() {
            (null(), 0)
        } else {
            (header_wide.as_ptr(), u32::MAX)
        };
        let body_pointer = if body.is_empty() {
            null()
        } else {
            body.as_ptr().cast()
        };
        if WinHttpSendRequest(
            request.0,
            header_pointer,
            header_length,
            body_pointer,
            body.len() as u32,
            body.len() as u32,
            0,
        ) == 0
        {
            return Err(fail("send"));
        }
        if WinHttpReceiveResponse(request.0, null_mut()) == 0 {
            return Err(fail("receive"));
        }

        let mut status = 0u32;
        let mut size = size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&raw mut status).cast(),
            &mut size,
            null_mut(),
        ) == 0
        {
            return Err(fail("status"));
        }

        let mut response = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            ) == 0
            {
                return Err(fail("read"));
            }
            if read == 0 || response.len() >= MAX_BODY {
                break;
            }
            response.extend_from_slice(&chunk[..read as usize]);
        }

        Ok(Response {
            status,
            body: response,
        })
    }
}

fn last_error() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetLastError() }
}

/// WinHTTP error codes in words a user can act on.
fn describe(code: u32) -> String {
    match code {
        12002 => "The server did not answer in time".to_string(),
        12007 => "The server name could not be found".to_string(),
        12029 => "Could not connect to the server".to_string(),
        12030 | 12152 => "The connection to the server was lost".to_string(),
        12005 | 12006 => "The server address is not valid".to_string(),
        12175 | 12157 | 12169 => "The secure (https) connection failed".to_string(),
        12180 | 12178 => "The system proxy could not be used".to_string(),
        other => format!("Network error {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_split() {
        let t = crack("http://127.0.0.1:8020/v1/audio/transcriptions").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str(), t.secure),
            ("127.0.0.1", 8020, "/v1/audio/transcriptions", false)
        );
        let t = crack("https://api.example.com/v1/chat/completions?x=1").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str(), t.secure),
            ("api.example.com", 443, "/v1/chat/completions?x=1", true)
        );
        let t = crack("http://host").unwrap();
        assert_eq!((t.port, t.path.as_str()), (80, "/"));
        assert!(crack("not a url").is_err());
        assert!(crack("ftp://host/file").is_err());
    }

    #[test]
    fn local_hosts_skip_the_proxy() {
        for host in [
            "localhost",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.20",
            "172.20.0.5",
            "my-server",
            "[::1]",
        ] {
            assert!(is_local(host), "{host}");
        }
        for host in ["api.example.com", "8.8.8.8", "172.32.0.1", "1.2.3"] {
            assert!(!is_local(host), "{host}");
        }
    }

    #[test]
    fn a_closed_port_is_reported_as_unreachable() {
        // Take a free port from the system, then close it again.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = request("GET", &format!("http://127.0.0.1:{port}/"), &[], &[], 5)
            .err()
            .expect("nothing listens on that port any more");
        assert!(error.contains("Could not connect"), "{error}");
    }

    #[test]
    fn a_request_reaches_a_local_server() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut chunk = [0u8; 1024];
            // Read until the body ("ping") has arrived.
            while !received.ends_with(b"ping") {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 4\r\nConnection: close\r\n\r\npong",
                )
                .unwrap();
            String::from_utf8_lossy(&received).into_owned()
        });

        let response = request(
            "POST",
            &format!("http://127.0.0.1:{port}/some/path?x=1"),
            &[
                ("Content-Type", "text/plain"),
                ("Authorization", "Bearer k"),
            ],
            b"ping",
            10,
        )
        .unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.body, b"pong");

        let received = server.join().unwrap();
        assert!(
            received.starts_with("POST /some/path?x=1 HTTP/1.1\r\n"),
            "{received}"
        );
        assert!(
            received.contains("Content-Type: text/plain\r\n"),
            "{received}"
        );
        assert!(
            received.contains("Authorization: Bearer k\r\n"),
            "{received}"
        );
        assert!(received.contains("Content-Length: 4\r\n"), "{received}");
    }
}
