//! Answers the Roku app's UDP broadcast "Who is JellyfinServer?" on port 7359.

use serde_json::json;
use std::net::{IpAddr, SocketAddr, UdpSocket as StdUdpSocket};
use tokio::net::UdpSocket;

/// Our LAN address as seen from `peer`: connect a throwaway UDP socket and read it back.
fn local_ip_towards(peer: SocketAddr) -> Option<IpAddr> {
    let s = StdUdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(peer).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

pub async fn run(port: u16, server_id: String, server_name: String) {
    let sock = match UdpSocket::bind("0.0.0.0:7359").await {
        Ok(s) => s,
        Err(e) => {
            println!("discovery disabled, cannot bind udp 7359: {e}");
            return;
        }
    };
    let mut buf = [0u8; 1024];
    loop {
        let Ok((n, peer)) = sock.recv_from(&mut buf).await else { continue };
        let msg = String::from_utf8_lossy(&buf[..n]).to_lowercase();
        if !msg.contains("who is jellyfinserver") {
            continue;
        }
        let Some(ip) = local_ip_towards(peer) else { continue };
        let reply = json!({
            "Address": format!("http://{ip}:{port}"),
            "Id": server_id,
            "Name": server_name,
            "EndpointAddress": null,
        });
        let _ = sock.send_to(reply.to_string().as_bytes(), peer).await;
        println!("discovery: answered {peer}");
    }
}
