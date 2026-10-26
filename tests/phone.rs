#![cfg(feature = "net")]
//! Chamada completa: chamador -> 2 relays reais -> `Phone` (destino), tudo em 127.0.0.1.

use std::time::Duration;

use clam::circuit::{
    manager::{Candidate, CircuitManager, ManagerConfig},
    relay::{RelayConfig, RelayNode},
};
use clam::media::{
    VoiceCall,
    codec::{Codec, CodecId, Mulaw},
    signal::{Answer, Caller},
};
use clam::phone::{ActiveCall, CallError, Phone, ScreenEvent, call, call_linked, open_screen_client_options, screen_client_options, share_screen, share_screen_slot, OPEN_SCREEN_SLOT};
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

/// Espera a condição ficar verdadeira (até `WAIT`).
async fn wait_until(mut cond: impl FnMut() -> bool) {
    timeout(WAIT, async {
        while !cond() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condição não ocorreu a tempo");
}

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

/// Voz em 16 kHz (quadros de 320 amostras), para os codecs de banda larga.
fn wide_voice(n: usize) -> Vec<i16> {
    (0..320)
        .map(|i| {
            let ph = std::f64::consts::TAU * 140.0 * (n * 320 + i) as f64 / 16000.0;
            (3000.0 * ph.sin() + 2000.0 * (2.0 * ph).sin() + 1200.0 * (3.0 * ph).sin()) as i16
        })
        .collect()
}

#[tokio::test]
async fn the_caller_chooses_the_codec_and_the_callee_follows_the_invite() {
    for id in [CodecId::Opus, CodecId::G722] {
        let (alice, bob) = ids();
        let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
        let (_srv, cands) = relays(4, &phone.local_addr().to_string()).await;

        let bob_pub = PublicKey::from(&bob);
        let caller = tokio::spawn(async move { call(client(cands), &alice, &bob_pub, id.build(), WAIT).await });
        let incoming = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap();
        let chosen = incoming.codec();
        assert_eq!(chosen, id);
        let mut b = incoming.accept(chosen.build()).await.unwrap();
        let a = caller.await.unwrap().unwrap();

        let mut got = vec![];
        let end = Instant::now() + Duration::from_millis(20 * 40 + 300);
        let (mut sent, mut tick) = (0, tokio::time::interval(Duration::from_millis(20)));
        while Instant::now() < end {
            tokio::select! {
                _ = tick.tick(), if sent < 40 => { a.send(wide_voice(sent)).await.unwrap(); sent += 1; }
                f = b.next_pcm() => match f { Some(f) => got.push(f), None => break },
                _ = sleep_until(end) => break,
            }
        }
        let loud = got.iter().filter(|f| (f.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / f.len() as f64).sqrt() > 1000.0).count();
        assert!(got.iter().all(|f| f.len() == 320), "{id:?}: quadros de 320 amostras");
        assert!(loud >= 20, "{id:?}: {loud} quadros com voz de {} tocados", got.len());
        assert!(b.stats().received >= 35, "{id:?}: recebidos {}", b.stats().received);
        a.hangup();
    }
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
    wait_until(|| b.stats().received >= 1).await;
    b.send(tone(1)).await.unwrap();
    assert_eq!(read_frame(&mut c1).await.len(), 200);

    // Segunda conexão: manda voz nova direto, sem convite.
    let mut c2 = TcpStream::connect(addr).await.unwrap();
    send_frame(&mut c2, &voice.send(&tone(2)).unwrap()).await;
    wait_until(|| b.stats().received >= 2).await;
    // Repetição do primeiro pacote por c1 não volta a rota.
    send_frame(&mut c1, &first).await;
    wait_until(|| b.stats().rejected >= 1).await;
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
    timeout(WAIT, async {
        while !c.status().await.is_some_and(|s| s.standby.is_some_and(|b| b.ready)) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("reserva não ficou pronta");
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

fn screen_client(cands: Vec<Candidate>) -> CircuitClient {
    CircuitClient::spawn_with(CircuitManager::new(ManagerConfig::default(), "contato:9", cands), Duration::from_secs(2), screen_client_options())
}

fn video(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed)).collect()
}

/// Lê eventos de tela até juntar `n` quadros (ou estourar `WAIT`).
async fn collect_frames(viewer: &mut ActiveCall, n: usize) -> Vec<clam::media::screen::ScreenFrame> {
    let mut got = vec![];
    timeout(WAIT * 2, async {
        while got.len() < n {
            match viewer.next_screen().await {
                Some(ScreenEvent::Frame(f)) => got.push(f),
                Some(ScreenEvent::Stopped | ScreenEvent::Lost(_)) => {}
                None => break,
            }
        }
    })
    .await
    .expect("quadros de tela não chegaram a tempo");
    got
}

#[tokio::test]
async fn caller_shares_its_screen_to_the_callee_over_its_own_circuit() {
    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(4, &phone.local_addr().to_string()).await;
    let bob_pub = PublicKey::from(&bob);
    let voice = cands.clone();
    let caller = tokio::spawn(async move { call(client(voice), &alice, &bob_pub, Mulaw::new(), WAIT).await });
    let incoming = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap();
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let a = caller.await.unwrap().unwrap();

    let share = share_screen(screen_client(cands), &a.next_share_keys(), WAIT).await.unwrap();
    let frames = [video(900, 1), video(120, 2), video(2500, 3), video(60, 4)];
    for (i, f) in frames.iter().enumerate() {
        assert!(share.push(f, i == 0).unwrap());
    }
    let got = collect_frames(&mut b, frames.len()).await;
    for (g, want) in got.iter().zip(&frames) {
        assert_eq!(&g.data, want);
    }
    assert!(got[0].keyframe && !got[1].keyframe);

    share.stop().await;
    let stopped = timeout(WAIT, async {
        loop {
            match b.next_screen().await {
                Some(ScreenEvent::Stopped) => return true,
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await;
    assert_eq!(stopped, Ok(true));
    // A voz continua funcionando depois.
    let back = talk(&a, &mut b, 10).await;
    assert!(!back.is_empty());
}

#[tokio::test]
async fn the_screen_without_privacy_arrives_with_no_cover_and_a_faster_slot() {
    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(4, &phone.local_addr().to_string()).await;
    let bob_pub = PublicKey::from(&bob);
    let voice = cands.clone();
    let caller = tokio::spawn(async move { call(client(voice), &alice, &bob_pub, Mulaw::new(), WAIT).await });
    let incoming = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap();
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let a = caller.await.unwrap().unwrap();

    let open = CircuitClient::spawn_with(CircuitManager::new(ManagerConfig::default(), "contato:9", cands), Duration::from_secs(2), open_screen_client_options());
    let share = share_screen_slot(open, &a.next_share_keys(), WAIT, OPEN_SCREEN_SLOT).await.unwrap();
    let frames = [video(900, 1), video(120, 2), video(2500, 3)];
    for (i, f) in frames.iter().enumerate() {
        assert!(share.push(f, i == 0).unwrap());
    }
    let got = collect_frames(&mut b, frames.len()).await;
    for (g, want) in got.iter().zip(&frames) {
        assert_eq!(&g.data, want);
    }
    share.stop().await;
}

#[tokio::test]
async fn a_second_screen_share_in_the_same_call_reaches_the_viewer() {
    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(4, &phone.local_addr().to_string()).await;
    let bob_pub = PublicKey::from(&bob);
    let voice = cands.clone();
    let caller = tokio::spawn(async move { call(client(voice), &alice, &bob_pub, Mulaw::new(), WAIT).await });
    let incoming = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap();
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let a = caller.await.unwrap().unwrap();
    for round in 0..3u8 {
        let share = share_screen(screen_client(cands.clone()), &a.next_share_keys(), WAIT).await.unwrap();
        let f = video(600, round);
        share.push(&f, true).unwrap();
        let got = collect_frames(&mut b, 1).await;
        assert_eq!(got[0].data, f, "transmissão {round}");
        share.stop().await;
    }
}

#[tokio::test]
async fn the_screen_without_privacy_carries_a_burst_of_big_frames_quickly() {
    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(4, &phone.local_addr().to_string()).await;
    let bob_pub = PublicKey::from(&bob);
    let voice = cands.clone();
    let caller = tokio::spawn(async move { call(client(voice), &alice, &bob_pub, Mulaw::new(), WAIT).await });
    let incoming = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap();
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let a = caller.await.unwrap().unwrap();
    let c = CircuitClient::spawn_with(CircuitManager::new(ManagerConfig::default(), "contato:9", cands), Duration::from_secs(2), open_screen_client_options());
    let share = share_screen_slot(c, &a.next_share_keys(), WAIT, OPEN_SCREEN_SLOT).await.unwrap();
    let started = std::time::Instant::now();
    let f = video(30_000, 1);
    for i in 0..3 {
        assert!(share.push(&f, i == 0).unwrap());
    }
    let got = collect_frames(&mut b, 3).await;
    assert!(got.iter().all(|g| g.data == f));
    // 90 kB são ~530 pacotes: com 0,5 ms por pacote cabem em ~0,3 s; o quíntuplo do tempo cobre a carga do teste.
    assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
    assert!(share.stats().packets_sent >= 500, "{}", share.stats().packets_sent);
}

#[tokio::test]
async fn callee_shares_its_screen_to_the_caller_through_the_callers_phone() {
    let (alice, bob) = ids();
    let alice_pub = PublicKey::from(&alice);
    let mut alice_phone = Phone::bind("127.0.0.1:0".parse().unwrap(), alice.clone(), vec![*PublicKey::from(&bob).as_bytes()]).await.unwrap();
    let mut bob_phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*alice_pub.as_bytes()]).await.unwrap();
    let (_s1, to_bob) = relays(4, &bob_phone.local_addr().to_string()).await;
    let (_s2, to_alice) = relays(4, &alice_phone.local_addr().to_string()).await;
    let link = alice_phone.link();
    let bob_pub = PublicKey::from(&bob);
    let caller = tokio::spawn(async move { call_linked(client(to_bob), &alice, &bob_pub, Mulaw::new(), WAIT, Some(&link)).await });
    let incoming = timeout(WAIT, bob_phone.next_incoming()).await.unwrap().unwrap();
    let b = incoming.accept(Mulaw::new()).await.unwrap();
    let mut a = caller.await.unwrap().unwrap();

    let share = share_screen(screen_client(to_alice), &b.next_share_keys(), WAIT).await.unwrap();
    let frames = [video(700, 9), video(300, 8)];
    for (i, f) in frames.iter().enumerate() {
        share.push(f, i == 0).unwrap();
    }
    let got = collect_frames(&mut a, 2).await;
    assert_eq!(got.iter().map(|g| g.data.clone()).collect::<Vec<_>>(), frames);
    // Ninguém ligou para a alice: o circuito de tela não vira convite.
    assert!(timeout(Duration::from_millis(200), alice_phone.next_incoming()).await.is_err());
}

#[tokio::test]
async fn screen_circuit_with_foreign_keys_shows_nothing() {
    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(4, &phone.local_addr().to_string()).await;
    let bob_pub = PublicKey::from(&bob);
    let voice = cands.clone();
    let caller = tokio::spawn(async move { call(client(voice), &alice, &bob_pub, Mulaw::new(), WAIT).await });
    let mut b = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap().accept(Mulaw::new()).await.unwrap();
    let _a = caller.await.unwrap().unwrap();

    // Outra chamada, outras chaves: o telefone não prende o circuito a esta chamada.
    let (x, y) = ids();
    let (xe, ye) = (clam::media::keys::Ephemeral::generate(), clam::media::keys::Ephemeral::generate());
    let foreign = clam::media::keys::CallKeys::initiator(&x, &xe, &PublicKey::from(&y), &ye.public()).unwrap();
    let share = share_screen(screen_client(cands), &foreign, WAIT).await.unwrap();
    share.push(&video(500, 1), true).unwrap();
    assert!(timeout(Duration::from_millis(800), b.next_screen()).await.is_err());
}

#[cfg(feature = "video")]
#[tokio::test]
async fn synthetic_screen_goes_through_h264_and_the_screen_circuit() {
    use clam::video::{H264Decoder, H264Encoder, RawFrame, ScreenSource, pump};

    struct Bars(usize);
    impl ScreenSource for Bars {
        fn grab(&mut self) -> Option<RawFrame> {
            self.0 += 1;
            let (w, h) = (160, 96);
            let rgba = (0..w * h).flat_map(|i| if (i % w + self.0 * 8) % 64 < 32 { [220, 40, 40, 255] } else { [40, 40, 220, 255] }).collect();
            RawFrame::new(w, h, rgba).ok()
        }
    }

    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(4, &phone.local_addr().to_string()).await;
    let bob_pub = PublicKey::from(&bob);
    let voice = cands.clone();
    let caller = tokio::spawn(async move { call(client(voice), &alice, &bob_pub, Mulaw::new(), WAIT).await });
    let incoming = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap();
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let a = caller.await.unwrap().unwrap();

    let share = share_screen(screen_client(cands), &a.next_share_keys(), WAIT).await.unwrap();
    let mut src = Bars(0);
    let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
    let mut dec = H264Decoder::new().unwrap();
    let mut shown = None;
    let mut sent = 0;
    while sent < 6 {
        if pump(&mut src, &mut enc, &share).unwrap() {
            sent += 1;
        }
        while let Ok(Some(ScreenEvent::Frame(f))) = timeout(Duration::from_millis(150), b.next_screen()).await {
            if let Some(img) = dec.decode(&f.data).unwrap() {
                shown = Some(img);
            }
        }
    }
    let img = shown.expect("a tela chegou");
    assert_eq!((img.width, img.height), (160, 96));
    share.stop().await;
}

#[cfg(feature = "video")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_size_screen_gets_through_at_the_circuit_slot_rate() {
    use clam::video::{FPS, H264Decoder, H264Encoder, RawFrame, ScreenSource, pump};

    /// Fundo muito detalhado e parado, com um quadrado que anda: o pior caso para o quadro-chave.
    struct Bars(usize, Vec<u8>);
    impl ScreenSource for Bars {
        fn grab(&mut self) -> Option<RawFrame> {
            self.0 += 1;
            let (w, h) = (1280, 720);
            let mut rgba = self.1.clone();
            let x0 = self.0 % 30 * 40;
            for y in 200..240 {
                for x in x0..x0 + 40 {
                    rgba[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
            RawFrame::new(w, h, rgba).ok()
        }
    }

    let (alice, bob) = ids();
    let mut phone = Phone::bind("127.0.0.1:0".parse().unwrap(), bob.clone(), vec![*PublicKey::from(&alice).as_bytes()]).await.unwrap();
    let (_srv, cands) = relays(4, &phone.local_addr().to_string()).await;
    let bob_pub = PublicKey::from(&bob);
    let voice = cands.clone();
    let caller = tokio::spawn(async move { call(client(voice), &alice, &bob_pub, Mulaw::new(), WAIT).await });
    let incoming = timeout(WAIT, phone.next_incoming()).await.unwrap().unwrap();
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let a = caller.await.unwrap().unwrap();

    let share = share_screen(screen_client(cands), &a.next_share_keys(), WAIT).await.unwrap();
    let background = (0..1280 * 720).flat_map(|i| {
        let n = (((i % 1280 / 3 * 7 + i / 1280 / 3 * 13) * 2654435761usize) >> 24) as u8;
        [n, n / 2, 90, 255]
    });
    let mut src = Bars(0, background.collect());
    let mut enc = H264Encoder::new(H264Encoder::default_bitrate()).unwrap();
    let mut dec = H264Decoder::new().unwrap();
    let mut shown = None;
    let mut count = 0;
    let t0 = std::time::Instant::now();
    let mut lost = 0;
    while t0.elapsed() < Duration::from_secs(8) {
        pump(&mut src, &mut enc, &share).unwrap();
        let end = tokio::time::Instant::now() + Duration::from_secs_f32(1.0 / FPS);
        while let Ok(Some(ev)) = tokio::time::timeout_at(end, b.next_screen()).await {
            if let ScreenEvent::Lost(n) = ev {
                lost = n;
            }
            if let ScreenEvent::Frame(f) = ev
                && let Some(img) = dec.decode(&f.data).unwrap()
            {
                shown = Some(img);
                count += 1;
            }
        }
    }
    // Cada troca de resolução recomeça com um quadro-chave que descarta o que esperava na fila.
    assert!(lost <= 4, "{lost} quadros perdidos no caminho");
    assert!(count >= 20, "só {count} imagens em 8 s");
    let img = shown.expect("a tela chegou");
    assert!(img.width <= 1920 && img.width >= 320);
    share.stop().await;
}
