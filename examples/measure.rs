//! Medição de latência do circuito de 2 saltos entre máquinas reais.
//! Roteiro completo em `docs/CIRCUITO.md`, seção "Medir entre máquinas reais".
//!
//! ```text
//! measure contact <escuta>                       # destino que ecoa (máquina C)
//! measure relay <escuta> <endereco-publico> [nome=host:porta]... [--no-cover]   # relay (máquinas A e B)
//! measure probe <relayA> <relayB> [--seconds N] [--interval-ms N] [--no-cover] [--no-circuit-cover] [--profile off|economy|standard|strong]   # cliente
//! measure direct <host:porta> [--seconds N] [--interval-ms N]      # linha de base sem circuito
//! ```
//! `relayX` é a linha `addr,onion,link` que o comando `relay` imprime.

use std::{collections::HashMap, time::Duration};

use clam::circuit::{
    manager::{Candidate, CircuitManager, Event, ManagerConfig},
    relay::{RelayConfig, RelayNode},
};
use clam::cover::CoverProfile;
use clam::transport::{CircuitClient, ClientOptions, RelayServer, ServerConfig, allow_list};
use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{Instant, interval, sleep_until, timeout},
};

/// Nome lógico do destino, o mesmo que a política de saída dos relays mapeia.
const DEST: &str = "contato:9";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 {
        return Err(format!("chave deve ter 64 hex, veio {}", s.len()));
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn parse_candidate(spec: &str) -> Result<Candidate, String> {
    let mut p = spec.split(',');
    let (addr, onion, link) = (p.next(), p.next(), p.next());
    let (Some(addr), Some(onion), Some(link)) = (addr, onion, link) else {
        return Err(format!("relay '{spec}': esperado addr,onion,link"));
    };
    Ok(Candidate { addr: addr.into(), public: unhex(onion)?, link_public: unhex(link)?, score: 1.0 })
}

fn opt<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Destino: devolve cada quadro `[u16][dados]` com o mesmo conteúdo.
async fn contact(listen: &str) -> std::io::Result<()> {
    let l = TcpListener::bind(listen).await?;
    eprintln!("destino ouvindo em {}", l.local_addr()?);
    loop {
        let (mut s, from) = l.accept().await?;
        eprintln!("conexão de {from}");
        tokio::spawn(async move {
            let mut len = [0u8; 2];
            while s.read_exact(&mut len).await.is_ok() {
                let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
                if s.read_exact(&mut buf).await.is_err() {
                    return;
                }
                let mut f = len.to_vec();
                f.extend_from_slice(&buf);
                if s.write_all(&f).await.is_err() {
                    return;
                }
            }
        });
    }
}

async fn relay(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let listen = args.first().ok_or("falta <escuta>")?;
    let public = args.get(1).ok_or("falta <endereco-publico>")?;
    let exits = args[2..]
        .iter()
        .filter(|e| *e != "--no-cover")
        .filter_map(|e| e.split_once('=').map(|(n, a)| (n.to_string(), a.to_string())))
        .collect::<Vec<_>>();
    let node = RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0);
    let onion = node.public_key();
    let mut cfg = ServerConfig::new(listen.parse()?);
    cfg.exit = allow_list(exits);
    if args.iter().any(|a| a == "--no-cover") {
        cfg.cover = None;
    }
    let srv = RelayServer::spawn(node, cfg).await?;
    println!("{public},{},{}", hex(&onion), hex(&srv.link_public_key()));
    eprintln!("relay ouvindo em {}; copie a linha acima para o probe. Ctrl-C encerra.", srv.local_addr());
    std::future::pending::<()>().await;
    Ok(())
}

#[derive(Default)]
struct Samples(Vec<f64>);

impl Samples {
    fn report(&mut self, label: &str, sent: usize) {
        let got = self.0.len();
        println!("{label}: enviados {sent}, recebidos {got}, perda {:.2}%", 100.0 * (sent - got.min(sent)) as f64 / sent.max(1) as f64);
        if self.0.is_empty() {
            return;
        }
        self.0.sort_by(f64::total_cmp);
        let q = |p: f64| self.0[((self.0.len() - 1) as f64 * p).round() as usize];
        let mean = self.0.iter().sum::<f64>() / got as f64;
        let jitter = self.0.iter().map(|x| (x - mean).abs()).sum::<f64>() / got as f64;
        println!(
            "  RTT ms  min {:.1}  p50 {:.1}  p95 {:.1}  p99 {:.1}  max {:.1}  média {:.1}  jitter {:.1}",
            self.0[0], q(0.5), q(0.95), q(0.99), self.0[got - 1], mean, jitter
        );
    }
}

async fn probe(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let a = parse_candidate(args.first().ok_or("falta <relayA>")?)?;
    let b = parse_candidate(args.get(1).ok_or("falta <relayB>")?)?;
    let secs: u64 = opt(args, "--seconds", 60);
    let every: u64 = opt(args, "--interval-ms", 20);
    let cover = if args.iter().any(|x| x == "--no-cover") { None } else { ClientOptions::default().cover };
    let circuit_cover = if args.iter().any(|x| x == "--no-circuit-cover") { None } else { ClientOptions::default().circuit_cover };

    let profile: Option<CoverProfile> = match args.iter().position(|x| x == "--profile").and_then(|i| args.get(i + 1)) {
        Some(p) => Some(p.parse()?),
        None => None,
    };
    let mgr = CircuitManager::new(ManagerConfig::default(), DEST, vec![a, b]);
    let mut c = CircuitClient::spawn_with(mgr, Duration::from_secs(3), ClientOptions { cover, circuit_cover, profile, ..Default::default() });
    let t0 = Instant::now();
    let mut ready = None;
    let mut path = None;
    while ready.is_none() {
        match timeout(Duration::from_secs(15), c.next_event()).await {
            Ok(Some(Event::Rtt { first, second, .. })) => path = Some((first, second)),
            Ok(Some(Event::Ready { rtt_ms })) => ready = Some((rtt_ms, t0.elapsed())),
            Ok(Some(Event::CircuitFailed { first, second, reason })) => eprintln!("circuito falhou {first} -> {second}: {reason:?}"),
            Ok(Some(_)) => {}
            _ => return Err("circuito não ficou pronto em 15 s (firewall? chaves? exit?)".into()),
        }
    }
    let (rtt0, build) = ready.unwrap();
    println!("circuito pronto em {} ms (RTT de verificação {rtt0} ms), cover de link {}, de circuito {}", build.as_millis(), if cover.is_some() { "ligado" } else { "desligado" }, if circuit_cover.is_some() { "ligado" } else { "desligado" });

    if let Some((first, second)) = path {
        // A ordem dos relays é sorteada; mostra a que valeu nesta medição.
        println!("caminho: cliente -> {first} -> {second} -> destino");
    }
    let (mut sent, mut seq, mut failovers) = (0usize, 0u64, 0usize);
    let mut at: HashMap<u64, Instant> = HashMap::new();
    let mut rtts = Samples::default();
    let mut ctrl = Samples::default();
    let mut tick = interval(Duration::from_millis(every));
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let mut p = seq.to_be_bytes().to_vec();
                p.extend_from_slice(&[0u8; 120]);
                at.insert(seq, Instant::now());
                if c.send(&p).await.is_ok() { sent += 1; }
                seq += 1;
            }
            ev = c.next_event() => match ev {
                Some(Event::Data(d)) if d.len() >= 8 => {
                    let n = u64::from_be_bytes(d[..8].try_into().unwrap());
                    if let Some(t) = at.remove(&n) { rtts.0.push(t.elapsed().as_secs_f64() * 1e3); }
                }
                Some(Event::Rtt { rtt_ms, .. }) => {
                    ctrl.0.push(rtt_ms as f64);
                }
                Some(Event::FailedOver { silence_ms }) => { failovers += 1; println!("failover (silêncio {silence_ms} ms)"); }
                Some(_) => {}
                None => break,
            },
            _ = sleep_until(end) => break,
        }
    }
    rtts.report("voz (quadro 128 B, ida e volta pelo circuito)", sent);
    ctrl.report("pings de controle do gerente", ctrl.0.len());
    println!("failovers: {failovers}");
    c.stop();
    Ok(())
}

/// Linha de base: mesmo eco por TCP direto, sem relays.
async fn direct(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let addr = args.first().ok_or("falta <host:porta>")?;
    let secs: u64 = opt(args, "--seconds", 60);
    let every: u64 = opt(args, "--interval-ms", 20);
    let s = TcpStream::connect(addr).await?;
    s.set_nodelay(true)?;
    let (mut rd, mut wr) = s.into_split();
    let mut sent_at: Vec<Instant> = vec![];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut len = [0u8; 2];
        while rd.read_exact(&mut len).await.is_ok() {
            let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
            if rd.read_exact(&mut buf).await.is_err() || tx.send(u64::from_be_bytes(buf[..8].try_into().unwrap())).is_err() {
                return;
            }
        }
    });
    let mut rtts = Samples::default();
    let mut tick = interval(Duration::from_millis(every));
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let mut f = 128u16.to_be_bytes().to_vec();
                f.extend_from_slice(&(sent_at.len() as u64).to_be_bytes());
                f.extend_from_slice(&[0u8; 120]);
                sent_at.push(Instant::now());
                wr.write_all(&f).await?;
            }
            Some(n) = rx.recv() => rtts.0.push(sent_at[n as usize].elapsed().as_secs_f64() * 1e3),
            _ = sleep_until(end) => break,
        }
    }
    rtts.report("TCP direto", sent_at.len());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("contact") => contact(args.get(1).ok_or("falta <escuta>")?).await?,
        Some("relay") => relay(&args[1..]).await?,
        Some("probe") => probe(&args[1..]).await?,
        Some("direct") => direct(&args[1..]).await?,
        _ => eprintln!("uso: measure contact|relay|probe|direct (veja o cabeçalho do arquivo)"),
    }
    Ok(())
}
