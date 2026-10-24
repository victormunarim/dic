//! Chamada de voz de teste entre duas máquinas (sem placa de som: cada lado envia um tom).
//! Roteiro completo no README, seção "Testar uma chamada entre Linux e Windows".
//!
//! ```text
//! call keygen <arquivo>                                   # cria (ou lê) a identidade e imprime a chave pública
//! call phone <escuta> <arquivo> <pub-do-chamador>...      # atende; só os contatos listados tocam
//! call dial <arquivo> <pub-do-chamado> <relayA> <relayB> [--seconds N]
//! ```
//! Os relays são os do `measure relay` (linha `addr,onion,link`) com `contato:9=<host:porta do phone>`.

use std::time::Duration;

use clam::circuit::manager::{Candidate, CircuitManager, ManagerConfig};
use clam::media::codec::Mulaw;
use clam::phone::{ActiveCall, Phone, call};
use clam::transport::{CircuitClient, ClientOptions};
use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};
use tokio::time::{Instant, MissedTickBehavior, interval};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<[u8; 32], String> {
    let s = s.trim();
    if s.len() != 64 {
        return Err(format!("chave deve ter 64 hex, veio {}", s.len()));
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Lê a identidade de `path`; se o arquivo não existir, cria uma nova.
fn identity(path: &str) -> Result<StaticSecret, Box<dyn std::error::Error>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(StaticSecret::from(unhex(&s)?)),
        Err(_) => {
            let k = StaticSecret::random_from_rng(OsRng);
            std::fs::write(path, hex(&k.to_bytes()))?;
            eprintln!("identidade nova gravada em {path} (guarde; é a chave secreta)");
            Ok(k)
        }
    }
}

fn parse_candidate(spec: &str) -> Result<Candidate, String> {
    let mut p = spec.split(',');
    let (Some(addr), Some(onion), Some(link)) = (p.next(), p.next(), p.next()) else {
        return Err(format!("relay '{spec}': esperado addr,onion,link"));
    };
    Ok(Candidate { addr: addr.into(), public: unhex(onion)?.into(), link_public: unhex(link)?, score: 1.0 })
}

fn tone(hz: f64, n: usize) -> Vec<i16> {
    (0..160).map(|i| (6000.0 * (std::f64::consts::TAU * hz * (n * 160 + i) as f64 / 8000.0).sin()) as i16).collect()
}

/// Envia um tom a cada 20 ms e imprime, por segundo, o que chegou do outro lado.
/// `secs = None`: vai até o outro lado desligar.
async fn talk(mut c: ActiveCall, hz: f64, secs: Option<u64>) {
    let mut tick = interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut report = interval(Duration::from_secs(1));
    let end = secs.map(|s| Instant::now() + Duration::from_secs(s));
    let (mut n, mut frames, mut energy) = (0usize, 0u64, 0f64);
    let mut prev = c.stats();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if c.send(tone(hz, n)).await.is_err() { break; }
                n += 1;
                if end.is_some_and(|e| Instant::now() >= e) { break; }
            }
            f = c.next_pcm() => match f {
                Some(f) => {
                    frames += 1;
                    energy += f.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / f.len() as f64;
                }
                None => break,
            },
            _ = report.tick() => {
                let s = c.stats();
                let rms = if frames > 0 { (energy / frames as f64).sqrt() } else { 0.0 };
                println!(
                    "pacotes recebidos +{:<3} (total {})  rejeitados {}  escondidos +{}  atraso {} ms  rms {:.0}",
                    s.received - prev.received, s.received, s.rejected, s.concealed - prev.concealed, s.delay_ms, rms
                );
                prev = s;
                (frames, energy) = (0, 0.0);
            }
        }
    }
    let s = c.stats();
    println!("chamada encerrada: enviados {}, recebidos {}, escondidos {}, atrasados {}", s.sent, s.received, s.concealed, s.late);
    c.hangup();
    tokio::time::sleep(Duration::from_millis(500)).await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("keygen") => {
            let k = identity(args.get(1).ok_or("falta <arquivo>")?)?;
            println!("{}", hex(PublicKey::from(&k).as_bytes()));
        }
        Some("phone") => {
            let listen = args.get(1).ok_or("falta <escuta>")?;
            let me = identity(args.get(2).ok_or("falta <arquivo>")?)?;
            let contacts = args[3..].iter().map(|s| unhex(s)).collect::<Result<Vec<_>, _>>()?;
            if contacts.is_empty() {
                return Err("liste ao menos uma <pub-do-chamador>: sem contatos ninguém toca".into());
            }
            let mut phone = Phone::bind(listen.parse()?, me, contacts).await?;
            eprintln!("telefone ouvindo em {}; Ctrl-C encerra", phone.local_addr());
            while let Some(inc) = phone.next_incoming().await {
                println!("chamada de {}", &hex(inc.caller().as_bytes())[..16]);
                match inc.accept(Mulaw::new()).await {
                    Ok(c) => {
                        tokio::spawn(talk(c, 660.0, None));
                    }
                    Err(e) => eprintln!("não atendeu: {e}"),
                }
            }
        }
        Some("dial") => {
            let me = identity(args.get(1).ok_or("falta <arquivo>")?)?;
            let peer = PublicKey::from(unhex(args.get(2).ok_or("falta <pub-do-chamado>")?)?);
            let a = parse_candidate(args.get(3).ok_or("falta <relayA>")?)?;
            let b = parse_candidate(args.get(4).ok_or("falta <relayB>")?)?;
            let secs = args.iter().position(|a| a == "--seconds").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(15);
            let mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", vec![a, b]);
            let client = CircuitClient::spawn_with(mgr, Duration::from_secs(3), ClientOptions::default());
            println!("ligando...");
            let c = call(client, &me, &peer, Mulaw::new(), Duration::from_secs(15)).await?;
            println!("atendida; {secs} s de tom de 440 Hz");
            talk(c, 440.0, Some(secs)).await;
        }
        _ => eprintln!("uso: call keygen|phone|dial (veja o cabeçalho do arquivo)"),
    }
    Ok(())
}
