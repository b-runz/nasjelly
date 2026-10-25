//! Static file streaming with HTTP Range support (the Roku seeks with byte ranges).

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// Parse a single `bytes=` range against a file length into an inclusive (start, end).
/// `Ok(None)` means no usable Range header; `Err(())` means unsatisfiable.
pub fn parse_range(value: Option<&str>, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(v) = value else { return Ok(None) };
    let Some(spec) = v.trim().strip_prefix("bytes=") else { return Ok(None) };
    // multiple ranges: serve only the first
    let spec = spec.split(',').next().unwrap_or("").trim();
    let Some((a, b)) = spec.split_once('-') else { return Ok(None) };
    let (a, b) = (a.trim(), b.trim());
    if len == 0 {
        return Err(());
    }
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = a.parse().map_err(|_| ())?;
        let end = if b.is_empty() { len - 1 } else { b.parse::<u64>().map_err(|_| ())?.min(len - 1) };
        (start, end)
    };
    if start >= len || start > end {
        return Err(());
    }
    Ok(Some((start, end)))
}

pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("ts") | Some("m2ts") => "video/mp2t",
        Some("avi") => "video/x-msvideo",
        Some("mpg") | Some("mpeg") => "video/mpeg",
        Some("wmv") => "video/x-ms-wmv",
        Some("flv") => "video/x-flv",
        _ => "application/octet-stream",
    }
}

pub async fn serve_file(path: &Path, headers: &HeaderMap) -> Response {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let (status, start, end) = match parse_range(range, len) {
        Ok(Some((s, e))) => (StatusCode::PARTIAL_CONTENT, s, e),
        Ok(None) => (StatusCode::OK, 0, len.saturating_sub(1)),
        Err(()) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap();
        }
    };
    let count = if len == 0 { 0 } else { end - start + 1 };
    if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let body = Body::from_stream(ReaderStream::with_capacity(file.take(count), 256 * 1024));
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type(path))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, count.to_string())
        .body(body)
        .unwrap();
    if status == StatusCode::PARTIAL_CONTENT {
        resp.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")).unwrap(),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), Ok(None));
        assert_eq!(parse_range(Some("bytes=0-"), 100), Ok(Some((0, 99))));
        assert_eq!(parse_range(Some("bytes=10-19"), 100), Ok(Some((10, 19))));
        assert_eq!(parse_range(Some("bytes=90-200"), 100), Ok(Some((90, 99))));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Ok(Some((90, 99))));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Err(()));
        assert_eq!(parse_range(Some("bytes=20-10"), 100), Err(()));
        assert_eq!(parse_range(Some("items=1-2"), 100), Ok(None));
    }
}
