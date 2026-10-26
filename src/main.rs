mod api;
mod config;
mod discovery;
mod hls;
mod library;
mod mkv;
mod playback;
mod probe;
mod stream;
mod vtt;

use axum::extract::Request;
use axum::http::Uri;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

/// Jellyfin clients mix path casing (`/Items`, `/items`) and sometimes add a trailing
/// slash; routes are registered lowercase, so normalise before routing.
fn normalize_path(mut req: Request) -> Request {
    let uri = req.uri();
    let mut path = uri.path().to_ascii_lowercase();
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    let pq = match uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    if let Ok(new) = pq.parse::<Uri>() {
        *req.uri_mut() = new;
    }
    req
}

#[tokio::main]
async fn main() {
    let cfg_path = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("nasjelly.toml"));
    let cfg = match config::Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    for f in &cfg.folders {
        if !f.path.is_dir() {
            eprintln!("warning: folder {} ({}) does not exist", f.name, f.path.display());
        }
    }

    for tool in [&cfg.ffmpeg, &cfg.ffprobe] {
        let ok = std::process::Command::new(tool)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("warning: cannot run {tool:?}; set its full path in the config (playback will fail)");
        }
    }

    let port = cfg.port;
    let state = Arc::new(api::AppState::new(cfg));
    tokio::spawn(discovery::run(port, state.server_id.clone(), state.cfg.server_name.clone()));

    let app = api::router(state).map_request(normalize_path);
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on port {port}: {e}");
            std::process::exit(1);
        }
    };
    println!("nasjelly listening on http://0.0.0.0:{port}");
    axum::serve(listener, axum::ServiceExt::<Request>::into_make_service(app))
        .await
        .unwrap();
}
