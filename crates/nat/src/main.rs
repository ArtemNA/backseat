//! natcheck: prints what kind of NAT this machine is behind. Addresses are masked unless
//! --show-addresses is given, so the output can be shared.

fn mask(a: std::net::SocketAddr, show: bool) -> String {
    if show {
        return a.to_string();
    }
    match a.ip() {
        std::net::IpAddr::V4(v) => {
            let o = v.octets();
            format!("{}.{}.x.x:{}", o[0], o[1], a.port())
        }
        std::net::IpAddr::V6(_) => format!("[v6]:{}", a.port()),
    }
}

fn main() {
    let show = std::env::args().any(|a| a == "--show-addresses");
    let servers = nat::resolve(nat::DEFAULT_SERVERS);
    if servers.len() < 2 {
        eprintln!("natcheck: could not resolve at least two STUN servers (DNS or no internet)");
        std::process::exit(1);
    }
    let r = match nat::classify(&servers) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("natcheck: {e}");
            std::process::exit(1);
        }
    };
    println!("local socket      {}", mask(r.local, show));
    for (s, a) in &r.answers {
        println!("server {:<22} sees {}", s.to_string(), a.map(|a| mask(a, show)).unwrap_or_else(|| "no answer".into()));
    }
    let text = match r.verdict {
        nat::Verdict::UdpBlocked => "UDP blocked or no internet: no direct connection possible".to_string(),
        nat::Verdict::NoNat => "no NAT: public address".to_string(),
        nat::Verdict::Easy => "easy NAT (same mapping for every server): direct connections usually work".to_string(),
        nat::Verdict::Hard { port_step: Some(s) } => {
            format!("hard NAT, predictable ports (step {s}): direct connection may need port prediction")
        }
        nat::Verdict::Hard { port_step: None } => {
            "hard NAT, random ports: direct connection fails against another hard NAT".to_string()
        }
    };
    println!("verdict           {text}");
}
