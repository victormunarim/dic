#![cfg(feature = "net")]
//! Chamada completa pela introdução e pelo encontro às cegas (etapa 3 de "2 relays"), tudo em 127.0.0.1: D espera
//! num ponto de introdução, C chega por `intro:9` e `rv:9` sem receber nenhum endereço de D, e a chamada normal
//! (`call_linked` e `Phone`) roda no circuito do encontro.
//!
//! Em loopback todo mundo é 127.0.0.1, então "sem o IP do outro" aqui é estrutural: C só tem o `IntroTarget`
//! (relay e chave de serviço) e a chave de contato de D, e D só tem o blob. O que esconde o IP na rede real é o
//! primeiro salto em relay de terceiro, que `circuit` exige.

use std::time::Duration;

use clam::{
    circuit::{
        manager::Candidate,
        relay::{RelayConfig, RelayNode},
    },
    hidden::{self, INTRO_DEST, RV_DEST},
    media::codec::Mulaw,
    meet::{HiddenService, IntroTarget, MeetError, ServiceConfig, meet},
    pairing::PresenceKeys,
    phone::{ActiveCall, Phone, call_linked},
    presence::IntroPointV2,
    service::{self, PresenceConfig},
    store::MemoryStore,
    ed25519_dalek::SigningKey,
    rand_core::OsRng,
    transport::{RelayServer, ServerConfig, allow_list},
    x25519_dalek::{PublicKey, StaticSecret},
};
use tokio::{
    net::TcpListener,
    time::{Instant, timeout},
};

const WAIT: Duration = Duration::from_secs(15);

fn tone(n: usize) -> Vec<i16> {
    (0..160).map(|i| (9000.0 * (std::f64::consts::TAU * 440.0 * (n * 160 + i) as f64 / 8000.0).sin()) as i16).collect()
}

/// Cinco relays que saem para o mesmo ponto de introdução e o mesmo de encontro.
async fn relays() -> (Vec<RelayServer>, Vec<Candidate>) {
    let intro = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rv = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let exit = vec![(INTRO_DEST.to_string(), intro.local_addr().unwrap().to_string()), (RV_DEST.to_string(), rv.local_addr().unwrap().to_string())];
    tokio::spawn(hidden::serve_intro(intro));
    tokio::spawn(hidden::serve_rendezvous(rv));
    let (mut servers, mut cands) = (vec![], vec![]);
    for _ in 0..5 {
        let node = RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0);
        let public = node.public_key();
        let mut cfg = ServerConfig::new("127.0.0.1:0".parse().unwrap());
        cfg.exit = allow_list(exit.clone());
        let srv = RelayServer::spawn(node, cfg).await.unwrap();
        cands.push(Candidate { addr: srv.local_addr().to_string(), public, link_public: srv.link_public_key(), score: 1.0 });
        servers.push(srv);
    }
    (servers, cands)
}

/// Manda `n` quadros de `tx` e devolve o que `rx` tocar até o prazo. Os dois lados falam, como a GUI.
async fn talk(tx: &ActiveCall, rx: &mut ActiveCall, n: usize) -> usize {
    let (mut sent, mut got) = (0, 0);
    let end = Instant::now() + Duration::from_millis(20 * n as u64 + 500);
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    while Instant::now() < end {
        tokio::select! {
            _ = tick.tick() => {
                if sent < n { tx.send(tone(sent)).await.unwrap(); sent += 1; }
                rx.send(vec![0; 160]).await.unwrap();
            }
            f = rx.next_pcm() => match f { Some(_) => got += 1, None => break },
            () = tokio::time::sleep_until(end) => break,
        }
    }
    got
}

struct World {
    _servers: Vec<RelayServer>,
    cands: Vec<Candidate>,
    caller: StaticSecret,
    callee: StaticSecret,
    phone: Phone,
    service: HiddenService,
}

/// D sobe o telefone e o serviço; o ponto de introdução é o relay 0 e o de encontro (de C) o relay 1.
async fn world(contacts_of_d: impl FnOnce(&StaticSecret) -> Vec<[u8; 32]>) -> World {
    let (servers, cands) = relays().await;
    let (caller, callee) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
    let phone = Phone::bind("127.0.0.1:0".parse().unwrap(), callee.clone(), contacts_of_d(&caller)).await.unwrap();
    let service = HiddenService::start(ServiceConfig {
        secret: callee.clone(),
        phone: phone.local_addr(),
        contacts: phone.contacts(),
        pool: cands.clone(),
        intros: vec![cands[0].clone()],
    })
    .await
    .unwrap();
    World { _servers: servers, cands, caller, callee, phone, service }
}

#[tokio::test]
async fn a_call_is_made_through_introduction_and_rendezvous() {
    let mut w = world(|c| vec![*PublicKey::from(c).as_bytes()]).await;
    assert_eq!(w.service.points().len(), 1);
    let via = w.service.points()[0].clone();
    let callee_pub = PublicKey::from(&w.callee);

    let client = timeout(WAIT, meet(&w.caller, &callee_pub, &via, &w.cands[1], &w.cands)).await.expect("encontro demorou").expect("encontro falhou");
    let caller = w.caller.clone();
    let call = tokio::spawn(async move { call_linked(client, &caller, &callee_pub, Mulaw::new(), WAIT, None).await });
    let incoming = timeout(WAIT, w.phone.next_incoming()).await.expect("D não recebeu a chamada").unwrap();
    assert_eq!(incoming.caller().as_bytes(), PublicKey::from(&w.caller).as_bytes());
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let mut a = call.await.unwrap().unwrap();

    assert!(talk(&a, &mut b, 30).await >= 20, "ida");
    assert!(talk(&b, &mut a, 30).await >= 20, "volta");
    a.hangup();
}

#[tokio::test]
async fn an_introduction_from_a_stranger_is_dropped_silently() {
    let mut w = world(|_| vec![]).await;
    let via = w.service.points()[0].clone();
    let callee_pub = PublicKey::from(&w.callee);

    // O ponto entrega (ele não conhece contatos), mas D descarta: o encontro nunca acontece.
    let result = timeout(Duration::from_secs(45), meet(&w.caller, &callee_pub, &via, &w.cands[1], &w.cands)).await.expect("sem prazo");
    assert!(matches!(result, Err(MeetError::NoMeeting)), "esperava NoMeeting, veio {:?}", result.map(|_| ()));
    assert!(timeout(Duration::from_millis(300), w.phone.next_incoming()).await.is_err(), "o telefone de D não pode tocar");
}

#[tokio::test]
async fn two_introductions_in_a_row_both_reach_the_meeting() {
    let w = world(|c| vec![*PublicKey::from(c).as_bytes()]).await;
    let via = w.service.points()[0].clone();
    let callee_pub = PublicKey::from(&w.callee);
    // Cada introdução usa cookie e circuitos novos; o registro de D no ponto continua valendo entre elas.
    for round in 0..2 {
        let c = timeout(WAIT, meet(&w.caller, &callee_pub, &via, &w.cands[1], &w.cands)).await.unwrap().unwrap_or_else(|e| panic!("encontro {round}: {e}"));
        c.stop();
    }
}

#[tokio::test]
async fn without_third_party_relays_the_service_does_not_start() {
    let (_servers, cands) = relays().await;
    let callee = StaticSecret::random_from_rng(OsRng);
    let phone = Phone::bind("127.0.0.1:0".parse().unwrap(), callee.clone(), vec![]).await.unwrap();
    let result = HiddenService::start(ServiceConfig { secret: callee, phone: phone.local_addr(), contacts: phone.contacts(), pool: vec![], intros: vec![cands[0].clone()] }).await;
    assert!(matches!(result, Err(MeetError::NoPool)));
}

/// O caminho inteiro sem endereço: D publica os pontos na presença v2 (cifrada só para C), C os busca no
/// armazenamento, encontra D e liga. C nunca recebe um endereço de D; só os pontos de introdução.
#[tokio::test]
async fn a_contact_finds_the_intro_points_in_the_presence_record_and_calls() {
    let mut w = world(|c| vec![*PublicKey::from(c).as_bytes()]).await;
    let (caller_pub, callee_pub) = (PublicKey::from(&w.caller), PublicKey::from(&w.callee));
    let (d_keys, c_keys) = (PresenceKeys::derive(&w.callee, &caller_pub), PresenceKeys::derive(&w.caller, &callee_pub));
    let (signer, store, cfg, now) = (SigningKey::generate(&mut OsRng), MemoryStore::new(), PresenceConfig::default(), 1_800_000_000);

    let points: Vec<IntroPointV2> = w.service.points().iter().map(IntroPointV2::from).collect();
    service::publish_v2(&store, &cfg, &d_keys, &signer, now, &points).unwrap();

    let found = service::lookup_v2(&store, &cfg, &c_keys, &signer.verifying_key(), now).expect("C não achou a presença de D");
    let via = IntroTarget::from(found.first().expect("sem ponto de introdução"));
    assert_eq!(via, w.service.points()[0], "o ponto lido é o que D publicou");

    let client = timeout(WAIT, meet(&w.caller, &callee_pub, &via, &w.cands[1], &w.cands)).await.unwrap().unwrap();
    let caller = w.caller.clone();
    let call = tokio::spawn(async move { call_linked(client, &caller, &callee_pub, Mulaw::new(), WAIT, None).await });
    let incoming = timeout(WAIT, w.phone.next_incoming()).await.expect("D não recebeu a chamada").unwrap();
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let a = call.await.unwrap().unwrap();
    assert!(talk(&a, &mut b, 20).await >= 12);
    a.hangup();
}

/// Medição em loopback do encontro (não roda junto dos outros: `--ignored --nocapture`). Imprime o tempo de montagem
/// (introdução + encontro + `call_linked`) e a latência do primeiro quadro de voz e o jitter dos seguintes. Sem rede real, isto é o
/// custo do protocolo e do cover; os números entre máquinas distintas somam a rede por cima (veja `docs/CIRCUITO.md`).
#[tokio::test]
#[ignore = "medição: imprime números, não afirma limites"]
async fn measure_the_meeting_setup_and_voice_latency() {
    let mut w = world(|c| vec![*PublicKey::from(c).as_bytes()]).await;
    let via = w.service.points()[0].clone();
    let callee_pub = PublicKey::from(&w.callee);

    let t0 = Instant::now();
    let client = timeout(WAIT, meet(&w.caller, &callee_pub, &via, &w.cands[1], &w.cands)).await.expect("encontro demorou").expect("encontro falhou");
    let t_meet = t0.elapsed();
    let caller = w.caller.clone();
    let call = tokio::spawn(async move { call_linked(client, &caller, &callee_pub, Mulaw::new(), WAIT, None).await });
    let incoming = timeout(WAIT, w.phone.next_incoming()).await.expect("D não recebeu a chamada").unwrap();
    let mut b = incoming.accept(Mulaw::new()).await.unwrap();
    let mut a = call.await.unwrap().unwrap();
    let t_call = t0.elapsed();

    // Voz contínua de `a` para `b`, um quadro com tom a cada 20 ms. Quadros com energia chegam em ordem; o primeiro dá
    // a latência de ida (mesmo relógio, mesmo processo) e a variação dos seguintes em torno do ritmo de 20 ms dá o jitter.
    const FRAMES: usize = 150;
    let start = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let (mut sent, mut arrivals) = (0, Vec::new());
    let end = start + Duration::from_millis(20 * FRAMES as u64 + 1000);
    while Instant::now() < end && arrivals.len() < FRAMES {
        tokio::select! {
            _ = tick.tick() => {
                if sent < FRAMES { a.send(tone(sent)).await.unwrap(); sent += 1; }
                // O lado de lá também fala (silêncio), como a GUI.
                b.send(vec![0; 160]).await.unwrap();
            }
            _ = a.next_pcm() => {}
            f = b.next_pcm() => match f {
                Some(pcm) if pcm.iter().any(|s| s.unsigned_abs() > 2000) => arrivals.push(start.elapsed()),
                Some(_) => {}
                None => break,
            },
            () = tokio::time::sleep_until(end) => break,
        }
    }
    let first = arrivals.first().copied().unwrap_or_default();
    let mut jitter: Vec<Duration> = arrivals.iter().enumerate().map(|(k, t)| t.abs_diff(first + Duration::from_millis(20 * k as u64))).collect();
    jitter.sort();
    let at = |p: usize| jitter.get(jitter.len().saturating_sub(1) * p / 100).copied().unwrap_or_default();
    println!("encontro: {t_meet:?}; até a chamada pronta: {t_call:?}; quadros {}/{FRAMES}; 1º quadro {first:?}; jitter p50 {:?} p95 {:?} máx {:?}", arrivals.len(), at(50), at(95), at(100));
    a.hangup();
}
