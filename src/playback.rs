//! Decides between direct play and an HLS remux, from the device profile the Roku
//! sends with PlaybackInfo. Video is never re-encoded; only audio may be converted.

use serde::Deserialize;
use std::path::Path;

use crate::probe::{MediaInfo, Stream};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DeviceProfile {
    pub direct_play_profiles: Vec<DirectPlayProfile>,
    pub transcoding_profiles: Vec<TranscodingProfile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DirectPlayProfile {
    pub container: Option<String>,
    #[serde(rename = "Type")]
    pub kind: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TranscodingProfile {
    pub container: Option<String>,
    #[serde(rename = "Type")]
    pub kind: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub protocol: Option<String>,
    pub max_audio_channels: Option<String>,
}

/// Matches a comma separated codec/container list. An empty or missing list allows anything.
fn list_allows(list: &Option<String>, value: &str) -> bool {
    match list.as_deref().map(str::trim) {
        None | Some("") => true,
        Some(l) => l.split(',').any(|v| v.trim().eq_ignore_ascii_case(value)),
    }
}

fn list_contains(list: &Option<String>, value: &str) -> bool {
    match list.as_deref() {
        None => false,
        Some(l) => l.split(',').any(|v| v.trim().eq_ignore_ascii_case(value)),
    }
}

/// Container names (as Jellyfin uses them) a file extension may be matched as.
pub fn container_names(path: &Path) -> Vec<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mkv" => vec!["mkv", "matroska"],
        "webm" => vec!["webm"],
        "mp4" => vec!["mp4"],
        "m4v" => vec!["m4v", "mp4"],
        "mov" => vec!["mov"],
        "ts" => vec!["ts", "mpegts"],
        "m2ts" => vec!["m2ts", "ts"],
        "avi" => vec!["avi"],
        "mpg" | "mpeg" => vec!["mpeg", "mpg"],
        "wmv" => vec!["wmv", "asf"],
        "flv" => vec!["flv"],
        _ => vec![],
    }
}

/// The container value reported to the Roku (it echoes it back on the stream URL).
pub fn primary_container(path: &Path) -> String {
    container_names(path).first().copied().unwrap_or("mkv").to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub enum AudioOut {
    Copy,
    Ac3 { channels: u32 },
    Aac { channels: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    DirectPlay,
    /// Remux into HLS/TS, copying video. `audio` says what happens to the chosen audio track.
    Remux { audio: Option<AudioOut>, reasons: Vec<&'static str> },
    /// Would need video re-encoding, which this server never does.
    Unplayable { reasons: Vec<&'static str> },
}

pub fn decide(
    path: &Path,
    info: &MediaInfo,
    audio: Option<&Stream>,
    profile: &DeviceProfile,
) -> Decision {
    let containers = container_names(path);
    let Some(video) = info.video() else {
        return Decision::Unplayable { reasons: vec!["VideoCodecNotSupported"] };
    };

    let mut reasons = Vec::new();
    let video_profiles: Vec<&DirectPlayProfile> = profile
        .direct_play_profiles
        .iter()
        .filter(|p| p.kind.as_deref().map(|k| k.eq_ignore_ascii_case("video")).unwrap_or(true))
        .collect();
    let container_ok: Vec<&&DirectPlayProfile> = video_profiles
        .iter()
        .filter(|p| containers.iter().any(|c| list_contains(&p.container, c)))
        .collect();
    if container_ok.is_empty() {
        reasons.push("ContainerNotSupported");
    }
    let video_ok: Vec<&&&DirectPlayProfile> = container_ok
        .iter()
        .filter(|p| list_allows(&p.video_codec, &video.codec))
        .collect();
    if !container_ok.is_empty() && video_ok.is_empty() {
        reasons.push("VideoCodecNotSupported");
    }
    let audio_ok = video_ok
        .iter()
        .any(|p| audio.map(|a| list_allows(&p.audio_codec, &a.codec)).unwrap_or(true));
    if !video_ok.is_empty() && !audio_ok {
        reasons.push("AudioCodecNotSupported");
    }
    if reasons.is_empty() {
        return Decision::DirectPlay;
    }

    // Remux: the HLS/TS profile must accept the video codec as-is.
    let ts = profile.transcoding_profiles.iter().find(|p| {
        p.kind.as_deref().map(|k| k.eq_ignore_ascii_case("video")).unwrap_or(true)
            && p.protocol.as_deref().map(|x| x.eq_ignore_ascii_case("hls")).unwrap_or(false)
            && p.container.as_deref().map(|c| c.eq_ignore_ascii_case("ts")).unwrap_or(false)
    });
    let ts_video_ok = match ts {
        Some(p) => list_contains(&p.video_codec, &video.codec),
        // no profile: Roku plays h264/hevc in HLS
        None => matches!(video.codec.as_str(), "h264" | "hevc"),
    };
    if !ts_video_ok {
        if !reasons.contains(&"VideoCodecNotSupported") {
            reasons.push("VideoCodecNotSupported");
        }
        return Decision::Unplayable { reasons };
    }

    let audio_out = audio.map(|a| {
        let max_channels = ts
            .and_then(|p| p.max_audio_channels.as_deref())
            .and_then(|c| c.parse::<u32>().ok())
            .unwrap_or(2);
        let src_channels = a.channels.unwrap_or(2);
        let accepted = ts.map(|p| p.audio_codec.clone()).unwrap_or(None);
        if list_contains(&accepted, &a.codec) && src_channels <= max_channels.max(2) {
            AudioOut::Copy
        } else if list_contains(&accepted, "ac3") && max_channels > 2 && src_channels > 2 {
            // ac3_fixed is integer-only, which matters on CPUs without an FPU
            AudioOut::Ac3 { channels: src_channels.min(max_channels).min(6) }
        } else {
            AudioOut::Aac { channels: 2 }
        }
    });
    Decision::Remux { audio: audio_out, reasons }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::StreamKind;

    fn stream(index: usize, kind: StreamKind, codec: &str, channels: Option<u32>) -> Stream {
        Stream {
            index,
            kind,
            codec: codec.into(),
            language: None,
            title: None,
            channels,
            width: None,
            height: None,
            bit_rate: None,
            is_default: true,
            is_forced: false,
        }
    }

    fn info(vcodec: &str, acodec: &str, channels: u32) -> MediaInfo {
        MediaInfo {
            duration_secs: 100.0,
            bit_rate: None,
            size: 0,
            streams: vec![
                stream(0, StreamKind::Video, vcodec, None),
                stream(1, StreamKind::Audio, acodec, Some(channels)),
            ],
        }
    }

    fn roku_profile() -> DeviceProfile {
        serde_json::from_str(
            r#"{
            "DirectPlayProfiles": [
              {"Container":"mkv,webm","Type":"Video","VideoCodec":"h264,hevc,vp9","AudioCodec":"aac,mp3,ac3,eac3,flac,opus"},
              {"Container":"mp4,mov,m4v","Type":"Video","VideoCodec":"h264,hevc","AudioCodec":"aac,mp3,ac3,eac3"}
            ],
            "TranscodingProfiles": [
              {"Container":"ts","Type":"Video","Protocol":"hls","VideoCodec":"h264,hevc","AudioCodec":"ac3,aac,mp3,eac3","MaxAudioChannels":"6"}
            ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn mkv_h264_ac3_direct_plays() {
        let i = info("h264", "ac3", 6);
        let d = decide(Path::new("a.mkv"), &i, i.audio().next(), &roku_profile());
        assert_eq!(d, Decision::DirectPlay);
    }

    #[test]
    fn mkv_hevc_dts_remuxes_with_ac3() {
        let i = info("hevc", "dts", 6);
        let d = decide(Path::new("a.mkv"), &i, i.audio().next(), &roku_profile());
        assert_eq!(
            d,
            Decision::Remux {
                audio: Some(AudioOut::Ac3 { channels: 6 }),
                reasons: vec!["AudioCodecNotSupported"]
            }
        );
    }

    #[test]
    fn stereo_dts_goes_to_aac() {
        let i = info("h264", "dts", 2);
        let d = decide(Path::new("a.mkv"), &i, i.audio().next(), &roku_profile());
        assert!(matches!(d, Decision::Remux { audio: Some(AudioOut::Aac { channels: 2 }), .. }));
    }

    #[test]
    fn avi_h264_mp3_remuxes_copying_audio() {
        let i = info("h264", "mp3", 2);
        let d = decide(Path::new("a.avi"), &i, i.audio().next(), &roku_profile());
        assert_eq!(
            d,
            Decision::Remux { audio: Some(AudioOut::Copy), reasons: vec!["ContainerNotSupported"] }
        );
    }

    #[test]
    fn xvid_is_unplayable() {
        let i = info("mpeg4", "mp3", 2);
        let d = decide(Path::new("a.avi"), &i, i.audio().next(), &roku_profile());
        assert!(matches!(d, Decision::Unplayable { .. }));
    }

    #[test]
    fn empty_direct_profiles_force_remux() {
        let mut p = roku_profile();
        p.direct_play_profiles.clear();
        let i = info("h264", "aac", 2);
        let d = decide(Path::new("a.mkv"), &i, i.audio().next(), &p);
        assert!(matches!(d, Decision::Remux { audio: Some(AudioOut::Copy), .. }));
    }
}
