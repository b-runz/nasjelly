use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Folder {
    pub name: String,
    pub path: PathBuf,
}

fn default_server_name() -> String {
    "NasJelly".into()
}
fn default_port() -> u16 {
    8096
}
fn default_ffmpeg() -> String {
    "ffmpeg".into()
}
fn default_ffprobe() -> String {
    "ffprobe".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_server_name")]
    pub server_name: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_ffmpeg")]
    pub ffmpeg: String,
    #[serde(default = "default_ffprobe")]
    pub ffprobe: String,
    /// Where HLS segments are written while remuxing. Defaults to the system temp dir;
    /// on the NAS point it at a disk share, /tmp there is small RAM.
    #[serde(default)]
    pub transcode_dir: Option<PathBuf>,
    pub folders: Vec<Folder>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
        if cfg.folders.is_empty() {
            return Err("config must list at least one [[folders]] entry".into());
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            username = "me"
            password = "pw"
            [[folders]]
            name = "Downloads"
            path = "/share/Downloads"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.port, 8096);
        assert_eq!(cfg.server_name, "NasJelly");
        assert_eq!(cfg.folders[0].path, PathBuf::from("/share/Downloads"));
        assert_eq!(cfg.transcode_dir, None);
    }
}
