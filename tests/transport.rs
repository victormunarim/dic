#![cfg(feature = "net")]
//! Relays e cliente sobre TCP real em 127.0.0.1.

use std::time::Duration;

use clam::circuit::{
    manager::{Candidate, CircuitManager, Event, ManagerConfig},
    relay::{RelayConfig, RelayNode},
};
use clam::transport::{CircuitClient, RelayServer, ServerConfig, allow_list};
use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{Instant, sleep, timeout},
};

/// "Contato": eco de quadros `[u16][dados]` com prefixo `eco:`.
async fn spawn_contact() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut len = [0u8; 2];
                while s.read_exact(&mut len).await.is_ok() {
                    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
                    if s.read_exact(&mut buf).await.is_err() {
                        return;
                    }
                    let mut out = b"eco:".to_vec();
                    out.extend_from_slice(&buf);
                    let mut f = (out.len() as u16).to_be_bytes().to_vec();
                    f.extend_from_slice(&out);
                    if s.write_all(&f).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

async fn spawn_relays(n: usize, contact: &str) -> (Vec<RelayServer>, Vec<Candidate>) {
    let mut servers = vec![];
    let mut cands = vec![];
    for _ in 0..n {
        let node = RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0);
        let public = node.public_key();
        let mut cfg = ServerConfig::new("127.0.0.1:0".parse().unwrap());
        cfg.exit = allow_list(vec![("contato:9".into(), contact.into())]);
        let srv = RelayServer::spawn(node, cfg).await.unwrap();
        cands.push(Candidate { addr: srv.local_addr().to_string(), public, link_public: srv.link_public_key(), score: 1.0 });
        servers.push(srv);
    }
    (servers, cands)
}

fn client(cands: Vec<Candidate>) -> CircuitClient {
    let mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", cands);
    CircuitClient::spawn(mgr, Duration::from_secs(2))
}

/// Espera o circuito reserva ficar pronto (sem prazo fixo de montagem).
async fn wait_standby(c: &CircuitClient) {
    timeout(Duration::from_secs(10), async {
        while !c.status().await.is_some_and(|s| s.standby.is_some_and(|b| b.ready)) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("reserva não ficou pronta");
}

async fn wait_ready(c: &mut CircuitClient) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Event::Ready { .. }) = c.next_event().await {
                return;
            }
        }
    })
    .await
    .expect("circuito não ficou pronto");
}

#[tokio::test]
async fn voice_round_trip_over_tcp() {
    let contact = spawn_contact().await;
    let (_servers, cands) = spawn_relays(4, &contact).await;
    let mut c = client(cands);
    wait_ready(&mut c).await;

    c.send(b"ola").await.unwrap();
    let got = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(Event::Data(d)) = c.next_event().await {
                return d;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(got, b"eco:ola");
}

#[tokio::test]
async fn exit_policy_denies_unlisted_destinations() {
    let contact = spawn_contact().await;
    let (_servers, cands) = spawn_relays(2, &contact).await;
    // O cliente pede um destino fora da lista: nenhum eco volta.
    let mgr = CircuitManager::new(ManagerConfig::default(), "outro:1", cands);
    let mut c = CircuitClient::spawn(mgr, Duration::from_secs(2));
    wait_ready(&mut c).await;
    c.send(b"x").await.unwrap();
    let r = timeout(Duration::from_millis(500), async {
        loop {
            if let Some(Event::Data(_)) = c.next_event().await {
                return;
            }
        }
    })
    .await;
    assert!(r.is_err(), "destino fora da política não pode responder");
}

#[tokio::test]
async fn failover_after_killing_a_relay_keeps_silence_under_500ms() {
    let contact = spawn_contact().await;
    let (mut servers, cands) = spawn_relays(6, &contact).await;
    let mut c = client(cands.clone());
    wait_ready(&mut c).await;
    wait_standby(&c).await;

    let active = c.status().await.unwrap().active.unwrap();
    let idx = cands.iter().position(|x| x.addr == active.first).unwrap();

    let mut last_data = Instant::now();
    let mut max_gap = Duration::ZERO;
    let mut killed = false;
    let mut after_kill_data = 0;
    let start = Instant::now();
    let mut next_send = start;
    while start.elapsed() < Duration::from_secs(4) {
        if !killed && start.elapsed() > Duration::from_millis(800) {
            servers.remove(idx).stop();
            killed = true;
        }
        if Instant::now() >= next_send {
            let _ = c.send(b"voz").await;
            next_send += Duration::from_millis(20);
        }
        if let Ok(Some(ev)) = timeout(Duration::from_millis(5), c.next_event()).await
            && let Event::Data(_) = ev {
                max_gap = max_gap.max(last_data.elapsed());
                last_data = Instant::now();
                if killed {
                    after_kill_data += 1;
                }
            }
    }
    assert!(after_kill_data > 20, "mídia não retomou após a falha");
    assert!(max_gap < Duration::from_millis(500), "silêncio de {max_gap:?}");
}

#[tokio::test]
async fn client_refuses_relay_with_wrong_key() {
    let contact = spawn_contact().await;
    let (_servers, mut cands) = spawn_relays(2, &contact).await;
    // Descritores com chaves trocadas: o link não pode ser autenticado.
    for c in &mut cands {
        c.public = x25519_dalek_public(StaticSecret::random_from_rng(OsRng).to_bytes());
    }
    let mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", cands);
    let mut c = CircuitClient::spawn(mgr, Duration::from_secs(2));
    let r = timeout(Duration::from_secs(1), async {
        loop {
            if let Some(Event::Ready { .. }) = c.next_event().await {
                return;
            }
        }
    })
    .await;
    assert!(r.is_err(), "não pode montar circuito com chave errada");
}

fn x25519_dalek_public(secret: [u8; 32]) -> [u8; 32] {
    clam::x25519_dalek::PublicKey::from(&StaticSecret::from(secret)).to_bytes()
}

#[tokio::test]
async fn relay_drops_plaintext_cells_without_handshake() {
    let contact = spawn_contact().await;
    let (servers, _) = spawn_relays(1, &contact).await;
    let mut s = tokio::net::TcpStream::connect(servers[0].local_addr()).await.unwrap();
    s.write_all(&clam::circuit::Cell::with_body(1, clam::circuit::Cmd::Create, &[0; 33]).to_bytes())
        .await
        .unwrap();
    let mut buf = [0u8; 300];
    let n = timeout(Duration::from_secs(5), s.read(&mut buf)).await.expect("relay deve fechar");
    assert!(matches!(n, Ok(0) | Err(_)), "relay não pode responder em claro");
}

#[tokio::test]
async fn reputation_records_successes_and_drops_a_dead_relay() {
    use clam::reputation::{Reputation, ReputationConfig};
    let contact = spawn_contact().await;
    let (mut servers, cands) = spawn_relays(5, &contact).await;
    let dead = cands[0].addr.clone();
    servers.remove(0).stop();
    sleep(Duration::from_millis(100)).await;

    let cfg = ManagerConfig { build_timeout_ms: 500, ..Default::default() };
    let mgr = CircuitManager::new(cfg, "contato:9", vec![]);
    let mut c = CircuitClient::spawn_with_reputation(
        mgr,
        Duration::from_secs(1),
        Reputation::new(ReputationConfig::default()),
    );
    c.set_relays(cands.clone()).await;
    wait_ready(&mut c).await;
    wait_standby(&c).await;
    // Esvazia eventos para o ator não bloquear.
    while let Ok(Some(_)) = timeout(Duration::from_millis(5), c.next_event()).await {}

    c.send(b"ola").await.unwrap();
    let active = c.status().await.unwrap().active.unwrap();
    assert_ne!(active.first, dead);
    assert_ne!(active.second, dead);
    let s = c.relay_stats(&active.first).await.expect("sem histórico do relay ativo");
    assert!(s.good > 0.0 && !s.banned, "{s:?}");
}

#[tokio::test]
async fn reputation_snapshot_survives_a_restart() {
    use clam::reputation::{Reputation, ReputationConfig, load_file, save_file};
    let contact = spawn_contact().await;
    let (_servers, cands) = spawn_relays(4, &contact).await;
    let cfg = ReputationConfig::default();
    let mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", vec![]);
    let mut c = CircuitClient::spawn_with_reputation(mgr, Duration::from_secs(1), Reputation::new(cfg));
    c.set_relays(cands).await;
    wait_ready(&mut c).await;
    wait_standby(&c).await;
    while let Ok(Some(_)) = timeout(Duration::from_millis(5), c.next_event()).await {}

    let active = c.status().await.unwrap().active.unwrap();
    let path = std::env::temp_dir().join(format!("clam-rep-it-{}.bin", std::process::id()));
    save_file(&path, &c.reputation_snapshot().await.unwrap()).unwrap();
    c.stop();

    let mut loaded = load_file(cfg, &path, 0).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(loaded.stats(&active.first, 0).unwrap().good > 0.0);
}

#[tokio::test]
async fn link_authenticates_with_the_link_key_not_the_circuit_key() {
    let contact = spawn_contact().await;
    let (_servers, mut cands) = spawn_relays(3, &contact).await;
    for c in &mut cands {
        c.link_public = c.public; // erro: chave do circuito no lugar da do link
    }
    let mgr = CircuitManager::new(ManagerConfig { build_timeout_ms: 500, ..Default::default() }, "contato:9", cands);
    let mut c = CircuitClient::spawn(mgr, Duration::from_secs(1));
    let r = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(Event::Ready { .. }) = c.next_event().await {
                return;
            }
        }
    })
    .await;
    assert!(r.is_err(), "o link não pode abrir com a chave do circuito");
}

// ---------------------------------------------------------------------------
// Cover traffic
// ---------------------------------------------------------------------------

use clam::circuit::{Cell, Cmd};
use clam::link;
use tokio::net::TcpStream;

/// Abre um link cru até o relay (como um cliente faz) e conta os quadros que chegam em `window`.
async fn count_frames(addr: &str, key: &[u8; 32], send_padding: bool, window: Duration) -> (usize, Vec<Duration>) {
    let s = TcpStream::connect(addr).await.unwrap();
    let (mut rd, mut wr) = link::connect(s, key).await.unwrap();
    if send_padding {
        wr.write_cell(&Cell::with_body(0, Cmd::Padding, &[])).await.unwrap();
    }
    let start = Instant::now();
    let mut gaps = vec![];
    let mut last = start;
    let mut n = 0;
    while let Ok(Ok(cell)) = timeout(window.saturating_sub(start.elapsed()), rd.read_cell()).await {
        assert_eq!(cell.cmd, Cmd::Padding);
        gaps.push(last.elapsed());
        last = Instant::now();
        n += 1;
    }
    (n, gaps)
}

async fn relay_with(cover: Option<Duration>, max_cover_links: usize) -> (RelayServer, String, [u8; 32]) {
    let node = RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0);
    let mut cfg = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    cfg.cover = cover;
    cfg.max_cover_links = max_cover_links;
    let srv = RelayServer::spawn(node, cfg).await.unwrap();
    let (addr, key) = (srv.local_addr().to_string(), srv.link_public_key());
    (srv, addr, key)
}

#[tokio::test]
async fn relay_answers_peer_padding_with_constant_rate_padding() {
    let (_srv, addr, key) = relay_with(Some(Duration::from_millis(10)), 8).await;
    let (n, gaps) = count_frames(&addr, &key, true, Duration::from_millis(500)).await;
    assert!((30..=60).contains(&n), "{n} quadros em 500 ms (esperado ~50)");
    // Temporização regular: nenhum silêncio longo entre quadros.
    assert!(gaps.iter().skip(1).all(|g| *g < Duration::from_millis(60)), "{gaps:?}");
}

#[tokio::test]
async fn relay_stays_silent_without_peer_padding_or_when_disabled() {
    let (_a, addr, key) = relay_with(Some(Duration::from_millis(10)), 8).await;
    assert_eq!(count_frames(&addr, &key, false, Duration::from_millis(300)).await.0, 0);
    let (_b, addr, key) = relay_with(None, 8).await;
    assert_eq!(count_frames(&addr, &key, true, Duration::from_millis(300)).await.0, 0);
}

#[tokio::test]
async fn cover_links_are_capped() {
    let (_srv, addr, key) = relay_with(Some(Duration::from_millis(10)), 1).await;
    let (a, b) = tokio::join!(
        count_frames(&addr, &key, true, Duration::from_millis(400)),
        count_frames(&addr, &key, true, Duration::from_millis(400)),
    );
    let (hi, lo) = (a.0.max(b.0), a.0.min(b.0));
    assert!(hi > 15 && lo == 0, "com 1 vaga, um link tem padding ({hi}) e o outro não ({lo})");
}

#[tokio::test]
async fn voice_round_trip_works_with_and_without_client_cover() {
    use clam::transport::ClientOptions;
    for cover in [Some(Duration::from_millis(10)), None] {
        let contact = spawn_contact().await;
        let (_servers, cands) = spawn_relays(3, &contact).await;
        let mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", cands);
        let mut c = CircuitClient::spawn_with(mgr, Duration::from_secs(2), ClientOptions { cover, ..Default::default() });
        wait_ready(&mut c).await;
        c.send(b"ola").await.unwrap();
        let got = timeout(Duration::from_secs(2), async {
            loop {
                if let Some(Event::Data(d)) = c.next_event().await {
                    return d;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(got, b"eco:ola");
    }
}

/// Perfis: o cliente troca o cadência do padding no link já aberto, sem reconectar.
#[tokio::test]
async fn profile_switch_changes_link_cadence_live() {
    use clam::cover::CoverProfile;
    use clam::transport::ClientOptions;

    let secret = [7u8; 32];
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let cands = vec![
        // Os dois candidatos apontam para o mesmo relay falso: seja qual for o primeiro salto, há um só link.
        Candidate { addr: addr.clone(), public: x25519_dalek_public([1; 32]), link_public: x25519_dalek_public(secret), score: 1.0 },
        Candidate { addr, public: x25519_dalek_public([2; 32]), link_public: x25519_dalek_public(secret), score: 1.0 },
    ];
    let mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", cands);
    let opts = ClientOptions { profile: Some(CoverProfile::Standard), ..Default::default() };
    let c = CircuitClient::spawn_with(mgr, Duration::from_secs(2), opts);

    let (s, _) = timeout(Duration::from_secs(3), l.accept()).await.expect("cliente discou").unwrap();
    let (mut rd, _wr) = link::accept(s, &secret).await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(cell) = rd.read_cell().await {
            if cell.cmd == Cmd::Padding && tx.send(()).is_err() {
                return;
            }
        }
    });
    let count = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>| {
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    };
    sleep(Duration::from_millis(300)).await;
    count(&mut rx);

    sleep(Duration::from_secs(1)).await;
    let standard = count(&mut rx);
    c.set_profile(CoverProfile::Economy).await;
    sleep(Duration::from_millis(100)).await;
    count(&mut rx);
    sleep(Duration::from_secs(1)).await;
    let economy = count(&mut rx);
    c.set_profile(CoverProfile::Off).await;
    sleep(Duration::from_millis(100)).await;
    count(&mut rx);
    sleep(Duration::from_secs(1)).await;
    let off = count(&mut rx);
    c.set_profile(CoverProfile::Strong).await;
    sleep(Duration::from_millis(100)).await;
    count(&mut rx);
    sleep(Duration::from_secs(1)).await;
    let strong = count(&mut rx);

    assert!((85..=105).contains(&standard), "standard {standard}");
    assert!((42..=52).contains(&economy), "economy {economy}");
    assert!(off <= 2, "off {off}");
    assert!((170..=205).contains(&strong), "strong {strong}");
    c.stop();
}

/// Conta os quadros que chegam em `rd` durante `window`.
async fn count_on(rd: &mut link::LinkReader, window: Duration) -> usize {
    let (start, mut n) = (Instant::now(), 0);
    while let Ok(Ok(_)) = timeout(window.saturating_sub(start.elapsed()), rd.read_cell()).await {
        n += 1;
    }
    n
}

/// O perfil do relay muda o ritmo do que ele escreve, inclusive em links já abertos.
#[tokio::test]
async fn relay_profile_sets_and_switches_link_cadence() {
    use clam::cover::CoverProfile;

    let node = RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0);
    let mut cfg = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    cfg.profile = Some(CoverProfile::Economy);
    let srv = RelayServer::spawn(node, cfg).await.unwrap();
    let (addr, key) = (srv.local_addr().to_string(), srv.link_public_key());

    // Perfil de partida vale para links novos: 20 ms => ~50 quadros em 1 s.
    let (n, _) = count_frames(&addr, &key, true, Duration::from_secs(1)).await;
    assert!((40..=55).contains(&n), "economy {n}");

    // Link aberto antes da troca passa a seguir o novo perfil.
    let s = TcpStream::connect(&addr).await.unwrap();
    let (mut rd, mut wr) = link::connect(s, &key).await.unwrap();
    wr.write_cell(&Cell::with_body(0, Cmd::Padding, &[])).await.unwrap();
    count_on(&mut rd, Duration::from_millis(200)).await;
    srv.set_profile(CoverProfile::Strong);
    count_on(&mut rd, Duration::from_millis(100)).await;
    let strong = count_on(&mut rd, Duration::from_secs(1)).await;
    assert!((170..=205).contains(&strong), "strong {strong}");
    srv.set_profile(CoverProfile::Off);
    count_on(&mut rd, Duration::from_millis(100)).await;
    let off = count_on(&mut rd, Duration::from_millis(500)).await;
    assert!(off <= 2, "off {off}");
}
