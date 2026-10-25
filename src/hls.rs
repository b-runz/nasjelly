//! On-demand HLS remux. The playlist lists every segment up front (so the Roku can
//! seek anywhere); each segment is cut by its own ffmpeg run starting on a keyframe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::process::Command;

use crate::playback::AudioOut;

pub const TARGET_SEGMENT_SECS: f64 = 6.0;

/// Segment boundaries from keyframe times: a new segment starts on the first keyframe at
/// least `target` seconds after the previous boundary. Returns (start, end) pairs.
pub fn plan_segments(keyframes: &[f64], duration: f64, target: f64) -> Vec<(f64, f64)> {
    let mut bounds = vec![0.0];
    for &k in keyframes {
        if k >= duration {
            break;
        }
        if k - bounds.last().unwrap() >= target {
            bounds.push(k);
        }
    }
    let mut segs: Vec<(f64, f64)> = bounds.windows(2).map(|w| (w[0], w[1])).collect();
    let last = *bounds.last().unwrap();
    if duration > last {
        segs.push((last, duration));
    }
    segs
}

/// Fallback when no keyframe index is available: fixed-length segments.
pub fn fixed_segments(duration: f64, target: f64) -> Vec<(f64, f64)> {
    let mut segs = Vec::new();
    let mut t = 0.0;
    while t < duration {
        let end = (t + target).min(duration);
        segs.push((t, end));
        t = end;
    }
    segs
}

pub fn playlist(segments: &[(f64, f64)], query: &str) -> String {
    let max = segments.iter().map(|(s, e)| e - s).fold(0.0, f64::max);
    let mut out = String::new();
    out.push_str("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-PLAYLIST-TYPE:VOD\n");
    out.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", max.ceil() as u64));
    out.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    for (i, (s, e)) in segments.iter().enumerate() {
        out.push_str(&format!("#EXTINF:{:.6},\nhls/{i}.ts", e - s));
        if !query.is_empty() {
            out.push('?');
            out.push_str(query);
        }
        out.push('\n');
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// ffmpeg's CLI seeks 3/23 s early when the video has B-frames, which would land on the
/// previous keyframe; seeking slightly past the keyframe makes it land on the right one.
const SEEK_NUDGE: f64 = 0.135;

/// `-segment_times` values for a job starting at segment `first`. The muxer splits on the
/// first keyframe at or after each time, measured from the job's start, with a small
/// offset after seeking; aiming halfway between keyframes keeps the split on target.
pub fn split_times(segments: &[(f64, f64)], keyframes: Option<&[f64]>, first: usize) -> Vec<f64> {
    let origin = segments[first].0;
    segments[first + 1..]
        .iter()
        .map(|&(b, _)| {
            let prev = keyframes
                .and_then(|k| k.iter().rev().find(|&&t| t < b - 1e-6).copied())
                .unwrap_or(b);
            ((prev + b) / 2.0 - origin).max(0.001)
        })
        .collect()
}

/// ffmpeg arguments for a segmenting job that starts at segment `first` and runs to the end.
pub fn job_args(
    path: &Path,
    segments: &[(f64, f64)],
    keyframes: Option<&[f64]>,
    first: usize,
    video_index: usize,
    audio: Option<(usize, &AudioOut)>,
    dir: &Path,
) -> Vec<String> {
    let start = segments[first].0;
    let mut a: Vec<String> = vec!["-nostdin".into(), "-v".into(), "error".into()];
    let seek = if start > 0.0 { start + SEEK_NUDGE } else { 0.0 };
    if seek > 0.0 {
        a.extend(["-ss".into(), format!("{seek:.3}")]);
    }
    a.extend(["-i".into(), path.to_string_lossy().into_owned()]);
    a.extend(["-map".into(), format!("0:{video_index}"), "-c:v".into(), "copy".into()]);
    if let Some((index, out)) = audio {
        a.extend(["-map".into(), format!("0:{index}")]);
        match out {
            AudioOut::Copy => a.extend(["-c:a".into(), "copy".into()]),
            AudioOut::Ac3 { channels } => a.extend([
                "-c:a".into(),
                "ac3_fixed".into(),
                "-ac".into(),
                channels.to_string(),
                "-b:a".into(),
                if *channels > 2 { "448k".into() } else { "192k".into() },
            ]),
            AudioOut::Aac { channels } => a.extend([
                "-c:a".into(),
                "aac".into(),
                "-ac".into(),
                channels.to_string(),
                "-b:a".into(),
                "192k".into(),
            ]),
        }
    }
    a.extend(["-sn".into(), "-dn".into()]);
    if seek > 0.0 {
        // place output on the source timeline so segments after a re-seek line up
        a.extend(["-output_ts_offset".into(), format!("{seek:.3}")]);
    }
    a.extend(["-f".into(), "segment".into(), "-segment_format".into(), "mpegts".into()]);
    let times = split_times(segments, keyframes, first);
    if !times.is_empty() {
        let list: Vec<String> = times.iter().map(|t| format!("{t:.3}")).collect();
        a.extend(["-segment_times".into(), list.join(",")]);
    }
    a.extend([
        "-segment_start_number".into(),
        first.to_string(),
        "-segment_list".into(),
        dir.join("list.csv").to_string_lossy().into_owned(),
        "-segment_list_type".into(),
        "csv".into(),
        dir.join("%d.ts").to_string_lossy().into_owned(),
    ]);
    a
}

/// Segment numbers ffmpeg has finished, from its csv segment list ("12.ts,start,end").
pub fn finished_segments(list: &str) -> Vec<usize> {
    list.lines()
        .filter_map(|l| l.split(',').next()?.strip_suffix(".ts")?.parse().ok())
        .collect()
}

/// How many finished segments may sit ahead of the Roku before ffmpeg is paused.
const MAX_AHEAD: usize = 5;
/// A job nobody asked anything of for this long is stopped and cleaned up.
const IDLE_SECS: u64 = 90;
/// How long a segment request waits for a running job before giving up.
const WAIT_SECS: u64 = 60;

struct Job {
    key: String,
    dir: PathBuf,
    first: usize,
    child: tokio::process::Child,
    requested: usize,
    last_request: Instant,
    paused: bool,
}

impl Job {
    fn finished(&self) -> Vec<usize> {
        std::fs::read_to_string(self.dir.join("list.csv"))
            .map(|s| finished_segments(&s))
            .unwrap_or_default()
    }

    fn signal(&mut self, sig: i32) {
        if let Some(pid) = self.child.id() {
            unsafe {
                libc::kill(pid as i32, sig);
            }
        }
    }

    fn stop(&mut self) {
        if self.paused {
            self.signal(libc::SIGCONT);
        }
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub struct JobSpec<'a> {
    /// Identifies the output (file + audio choice); a different key replaces the job.
    pub key: String,
    pub path: &'a Path,
    pub segments: &'a [(f64, f64)],
    pub keyframes: Option<&'a [f64]>,
    pub video_index: usize,
    pub audio: Option<(usize, &'a AudioOut)>,
}

/// Runs at most one segmenting ffmpeg (one Roku watching one thing).
pub struct Transcoder {
    ffmpeg: String,
    base: PathBuf,
    job: Arc<tokio::sync::Mutex<Option<Job>>>,
}

impl Transcoder {
    pub fn new(ffmpeg: String, base: PathBuf) -> Transcoder {
        // segments left behind by a previous run
        let _ = std::fs::remove_dir_all(&base);
        let t = Transcoder {
            ffmpeg,
            base,
            job: Arc::new(tokio::sync::Mutex::new(None)),
        };
        tokio::spawn(Self::housekeeping(t.job.clone()));
        t
    }

    /// Pause ffmpeg when it runs ahead of the Roku, resume when it falls behind,
    /// and stop jobs nobody is watching.
    async fn housekeeping(job: Arc<tokio::sync::Mutex<Option<Job>>>) {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut guard = job.lock().await;
            let Some(j) = guard.as_mut() else { continue };
            if j.last_request.elapsed() > Duration::from_secs(IDLE_SECS) {
                println!("transcode: stopping idle job {}", j.key);
                j.stop();
                *guard = None;
                continue;
            }
            let done = j.finished().into_iter().max();
            let ahead = done.map(|d| d.saturating_sub(j.requested)).unwrap_or(0);
            if !j.paused && ahead >= MAX_AHEAD {
                j.signal(libc::SIGSTOP);
                j.paused = true;
            } else if j.paused && ahead < MAX_AHEAD - 2 {
                j.signal(libc::SIGCONT);
                j.paused = false;
            }
        }
    }

    fn start(&self, spec: &JobSpec, first: usize) -> std::io::Result<Job> {
        let dir = self.base.join(format!("{}-{first}", crate::library::hash_id(&spec.key)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let args = job_args(spec.path, spec.segments, spec.keyframes, first, spec.video_index, spec.audio, &dir);
        println!("transcode: {} from segment {first}", spec.path.display());
        let child = tokio::process::Command::new(&self.ffmpeg)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Job {
            key: spec.key.clone(),
            dir,
            first,
            child,
            requested: first,
            last_request: Instant::now(),
            paused: false,
        })
    }

    /// Path of finished segment `n`, starting or restarting ffmpeg as needed.
    pub async fn segment(&self, spec: &JobSpec<'_>, n: usize) -> Result<PathBuf, String> {
        if n >= spec.segments.len() {
            return Err(format!("no segment {n}"));
        }
        let deadline = Instant::now() + Duration::from_secs(WAIT_SECS);
        loop {
            {
                let mut guard = self.job.lock().await;
                let restart = match guard.as_mut() {
                    Some(j) if j.key == spec.key => {
                        j.last_request = Instant::now();
                        let finished = j.finished();
                        if finished.contains(&n) {
                            j.requested = n;
                            // drop segments the Roku has moved past
                            for old in finished.iter().filter(|&&s| s + 3 < n) {
                                let _ = std::fs::remove_file(j.dir.join(format!("{old}.ts")));
                            }
                            return Ok(j.dir.join(format!("{n}.ts")));
                        }
                        let produced = finished.iter().copied().max().unwrap_or(j.first.saturating_sub(1));
                        let running = matches!(j.child.try_wait(), Ok(None));
                        let soon = n >= j.first && n <= produced + 2;
                        if soon && !running {
                            return Err(format!("ffmpeg ended without producing segment {n}"));
                        }
                        if soon {
                            // keep going towards n even if we were paused
                            j.requested = j.requested.max(n.saturating_sub(MAX_AHEAD - 1));
                            if j.paused {
                                j.signal(libc::SIGCONT);
                                j.paused = false;
                            }
                            false
                        } else {
                            true
                        }
                    }
                    _ => true,
                };
                if restart {
                    if let Some(mut old) = guard.take() {
                        old.stop();
                    }
                    *guard = Some(self.start(spec, n).map_err(|e| format!("cannot start ffmpeg: {e}"))?);
                }
            }
            if Instant::now() > deadline {
                return Err(format!("timed out waiting for segment {n}"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Keyframe times per file: Matroska Cues first, then an ffprobe packet scan.
pub struct Keyframes {
    ffprobe: String,
    cache: Mutex<HashMap<PathBuf, Arc<Option<Vec<f64>>>>>,
}

impl Keyframes {
    pub fn new(ffprobe: String) -> Keyframes {
        Keyframes {
            ffprobe,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get(&self, path: &Path) -> Arc<Option<Vec<f64>>> {
        if let Some(k) = self.cache.lock().unwrap().get(path) {
            return k.clone();
        }
        let p = path.to_path_buf();
        let mut kf = tokio::task::spawn_blocking(move || crate::mkv::keyframes(&p).ok().flatten())
            .await
            .ok()
            .flatten();
        if kf.is_none() {
            kf = self.scan(path).await;
        }
        let kf = Arc::new(kf);
        self.cache.lock().unwrap().insert(path.to_path_buf(), kf.clone());
        kf
    }

    async fn scan(&self, path: &Path) -> Option<Vec<f64>> {
        let out = Command::new(&self.ffprobe)
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
            .arg(path)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let mut times = parse_packet_scan(&String::from_utf8_lossy(&out.stdout));
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        times.dedup();
        (times.len() >= 2).then_some(times)
    }
}

fn parse_packet_scan(text: &str) -> Vec<f64> {
    text.lines()
        .filter_map(|l| {
            let mut parts = l.split(',');
            let t = parts.next()?.trim().parse::<f64>().ok()?;
            let flags = parts.next()?;
            flags.starts_with('K').then_some(t)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_segments_on_keyframes() {
        let kf = [0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0];
        let segs = plan_segments(&kf, 15.0, 6.0);
        assert_eq!(segs, vec![(0.0, 6.0), (6.0, 12.0), (12.0, 15.0)]);
    }

    #[test]
    fn plans_long_gop() {
        let kf = [0.0, 10.0, 20.0];
        let segs = plan_segments(&kf, 25.0, 6.0);
        assert_eq!(segs, vec![(0.0, 10.0), (10.0, 20.0), (20.0, 25.0)]);
    }

    #[test]
    fn fixed_segments_cover_duration() {
        assert_eq!(fixed_segments(13.0, 6.0), vec![(0.0, 6.0), (6.0, 12.0), (12.0, 13.0)]);
    }

    #[test]
    fn playlist_lists_segments_with_query() {
        let p = playlist(&[(0.0, 6.0), (6.0, 7.5)], "ApiKey=x");
        assert!(p.contains("#EXT-X-TARGETDURATION:6\n"));
        assert!(p.contains("#EXTINF:1.500000,\nhls/1.ts?ApiKey=x\n"));
        assert!(p.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn parses_packet_scan() {
        let t = parse_packet_scan("0.000000,K__\n0.040000,___\n2.000000,K_\nN/A,K_\n");
        assert_eq!(t, vec![0.0, 2.0]);
    }

    #[test]
    fn split_times_aim_between_keyframes() {
        let kf = [0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0];
        let segs = plan_segments(&kf, 15.0, 6.0);
        assert_eq!(split_times(&segs, Some(&kf), 0), vec![5.0, 11.0]);
        assert_eq!(split_times(&segs, Some(&kf), 1), vec![5.0]);
        assert!(split_times(&segs, Some(&kf), 2).is_empty());
    }

    #[test]
    fn job_args_seek_and_convert_audio() {
        let kf = [0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0];
        let segs = plan_segments(&kf, 15.0, 6.0);
        let a = job_args(Path::new("/x.mkv"), &segs, Some(&kf), 1, 0, Some((1, &AudioOut::Ac3 { channels: 6 })), Path::new("/t"));
        let s = a.join(" ");
        assert!(s.contains("-ss 6.135 -i /x.mkv"), "{s}");
        assert!(s.contains("-map 0:1 -c:a ac3_fixed -ac 6 -b:a 448k"));
        assert!(s.contains("-output_ts_offset 6.135"));
        assert!(s.contains("-segment_times 5.000 -segment_start_number 1"));
        assert!(s.ends_with("-segment_list_type csv /t/%d.ts"));
    }

    #[test]
    fn parses_segment_list() {
        assert_eq!(finished_segments("2.ts,0.0,6.0\n3.ts,6.0,12.0\n"), vec![2, 3]);
    }
}
