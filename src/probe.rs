//! ffprobe wrapper with an in-memory cache keyed by path, size and mtime.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Debug, Clone)]
pub struct Stream {
    /// ffprobe stream index, used for `-map 0:<index>`.
    pub index: usize,
    pub kind: StreamKind,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bit_rate: Option<u64>,
    pub is_default: bool,
    pub is_forced: bool,
}

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub duration_secs: f64,
    pub bit_rate: Option<u64>,
    pub size: u64,
    pub streams: Vec<Stream>,
}

impl MediaInfo {
    pub fn video(&self) -> Option<&Stream> {
        self.streams.iter().find(|s| s.kind == StreamKind::Video)
    }
    pub fn audio(&self) -> impl Iterator<Item = &Stream> {
        self.streams.iter().filter(|s| s.kind == StreamKind::Audio)
    }
}

#[derive(Deserialize)]
struct RawOutput {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
}

#[derive(Deserialize)]
struct RawFormat {
    duration: Option<String>,
    bit_rate: Option<String>,
    size: Option<String>,
}

#[derive(Deserialize)]
struct RawStream {
    index: usize,
    codec_type: Option<String>,
    codec_name: Option<String>,
    channels: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
    bit_rate: Option<String>,
    #[serde(default)]
    tags: HashMap<String, String>,
    #[serde(default)]
    disposition: HashMap<String, i64>,
}

pub fn parse_ffprobe_json(json: &str) -> Result<MediaInfo, String> {
    let raw: RawOutput = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let format = raw.format.ok_or("ffprobe output has no format")?;
    let mut streams = Vec::new();
    for s in raw.streams {
        let kind = match s.codec_type.as_deref() {
            Some("video") => StreamKind::Video,
            Some("audio") => StreamKind::Audio,
            Some("subtitle") => StreamKind::Subtitle,
            _ => continue,
        };
        // cover art inside mkv/mp4 shows up as a video stream
        if kind == StreamKind::Video && s.disposition.get("attached_pic") == Some(&1) {
            continue;
        }
        let tag = |k: &str| {
            s.tags
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
        };
        streams.push(Stream {
            index: s.index,
            kind,
            codec: s.codec_name.unwrap_or_default(),
            language: tag("language").filter(|l| l != "und"),
            title: tag("title"),
            channels: s.channels,
            width: s.width,
            height: s.height,
            bit_rate: s.bit_rate.and_then(|b| b.parse().ok()),
            is_default: s.disposition.get("default") == Some(&1),
            is_forced: s.disposition.get("forced") == Some(&1),
        });
    }
    Ok(MediaInfo {
        duration_secs: format.duration.and_then(|d| d.parse().ok()).unwrap_or(0.0),
        bit_rate: format.bit_rate.and_then(|b| b.parse().ok()),
        size: format.size.and_then(|b| b.parse().ok()).unwrap_or(0),
        streams,
    })
}

type CacheKey = (PathBuf, u64, Option<SystemTime>);

pub struct Prober {
    ffprobe: String,
    cache: Mutex<HashMap<CacheKey, MediaInfo>>,
}

impl Prober {
    pub fn new(ffprobe: String) -> Prober {
        Prober {
            ffprobe,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn probe(&self, path: &Path) -> Result<MediaInfo, String> {
        let meta = tokio::fs::metadata(path).await.map_err(|e| e.to_string())?;
        let key = (path.to_path_buf(), meta.len(), meta.modified().ok());
        if let Some(info) = self.cache.lock().unwrap().get(&key) {
            return Ok(info.clone());
        }
        let out = Command::new(&self.ffprobe)
            .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
            .arg(path)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| format!("cannot run {}: {e}", self.ffprobe))?;
        if !out.status.success() {
            return Err(format!(
                "ffprobe failed on {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let info = parse_ffprobe_json(&String::from_utf8_lossy(&out.stdout))?;
        self.cache.lock().unwrap().insert(key, info.clone());
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_streams_and_skips_cover_art() {
        let json = r#"{
          "streams": [
            {"index":0,"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,
             "disposition":{"default":1,"attached_pic":0}},
            {"index":1,"codec_type":"audio","codec_name":"dts","channels":6,
             "tags":{"language":"eng","title":"DTS 5.1"},"disposition":{"default":1}},
            {"index":2,"codec_type":"subtitle","codec_name":"subrip","tags":{"language":"und"}},
            {"index":3,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},
            {"index":4,"codec_type":"attachment","codec_name":"ttf"}
          ],
          "format": {"duration":"5400.5","bit_rate":"8000000","size":"5400000000"}
        }"#;
        let info = parse_ffprobe_json(json).unwrap();
        assert_eq!(info.streams.len(), 3);
        assert_eq!(info.video().unwrap().codec, "hevc");
        let a = info.audio().next().unwrap();
        assert_eq!(a.codec, "dts");
        assert_eq!(a.language.as_deref(), Some("eng"));
        assert!(a.is_default);
        assert_eq!(info.streams[2].language, None);
        assert!((info.duration_secs - 5400.5).abs() < 1e-9);
    }
}
