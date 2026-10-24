#![cfg(feature = "net")]
//! Chamada completa: chamador -> 2 relays reais -> `Phone` (destino), tudo em 127.0.0.1.

use std::time::Duration;

use clam::circuit::{
    manager::{Candidate, CircuitManager, ManagerConfig},
    relay::{RelayConfig, RelayNode},
};
use clam::media::{
    VoiceCall,
    codec::{Codec, Mulaw},
    signal::{Answer, Caller},
};
use clam::phone::{ActiveCall, CallError, Phone, call};
use clam::transport::{CircuitClient, RelayServer, ServerConfig, allow_list};
use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, sleep, sleep_until, timeout},
};

const WAIT: Duration = Duration::from_secs(5);

fn tone(n: usize) -> Vec<i16> {
    (0..160).map(|i| (9000.0 * (std::f64::consts::TAU * 440.0 * (n * 160 + i) as f64 / 8000.0).sin()) as i16).collect()
}

fn ids() -> (StaticSecret, StaticSecret) {
    (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng))
}

async fn relays(n: usize, dest: &str) -> (Vec<RelayServer>, Vec<Candidate>) {
    let (mut servers, mut cands) = (vec![], vec![]);
    for _ in 0..n {
        let node = RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0);
        let public = node.public_key();
        let mut cfg = ServerConfig::new("127.0.0.1:0".parse().unwrap());
        cfg.exit = allow_list(vec![("contato:9".into(), dest.into())]);
        let srv = RelayServer::spawn(node, cfg).await.unwrap();
        cands.push(Candidate { addr: srv.local_addr().to_string(), public, link_public: srv.link_public_key(), score: 1.0 });
        servers.push(srv);
    }
    (servers, cands)
}

fn client(cands: Vec<Candidate>) -> CircuitClient {
    CircuitClient::spawn(CircuitManager::new(ManagerConfig::default(), "contato:9", cands), Duration::from_secs(2))
}

/// Manda `n` quadros e devolve o que o outro lado tocar nesse tempo (mais 300 ms). Sem DTX, o
/// alto-falante recebe quadros escondidos sem parar, então a coleta é por tempo, não até secar.
async fn talk(tx: &ActiveCall, rx: &mut ActiveCall, n: usize) -> Vec<Vec<i16>> {
    let mut got = vec![];
    let end = Instant::now() + Duration::from_millis(20 * n as u64 + 300);
    let mut sent = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    while Instant::now() < end {
        tokio::select! {
            _ = tick.tick(), if sent < n => { tx.send(tone(sent)).await.unwrap(); sent += 1; }
            f = rx.next_pcm() => match f { Some(f) => got.push(f), None => break },
            _ = sleep_until(end) => break,
        }
    }
    got
}

#[tokio::test]
async fn call_over_two_hops_carries_audio_both_ways_and_hangs_up() {
    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(4, &phone.local_addr().to_string()).await;

    let bob_pub = PublicKey::from(&bob);
    let caller = tokio::spawn(async move { call(client(cands), &alice, &bob_pub, Mulaw::new(), WAIT).await });
    let incoming = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap();
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let mut a = caller.await.unwrap().unwrap();

    // Ida (a -> b) e volta (b -> a): o áudio que sai é o que o codec devolve, sem alteração.
    let mut reference = Mulaw::new();
    let want: Vec<Vec<i16>> = (0..30)
        .map(|i| {
            let e = reference.encode(&tone(i)).unwrap();
            reference.decode(Some(&e))
        })
        .collect();
    let got = talk(&a, &mut b, 30).await;
    let ok = got.iter().filter(|f| want.contains(f)).count();
    // Margem para o jitter do cover (até 20 ms por sentido) com o atraso inicial de 40 ms.
    assert!(ok >= 20, "ida: {ok} de 30");
    let back = talk(&b, &mut a, 30).await;
    let ok = back.iter().filter(|f| want.contains(f)).count();
    assert!(ok >= 20, "volta: {ok} de 30 (recebidos {}, tocados {})", a.stats().received, back.len());
    assert!(b.stats().received >= 28 && a.stats().received >= 28);

    a.hangup();
    assert!(timeout(WAIT, async { while b.next_pcm().await.is_some() {} }).await.is_ok(), "b deveria ver o desligar");
}

#[tokio::test]
async fn rejected_call_reports_rejection() {
    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(2, &phone.local_addr().to_string()).await;
    let bob_pub = PublicKey::from(&bob);
    let caller = tokio::spawn(async move { call(client(cands), &alice, &bob_pub, Mulaw::new(), WAIT).await.err() });
    timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap().reject().await;
    assert_eq!(caller.await.unwrap(), Some(CallError::Rejected));
}

#[tokio::test]
async fn unknown_caller_never_rings_and_gets_no_answer() {
    let (stranger, bob) = ids();
    let friend = StaticSecret::random_from_rng(OsRng);
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&friend).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(2, &phone.local_addr().to_string()).await;
    let bob_pub = PublicKey::from(&bob);
    let err = call(client(cands), &stranger, &bob_pub, Mulaw::new(), Duration::from_secs(2)).await.err();
    assert_eq!(err, Some(CallError::NoAnswer));
    assert!(timeout(Duration::from_millis(200), phone.next_incoming()).await.is_err());
}

async fn send_frame(s: &mut TcpStream, data: &[u8]) {
    let mut f = (data.len() as u16).to_be_bytes().to_vec();
    f.extend_from_slice(data);
    s.write_all(&f).await.unwrap();
}

async fn read_frame(s: &mut TcpStream) -> Vec<u8> {
    let mut len = [0u8; 2];
    timeout(WAIT, s.read_exact(&mut len)).await.unwrap().unwrap();
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    s.read_exact(&mut buf).await.unwrap();
    buf
}

/// Failover: a voz passa a vir por outra conexão, sem convite; o `Phone` segue a chamada
/// e responde pela conexão nova. Pacote repetido não troca a rota.
#[tokio::test]
async fn call_follows_the_voice_to_a_new_connection() {
    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let addr = phone.local_addr();

    let mut c1 = TcpStream::connect(addr).await.unwrap();
    let (caller, invite) = Caller::invite(&alice, &PublicKey::from(&bob), unix()).unwrap();
    send_frame(&mut c1, &invite).await;
    let b = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap().accept(Mulaw::new()).await.unwrap();
    let Answer::Accepted(keys) = caller.answer(&alice, &read_frame(&mut c1).await).unwrap() else { panic!() };
    let mut voice = VoiceCall::new(&keys, Mulaw::new());

    let first = voice.send(&tone(0)).unwrap();
    send_frame(&mut c1, &first).await;
    sleep(Duration::from_millis(100)).await;
    b.send(tone(1)).await.unwrap();
    assert_eq!(read_frame(&mut c1).await.len(), 200);

    // Segunda conexão: manda voz nova direto, sem convite.
    let mut c2 = TcpStream::connect(addr).await.unwrap();
    send_frame(&mut c2, &voice.send(&tone(2)).unwrap()).await;
    sleep(Duration::from_millis(100)).await;
    // Repetição do primeiro pacote por c1 não volta a rota.
    send_frame(&mut c1, &first).await;
    sleep(Duration::from_millis(100)).await;
    b.send(tone(3)).await.unwrap();
    assert_eq!(read_frame(&mut c2).await.len(), 200);
    assert!(b.stats().received >= 2);
}

fn unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// Derruba o primeiro relay do circuito ativo no meio da chamada: o reserva assume, o `Phone`
/// segue a voz para a conexão nova e o áudio volta nos dois sentidos.
#[tokio::test]
async fn call_survives_killing_the_active_relay() {
    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (mut servers, cands) = relays(6, &phone.local_addr().to_string()).await;

    let mut c = client(cands.clone());
    timeout(WAIT, async {
        while !matches!(c.next_event().await, Some(clam::circuit::manager::Event::Ready { .. })) {}
    })
    .await
    .unwrap();
    sleep(Duration::from_millis(600)).await; // reserva montar
    let active = c.status().await.unwrap().active.unwrap();
    let idx = cands.iter().position(|x| x.addr == active.first).unwrap();

    let bob_pub = PublicKey::from(&bob);
    let caller = tokio::spawn(async move { call(c, &alice, &bob_pub, Mulaw::new(), WAIT).await });
    let mut b = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap().accept(Mulaw::new()).await.unwrap();
    let mut a = caller.await.unwrap().unwrap();

    let start = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let (mut killed, mut n) = (false, 0usize);
    // Maior intervalo sem pacote novo, medido só depois da queda.
    let (mut last_a, mut last_b) = (Instant::now(), Instant::now());
    let (mut seen_a, mut seen_b) = (0, 0);
    let (mut gap_a, mut gap_b) = (Duration::ZERO, Duration::ZERO);
    let (mut base_a, mut base_b) = (0, 0);
    while start.elapsed() < Duration::from_secs(5) {
        tokio::select! {
            _ = tick.tick() => {
                if !killed && start.elapsed() > Duration::from_secs(1) {
                    servers.remove(idx).stop();
                    killed = true;
                    (base_a, base_b) = (a.stats().received, b.stats().received);
                }
                let _ = a.send(tone(n)).await;
                let _ = b.send(tone(n)).await;
                n += 1;
                let (ra, rb) = (a.stats().received, b.stats().received);
                if ra > seen_a {
                    if killed { gap_a = gap_a.max(last_a.elapsed()); }
                    (seen_a, last_a) = (ra, Instant::now());
                }
                if rb > seen_b {
                    if killed { gap_b = gap_b.max(last_b.elapsed()); }
                    (seen_b, last_b) = (rb, Instant::now());
                }
            }
            _ = a.next_pcm() => {}
            _ = b.next_pcm() => {}
        }
    }
    assert!(a.stats().received > base_a + 100, "a parou de receber: {base_a} -> {}", a.stats().received);
    assert!(b.stats().received > base_b + 100, "b parou de receber: {base_b} -> {}", b.stats().received);
    eprintln!("lacuna após a queda: a {gap_a:?}, b {gap_b:?}");
    let limit = Duration::from_millis(800);
    assert!(gap_a < limit && gap_b < limit, "silêncio de {gap_a:?} / {gap_b:?}");
}
