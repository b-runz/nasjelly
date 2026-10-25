//! Folder-backed "library": no scanning, directories are read when the Roku asks.
//! Item ids are md5(path) as 32 lowercase hex characters.

use md5::{Digest, Md5};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use crate::config::Folder;

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "mov", "avi", "ts", "m2ts", "webm", "mpg", "mpeg", "wmv", "flv",
];

pub fn hash_id(s: &str) -> String {
    let digest = Md5::digest(s.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn path_id(path: &Path) -> String {
    hash_id(&path.to_string_lossy())
}

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone)]
pub struct Root {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

pub struct Library {
    pub roots: Vec<Root>,
    known: RwLock<HashMap<String, PathBuf>>,
}

impl Library {
    pub fn new(folders: &[Folder]) -> Library {
        let roots: Vec<Root> = folders
            .iter()
            .map(|f| Root {
                id: path_id(&f.path),
                name: f.name.clone(),
                path: f.path.clone(),
            })
            .collect();
        let known = roots.iter().map(|r| (r.id.clone(), r.path.clone())).collect();
        Library {
            roots,
            known: RwLock::new(known),
        }
    }

    pub fn root_by_id(&self, id: &str) -> Option<&Root> {
        self.roots.iter().find(|r| r.id == id)
    }

    /// Map an id back to a path. Ids seen in listings are cached; after a restart an
    /// unknown id triggers a walk of the configured folders.
    pub fn resolve(&self, id: &str) -> Option<PathBuf> {
        if let Some(p) = self.known.read().unwrap().get(id) {
            return Some(p.clone());
        }
        for root in &self.roots {
            if let Some(p) = self.walk_find(&root.path, id) {
                return Some(p);
            }
        }
        None
    }

    fn walk_find(&self, dir: &Path, id: &str) -> Option<PathBuf> {
        let entries = self.list_dir(dir).ok()?;
        let mut subdirs = Vec::new();
        for e in entries {
            if e.id == id {
                return Some(e.path);
            }
            if e.is_dir {
                subdirs.push(e.path);
            }
        }
        subdirs.into_iter().find_map(|d| self.walk_find(&d, id))
    }

    /// Visible children of a directory: subdirectories and video files, hidden files skipped.
    pub fn list_dir(&self, dir: &Path) -> std::io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        for de in std::fs::read_dir(dir)? {
            let Ok(de) = de else { continue };
            let name = de.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with("@__") || name == "@Recycle" {
                continue;
            }
            let path = de.path();
            // follow symlinks
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let is_dir = meta.is_dir();
            if !is_dir && !(meta.is_file() && is_video(&path)) {
                continue;
            }
            out.push(Entry {
                id: path_id(&path),
                path,
                is_dir,
                modified: meta.modified().ok(),
            });
        }
        {
            let mut known = self.known.write().unwrap();
            for e in &out {
                known.insert(e.id.clone(), e.path.clone());
            }
        }
        Ok(out)
    }
}

/// Display name for a file: the file name without the extension.
pub fn display_name(path: &Path, is_dir: bool) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if is_dir {
        return name;
    }
    match path.file_stem() {
        Some(stem) => stem.to_string_lossy().into_owned(),
        None => name,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExternalSub {
    pub path: PathBuf,
    pub language: Option<String>,
    pub title: String,
}

/// .srt files belonging to a video: `<stem>*.srt` next to it, and anything under a
/// `Subs`/`Subtitles` folder beside it (including `Subs/<stem>/`), as release folders do.
pub fn external_subs(video: &Path) -> Vec<ExternalSub> {
    let Some(dir) = video.parent() else { return vec![] };
    let stem = video.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut found = Vec::new();

    let srt_files = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("srt")) == Some(true)
            })
            .collect();
        v.sort();
        v
    };

    for p in srt_files(dir) {
        let fstem = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        if fstem.starts_with(&stem) {
            let rest = fstem[stem.len()..].trim_start_matches(['.', '_', '-', ' ']).to_string();
            found.push(make_sub(p, &rest));
        }
    }

    let mut sub_dirs = Vec::new();
    for de in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = de.file_name().to_string_lossy().to_lowercase();
        if (name == "subs" || name == "subtitles") && de.path().is_dir() {
            sub_dirs.push(de.path());
        }
    }
    for sd in sub_dirs {
        for p in srt_files(&sd) {
            let fstem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            found.push(make_sub(p, &fstem));
        }
        for de in std::fs::read_dir(&sd).into_iter().flatten().flatten() {
            if de.path().is_dir() && de.file_name().to_string_lossy().to_lowercase() == stem {
                for p in srt_files(&de.path()) {
                    let fstem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    found.push(make_sub(p, &fstem));
                }
            }
        }
    }
    found
}

fn make_sub(path: PathBuf, hint: &str) -> ExternalSub {
    let language = guess_language(hint);
    let title = if hint.is_empty() {
        "External".to_string()
    } else {
        hint.to_string()
    };
    ExternalSub { path, language, title }
}

/// Map a hint like "en", "eng.forced" or "2_English" to an ISO 639-2 code.
fn guess_language(hint: &str) -> Option<String> {
    const LANGS: &[(&str, &[&str])] = &[
        ("eng", &["en", "eng", "english"]),
        ("dan", &["da", "dan", "danish", "dansk"]),
        ("swe", &["sv", "swe", "swedish", "svenska"]),
        ("nor", &["no", "nor", "nb", "norwegian", "norsk"]),
        ("ger", &["de", "ger", "deu", "german", "deutsch"]),
        ("fre", &["fr", "fre", "fra", "french"]),
        ("spa", &["es", "spa", "spanish"]),
        ("ita", &["it", "ita", "italian"]),
        ("dut", &["nl", "dut", "nld", "dutch"]),
        ("fin", &["fi", "fin", "finnish"]),
    ];
    let lower = hint.to_lowercase();
    for token in lower.split(|c: char| !c.is_ascii_alphabetic()) {
        for (code, names) in LANGS {
            if names.contains(&token) {
                return Some(code.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nasjelly-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ids_are_32_hex() {
        let id = path_id(Path::new("/share/Downloads/a.mkv"));
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn lists_only_dirs_and_videos_and_resolves_ids() {
        let d = tmpdir("list");
        fs::create_dir(d.join("Show")).unwrap();
        fs::write(d.join("Show/ep1.mkv"), b"x").unwrap();
        fs::write(d.join("movie.MKV"), b"x").unwrap();
        fs::write(d.join("movie.nfo"), b"x").unwrap();
        fs::write(d.join(".hidden.mkv"), b"x").unwrap();
        let lib = Library::new(&[Folder { name: "D".into(), path: d.clone() }]);
        let mut names: Vec<String> = lib.list_dir(&d).unwrap().into_iter().map(|e| display_name(&e.path, true)).collect();
        names.sort();
        assert_eq!(names, vec!["Show", "movie.MKV"]);

        // a fresh library (as after restart) finds nested ids by walking
        let lib2 = Library::new(&[Folder { name: "D".into(), path: d.clone() }]);
        let ep = d.join("Show/ep1.mkv");
        assert_eq!(lib2.resolve(&path_id(&ep)), Some(ep));
        assert_eq!(lib2.resolve(&lib2.roots[0].id), Some(d.clone()));
        assert_eq!(lib2.resolve("00000000000000000000000000000000"), None);
    }

    #[test]
    fn finds_external_subs() {
        let d = tmpdir("subs");
        fs::write(d.join("Movie.2020.mkv"), b"x").unwrap();
        fs::write(d.join("Movie.2020.srt"), b"x").unwrap();
        fs::write(d.join("Movie.2020.da.srt"), b"x").unwrap();
        fs::write(d.join("Other.srt"), b"x").unwrap();
        fs::create_dir(d.join("Subs")).unwrap();
        fs::write(d.join("Subs/2_English.srt"), b"x").unwrap();
        let subs = external_subs(&d.join("Movie.2020.mkv"));
        let langs: Vec<Option<String>> = subs.iter().map(|s| s.language.clone()).collect();
        assert_eq!(subs.len(), 3);
        assert_eq!(langs, vec![Some("dan".into()), None, Some("eng".into())]);
    }

    #[test]
    fn display_name_strips_extension_for_files() {
        assert_eq!(display_name(Path::new("/a/b.mkv"), false), "b");
        assert_eq!(display_name(Path::new("/a/b.c"), true), "b.c");
    }
}
