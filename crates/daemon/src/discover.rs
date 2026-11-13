//! Finding other SmoothKvm computers so pairing is "click it, type its PIN" instead of typing addresses.
//!
//! A computer answers a discovery question ONLY while its pairing window is open (and only with its name,
//! port and short device id), so it is invisible on the network the rest of the time. Questions go out as a
//! LAN broadcast and, if Tailscale is installed, directly to each online Tailscale peer (Tailscale does not
//! carry broadcasts). Nothing here grants any access: pairing still needs the single-use PIN.
use serde::Serialize;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

const QUESTION: &[u8] = b"SMOOTHKVM-DISCOVER 1";

#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct Found {
    pub name: String,
    pub addr: String,
    pub id: String,
}

/// This computer's name as people know it.
pub fn hostname() -> String {
    #[cfg(windows)]
    let h = std::env::var("COMPUTERNAME").unwrap_or_default();
    #[cfg(unix)]
    let h = {
        let mut buf = [0u8; 256];
        let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if r == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).to_string()
        } else {
            std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string()
        }
    };
    let h = crate::shared::clean(h.trim());
    if h.is_empty() {
        "This computer".into()
    } else {
        h.chars().take(40).collect()
    }
}

/// The addresses other computers can use to reach this one: the LAN address and the Tailscale address.
/// (Asking the OS which local address it would use for a route sends no packets.)
pub fn local_addrs() -> Vec<String> {
    let mut out = Vec::new();
    for probe in ["192.168.0.1:9", "10.0.0.1:9", "8.8.8.8:9", "100.100.100.100:9"] {
        if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
            if s.connect(probe).is_ok() {
                if let Ok(a) = s.local_addr() {
                    let ip = a.ip();
                    if !ip.is_loopback() && !ip.is_unspecified() && !out.contains(&ip.to_string()) {
                        out.push(ip.to_string());
                    }
                }
            }
        }
    }
    // Tailscale addresses (100.64.0.0/10) last, labelled by the caller if wanted.
    out.sort_by_key(|a| a.starts_with("100."));
    out
}

pub fn is_tailscale(ip: &str) -> bool {
    match ip.parse::<Ipv4Addr>() {
        Ok(v4) => v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]),
        Err(_) => false,
    }
}

/// Answer discovery questions on UDP `port` while pairing is open. Runs for the life of the receiver.
pub fn start_responder(port: u16, my_id: String) {
    std::thread::spawn(move || {
        let sock = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(s) => s,
            Err(e) => {
                crate::shared::warn(&format!("Other computers cannot find this one automatically (UDP port {port}: {e}); pair by address instead"));
                return;
            }
        };
        let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = [0u8; 64];
        let mut last: Option<(IpAddr, Instant)> = None;
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
            if &buf[..n] != QUESTION || crate::shared::pairing_remaining() == 0 {
                continue;
            }
            if !crate::RECEIVE_ENABLED.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }
            // at most one answer per second to the same address: no use as a traffic amplifier
            if let Some((ip, t)) = last {
                if ip == from.ip() && t.elapsed() < Duration::from_secs(1) {
                    continue;
                }
            }
            last = Some((from.ip(), Instant::now()));
            let reply = serde_json::json!({"skvm": 1, "name": hostname(), "port": port, "id": my_id}).to_string();
            let _ = sock.send_to(reply.as_bytes(), from);
        }
    });
}

/// Online Tailscale peers' IPv4 addresses (empty if Tailscale isn't installed).
fn tailscale_peers() -> Vec<Ipv4Addr> {
    #[cfg(windows)]
    let exe = r"C:\Program Files\Tailscale\tailscale.exe";
    #[cfg(unix)]
    let exe = "tailscale";
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["status", "--json"]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let Ok(out) = cmd.output() else { return vec![] };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return vec![] };
    let mut ips = Vec::new();
    if let Some(peers) = v["Peer"].as_object() {
        for p in peers.values().take(128) {
            if p["Online"].as_bool() == Some(false) {
                continue;
            }
            if let Some(ip) = p["TailscaleIPs"].as_array().and_then(|a| a.iter().filter_map(|x| x.as_str()?.parse::<Ipv4Addr>().ok()).next()) {
                ips.push(ip);
            }
        }
    }
    ips
}

/// Ask the network which SmoothKvm computers are open for pairing right now (takes about 1.5 seconds).
pub fn scan(port: u16, my_id: &str) -> Vec<Found> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return vec![] };
    let _ = sock.set_broadcast(true);
    let mut ports = vec![port];
    if port != 24800 {
        ports.push(24800);
    }
    let mut targets: Vec<SocketAddr> = Vec::new();
    for p in &ports {
        targets.push(SocketAddr::from((Ipv4Addr::BROADCAST, *p)));
        // the directed broadcast of each LAN we're on (some systems only send 255.255.255.255 out of one adapter)
        for a in local_addrs() {
            if let Ok(v4) = a.parse::<Ipv4Addr>() {
                if !is_tailscale(&a) {
                    let o = v4.octets();
                    targets.push(SocketAddr::from((Ipv4Addr::new(o[0], o[1], o[2], 255), *p)));
                }
            }
        }
        for ip in tailscale_peers() {
            targets.push(SocketAddr::from((ip, *p)));
        }
        #[cfg(test)]
        targets.push(SocketAddr::from((Ipv4Addr::LOCALHOST, *p)));
    }
    let _ = sock.set_read_timeout(Some(Duration::from_millis(150)));
    let mut found: Vec<Found> = Vec::new();
    let mut seen = HashSet::new();
    let start = Instant::now();
    let mut sent_round = 0;
    while start.elapsed() < Duration::from_millis(1500) {
        // ask twice (UDP can drop a packet)
        if sent_round < 2 && start.elapsed() >= Duration::from_millis(600 * sent_round) {
            for t in &targets {
                let _ = sock.send_to(QUESTION, t);
            }
            sent_round += 1;
        }
        let mut buf = [0u8; 512];
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) else { continue };
        if v["skvm"] != 1 {
            continue;
        }
        let id = crate::shared::clean(v["id"].as_str().unwrap_or("")).chars().take(32).collect::<String>();
        if id.is_empty() || id == my_id || !seen.insert(id.clone()) {
            continue;
        }
        let port = v["port"].as_u64().filter(|p| (1..=65535).contains(p)).unwrap_or(24800) as u16;
        let name = crate::shared::clean(v["name"].as_str().unwrap_or("")).chars().take(40).collect::<String>();
        found.push(Found { name: if name.is_empty() { from.ip().to_string() } else { name }, addr: SocketAddr::new(from.ip(), port).to_string(), id });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_range() {
        assert!(is_tailscale("100.83.46.95"));
        assert!(!is_tailscale("100.20.1.1"));
        assert!(!is_tailscale("192.168.1.5"));
    }

    #[test]
    fn finds_a_computer_only_while_pairing_is_open() {
        let port = 34871;
        start_responder(port, "aaaa-bbbb".into());
        std::thread::sleep(Duration::from_millis(200));
        crate::shared::burn_pairing();
        assert!(scan(port, "me").iter().all(|f| f.id != "aaaa-bbbb"), "must stay silent while pairing is closed");
        crate::shared::open_pairing(30);
        let f = scan(port, "me");
        crate::shared::burn_pairing();
        let hit = f.iter().find(|f| f.id == "aaaa-bbbb").expect("should be found while pairing is open");
        assert!(hit.addr.ends_with(":34871"));
    }
}
