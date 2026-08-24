//! One HTTPS GET, over WinHTTP.
//!
//! # Why WinHTTP
//!
//! Atoll needs exactly one request: Claude Code's usage endpoint, a couple of
//! times a minute.
//! A Rust HTTP client would bring a TLS stack and a root-certificate bundle with
//! it — megabytes, for one GET, in a binary that sits in the notification area
//! all day. WinHTTP is already on the machine, already trusts the certificate
//! store the rest of Windows trusts, and needs no dependency Atoll does not
//! already have.
//!
//! The cost is this file: handle lifetimes managed by hand. Every handle is
//! closed on every path, including the error ones, which is what [`Handle`] is
//! for.

use std::io;

use windows::Win32::Networking::WinHttp::*;
use windows::core::PCWSTR;

/// How long any single stage of the request may take.
const TIMEOUT_MS: i32 = 8_000;

/// A WinHTTP handle that closes itself.
struct Handle(*mut std::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `GET https://<host><path>` with the given headers, returning the body.
///
/// Anything other than a 2xx is an error carrying the status code and no body:
/// the body of a failed request from an authenticated endpoint is exactly the
/// kind of thing that should not end up in a log.
pub fn get_json(host: &str, path: &str, headers: &[(&str, &str)]) -> io::Result<String> {
    let agent = wide("Atoll");
    let session = Handle(unsafe {
        WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    });
    if session.0.is_null() {
        return Err(last_error("WinHttpOpen"));
    }
    unsafe {
        WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS)
            .map_err(|error| io::Error::other(format!("WinHttpSetTimeouts: {error}")))?;
    }

    let host_wide = wide(host);
    let connection = Handle(unsafe {
        WinHttpConnect(
            session.0,
            PCWSTR(host_wide.as_ptr()),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        )
    });
    if connection.0.is_null() {
        return Err(last_error("WinHttpConnect"));
    }

    let verb = wide("GET");
    let path_wide = wide(path);
    let request = Handle(unsafe {
        WinHttpOpenRequest(
            connection.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null_mut(),
            WINHTTP_FLAG_SECURE,
        )
    });
    if request.0.is_null() {
        return Err(last_error("WinHttpOpenRequest"));
    }

    let header_block: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let header_wide = wide(&header_block);

    unsafe {
        WinHttpSendRequest(
            request.0,
            Some(&header_wide[..header_wide.len() - 1]),
            None,
            0,
            0,
            0,
        )
        .map_err(|error| io::Error::other(format!("WinHttpSendRequest: {error}")))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|error| io::Error::other(format!("WinHttpReceiveResponse: {error}")))?;
    }

    let status = status_code(request.0)?;
    if !(200..300).contains(&status) {
        return Err(io::Error::other(format!("HTTP {status}")));
    }

    let mut body = Vec::new();
    loop {
        let mut available = 0u32;
        unsafe { WinHttpQueryDataAvailable(request.0, &mut available) }
            .map_err(|error| io::Error::other(format!("WinHttpQueryDataAvailable: {error}")))?;
        if available == 0 {
            break;
        }
        let mut chunk = vec![0u8; available as usize];
        let mut read = 0u32;
        unsafe {
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut std::ffi::c_void,
                available,
                &mut read,
            )
        }
        .map_err(|error| io::Error::other(format!("WinHttpReadData: {error}")))?;
        if read == 0 {
            break;
        }
        chunk.truncate(read as usize);
        body.extend_from_slice(&chunk);

        // A response this big is not the one we asked for.
        if body.len() > 1 << 20 {
            return Err(io::Error::other("response too large"));
        }
    }

    String::from_utf8(body).map_err(|_| io::Error::other("response was not UTF-8"))
}

fn status_code(request: *mut std::ffi::c_void) -> io::Result<u32> {
    let mut status = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    unsafe {
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut std::ffi::c_void),
            &mut size,
            std::ptr::null_mut(),
        )
    }
    .map_err(|error| io::Error::other(format!("WinHttpQueryHeaders: {error}")))?;
    Ok(status)
}

fn last_error(stage: &str) -> io::Error {
    io::Error::other(format!("{stage}: {}", io::Error::last_os_error()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the whole client path — DNS, TLS against the system certificate
    /// store, request, response, status parsing — against the real endpoint,
    /// **with no credentials at all**.
    ///
    /// An unauthenticated request is refused, and being refused is the point: a
    /// 4xx means every stage before reading the body worked. A green run here
    /// distinguishes "Atoll cannot reach the endpoint" from "the endpoint said
    /// no", which is the question that actually comes up.
    ///
    /// Ignored by default: it needs the network, and a test suite that fails on
    /// a train is a worse test suite.
    /// A one-off diagnostic for "the panel says usage unavailable": reports
    /// which link of the fallback chain produced the reading, without printing
    /// any of it. Ignored, and run by hand.
    #[test]
    #[ignore = "reads the machine's real caches"]
    fn the_fallback_chain_produces_a_reading() {
        use atoll_core::usage;

        let own = usage::claude_usage_cache_path();
        let foreign = usage::foreign_usage_cache_path();
        println!("own cache path ok: {}", own.is_ok());
        println!("foreign cache path ok: {}", foreign.is_ok());
        if let Ok(path) = &foreign {
            println!("foreign exists: {}", path.exists());
            match usage::read_claude_usage_cache(path) {
                Ok(Some(limits)) => println!("foreign parsed: {} limits", limits.limits.len()),
                Ok(None) => println!("foreign parsed: nothing"),
                Err(error) => println!("foreign unreadable: {error}"),
            }
        }
        let token = usage::claude_credentials_path()
            .ok()
            .and_then(|path| usage::read_claude_oauth_token(&path));
        println!("token readable: {}", token.is_some());

        let limits = crate::usage_cache::fetch_claude_limits(
            atoll_core::now_unix_secs(),
            atoll_core::usage::CLAUDE_USAGE_TTL_SECS,
        );
        println!(
            "chain result: {} limits, stamped: {}",
            limits.limits.len(),
            limits.fetched_at.is_some()
        );
    }

    #[test]
    #[ignore = "hits the network"]
    fn the_client_reaches_the_usage_endpoint() {
        let error = get_json("api.anthropic.com", "/api/oauth/usage", &[])
            .expect_err("an unauthenticated request must not succeed");
        let message = error.to_string();
        assert!(
            message.starts_with("HTTP 4"),
            "expected a refusal from the server, not a transport failure: {message}"
        );
    }
}
