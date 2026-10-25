//! Reads keyframe times from a Matroska file's Cues index without scanning the file.
//! Only the header area and the Cues element are read, which keeps it cheap on the NAS.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const ID_EBML: u32 = 0x1A45DFA3;
const ID_SEGMENT: u32 = 0x18538067;
const ID_SEEKHEAD: u32 = 0x114D9B74;
const ID_SEEK: u32 = 0x4DBB;
const ID_SEEKID: u32 = 0x53AB;
const ID_SEEKPOSITION: u32 = 0x53AC;
const ID_INFO: u32 = 0x1549A966;
const ID_TIMESTAMPSCALE: u32 = 0x2AD7B1;
const ID_TRACKS: u32 = 0x1654AE6B;
const ID_TRACKENTRY: u32 = 0xAE;
const ID_TRACKNUMBER: u32 = 0xD7;
const ID_TRACKTYPE: u32 = 0x83;
const ID_CUES: u32 = 0x1C53BB6B;
const ID_CUEPOINT: u32 = 0xBB;
const ID_CUETIME: u32 = 0xB3;
const ID_CUETRACKPOSITIONS: u32 = 0xB7;
const ID_CUETRACK: u32 = 0xF7;
const ID_CLUSTER: u32 = 0x1F43B675;

/// Largest master element we are willing to load into memory.
const MAX_ELEMENT: u64 = 64 * 1024 * 1024;

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Element id (marker bits kept) from a reader. Returns (id, bytes consumed).
fn read_id<R: Read>(r: &mut R) -> io::Result<(u32, u64)> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first)?;
    let len = first[0].leading_zeros() as usize + 1;
    if len > 4 {
        return Err(bad("invalid element id"));
    }
    let mut id = first[0] as u32;
    for _ in 1..len {
        r.read_exact(&mut first)?;
        id = (id << 8) | first[0] as u32;
    }
    Ok((id, len as u64))
}

/// Element size; `None` means "unknown size". Returns (size, bytes consumed).
fn read_size<R: Read>(r: &mut R) -> io::Result<(Option<u64>, u64)> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    let len = b[0].leading_zeros() as usize + 1;
    if len > 8 {
        return Err(bad("invalid element size"));
    }
    let mask = if len == 8 { 0 } else { 0xFFu8 >> len };
    let mut value = (b[0] & mask) as u64;
    let mut all_ones = (b[0] & mask) == mask;
    for _ in 1..len {
        r.read_exact(&mut b)?;
        all_ones &= b[0] == 0xFF;
        value = (value << 8) | b[0] as u64;
    }
    Ok((if all_ones { None } else { Some(value) }, len as u64))
}

/// Iterate (id, payload) children of an in-memory master element.
fn children(mut buf: &[u8]) -> Vec<(u32, &[u8])> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let mut cur = io::Cursor::new(buf);
        let Ok((id, _)) = read_id(&mut cur) else { break };
        let Ok((Some(size), _)) = read_size(&mut cur) else { break };
        let start = cur.position() as usize;
        let end = start.saturating_add(size as usize);
        if end > buf.len() {
            break;
        }
        out.push((id, &buf[start..end]));
        buf = &buf[end..];
    }
    out
}

fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0u64, |acc, &x| (acc << 8) | x as u64)
}

fn read_payload(f: &mut File, pos: u64, size: u64) -> io::Result<Vec<u8>> {
    if size > MAX_ELEMENT {
        return Err(bad("element too large"));
    }
    f.seek(SeekFrom::Start(pos))?;
    let mut buf = vec![0u8; size as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Keyframe times in seconds, sorted, from the Cues index. `Ok(None)` when the file
/// is not Matroska or has no usable index.
pub fn keyframes(path: &Path) -> io::Result<Option<Vec<f64>>> {
    let mut f = File::open(path)?;
    let file_len = f.metadata()?.len();

    let (id, _) = read_id(&mut f)?;
    if id != ID_EBML {
        return Ok(None);
    }
    let (Some(size), _) = read_size(&mut f)? else { return Ok(None) };
    f.seek(SeekFrom::Current(size as i64))?;

    let (id, _) = read_id(&mut f)?;
    if id != ID_SEGMENT {
        return Ok(None);
    }
    let (seg_size, _) = read_size(&mut f)?;
    let seg_start = f.stream_position()?;
    let seg_end = seg_size.map(|s| seg_start + s).unwrap_or(file_len).min(file_len);

    let mut scale: u64 = 1_000_000;
    let mut video_track: Option<u64> = None;
    let mut cues_pos: Option<u64> = None;

    let mut pos = seg_start;
    while pos < seg_end {
        f.seek(SeekFrom::Start(pos))?;
        let (id, id_len) = read_id(&mut f)?;
        let (size, size_len) = read_size(&mut f)?;
        let data = pos + id_len + size_len;
        let Some(size) = size else { break };
        match id {
            ID_SEEKHEAD => {
                let buf = read_payload(&mut f, data, size)?;
                for (cid, seek) in children(&buf) {
                    if cid != ID_SEEK {
                        continue;
                    }
                    let mut target = None;
                    let mut position = None;
                    for (sid, v) in children(seek) {
                        match sid {
                            ID_SEEKID => target = Some(uint(v) as u32),
                            ID_SEEKPOSITION => position = Some(uint(v)),
                            _ => {}
                        }
                    }
                    if target == Some(ID_CUES) {
                        if let Some(p) = position {
                            cues_pos = Some(seg_start + p);
                        }
                    }
                }
            }
            ID_INFO => {
                let buf = read_payload(&mut f, data, size)?;
                for (cid, v) in children(&buf) {
                    if cid == ID_TIMESTAMPSCALE {
                        scale = uint(v);
                    }
                }
            }
            ID_TRACKS => {
                let buf = read_payload(&mut f, data, size)?;
                for (cid, entry) in children(&buf) {
                    if cid != ID_TRACKENTRY {
                        continue;
                    }
                    let mut number = None;
                    let mut kind = None;
                    for (tid, v) in children(entry) {
                        match tid {
                            ID_TRACKNUMBER => number = Some(uint(v)),
                            ID_TRACKTYPE => kind = Some(uint(v)),
                            _ => {}
                        }
                    }
                    if kind == Some(1) && video_track.is_none() {
                        video_track = number;
                    }
                }
            }
            ID_CUES => {
                cues_pos = Some(pos);
                break;
            }
            ID_CLUSTER => break,
            _ => {}
        }
        pos = data + size;
    }

    let Some(cues_pos) = cues_pos else { return Ok(None) };
    f.seek(SeekFrom::Start(cues_pos))?;
    let (id, id_len) = read_id(&mut f)?;
    let (Some(size), size_len) = read_size(&mut f)? else { return Ok(None) };
    if id != ID_CUES {
        return Ok(None);
    }
    let buf = read_payload(&mut f, cues_pos + id_len + size_len, size)?;

    let mut times = Vec::new();
    for (cid, point) in children(&buf) {
        if cid != ID_CUEPOINT {
            continue;
        }
        let mut time = None;
        let mut for_video = video_track.is_none();
        for (pid, v) in children(point) {
            match pid {
                ID_CUETIME => time = Some(uint(v)),
                ID_CUETRACKPOSITIONS => {
                    for (tid, tv) in children(v) {
                        if tid == ID_CUETRACK && Some(uint(tv)) == video_track {
                            for_video = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if let (Some(t), true) = (time, for_video) {
            times.push(t as f64 * scale as f64 / 1e9);
        }
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    times.dedup();
    if times.len() < 2 {
        return Ok(None);
    }
    Ok(Some(times))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn reads_size_and_unknown_size() {
        let mut c = io::Cursor::new([0x81u8]);
        assert_eq!(read_size(&mut c).unwrap(), (Some(1), 1));
        let mut c = io::Cursor::new([0x40u8, 0x02]);
        assert_eq!(read_size(&mut c).unwrap(), (Some(2), 2));
        let mut c = io::Cursor::new([0x01u8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(read_size(&mut c).unwrap(), (None, 8));
    }

    #[test]
    fn non_mkv_returns_none() {
        let p = std::env::temp_dir().join(format!("nasjelly-notmkv-{}", std::process::id()));
        std::fs::write(&p, b"definitely not matroska").unwrap();
        assert_eq!(keyframes(&p).unwrap(), None);
    }

    /// Needs ffmpeg on PATH; skipped otherwise.
    #[test]
    fn reads_cues_from_ffmpeg_mkv() {
        let p = std::env::temp_dir().join(format!("nasjelly-cues-{}.mkv", std::process::id()));
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=25:duration=10",
                   "-f", "lavfi", "-i", "sine=duration=10",
                   "-c:v", "libx264", "-g", "50", "-keyint_min", "50", "-sc_threshold", "0", "-c:a", "aac"])
            .arg(&p)
            .status();
        let Ok(status) = status else { return };
        assert!(status.success());
        let kf = keyframes(&p).unwrap().expect("cues");
        assert_eq!(kf.len(), 5, "{kf:?}");
        assert!(kf[0].abs() < 0.1);
        assert!((kf[1] - 2.0).abs() < 0.1, "{kf:?}");
    }
}
