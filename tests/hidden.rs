#![cfg(feature = "net")]
//! `intro:9` e `rv:9` por circuitos reais de 2 saltos em 127.0.0.1: quem atende (D) espera num ponto de introdução,
//! quem liga (C) o alcança sem saber o endereço de D, e os dois se encontram num ponto de encontro.

use std::time::Duration;

use clam::{
    circuit::{
        manager::{Candidate, CircuitManager, Event, ManagerConfig},
        relay::{RelayConfig, RelayNode},
    },
    ed25519_dalek::{Signer, SigningKey},
    hidden::{self, CHUNK, INTRO_DEST, IntroReply, IntroRequest, RV_DEST, RvReply, RvRequest},
    rand_core::OsRng,
    transport::{CircuitClient, RelayServer, ServerConfig, allow_list},
    x25519_dalek::StaticSecret,
};
use tokio::{net::TcpListener, time::timeout};

/// Quatro relays que saem para o mesmo ponto de introdução e o mesmo ponto de encontro.
async fn relays() -> (Vec<RelayServer>, Vec<Candidate>) {
    let intro = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rv = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let exit = vec![(INTRO_DEST.to_string(), intro.local_addr().unwrap().to_string()), (RV_DEST.to_string(), rv.local_addr().unwrap().to_string())];
    tokio::spawn(hidden::serve_intro(intro));
    tokio::spawn(hidden::serve_rendezvous(rv));
    let (mut servers, mut cands) = (vec![], vec![]);
    for _ in 0..4 {
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

/// Um circuito de 2 saltos até `dest`, já pronto.
async fn circuit(cands: &[Candidate], dest: &str) -> CircuitClient {
    let mgr = CircuitManager::new(ManagerConfig::default(), dest, cands.to_vec());
    let mut c = CircuitClient::spawn(mgr, Duration::from_secs(2));
    timeout(Duration::from_secs(10), async {
        loop {
            if let Some(Event::Ready { .. }) = c.next_event().await {
                return;
            }
        }
    })
    .await
    .expect("circuito não ficou pronto");
    c
}

async fn say(c: &mut CircuitClient, body: &[u8]) {
    let mut message = (body.len() as u16).to_be_bytes().to_vec();
    message.extend_from_slice(body);
    for piece in message.chunks(CHUNK) {
        c.send(piece).await.unwrap();
    }
}

/// Próxima mensagem `u16 tamanho ‖ corpo` que chega pelo circuito. `acc` guarda o que sobrar.
async fn hear(c: &mut CircuitClient, acc: &mut Vec<u8>) -> Vec<u8> {
    timeout(Duration::from_secs(5), async {
        loop {
            if acc.len() >= 2 {
                let total = usize::from(u16::from_be_bytes([acc[0], acc[1]]));
                if acc.len() >= 2 + total {
                    return acc.drain(..2 + total).skip(2).collect();
                }
            }
            if let Some(Event::Data(d)) = c.next_event().await {
                acc.extend_from_slice(&d);
            }
        }
    })
    .await
    .expect("sem resposta do ponto")
}

#[tokio::test]
async fn an_introduction_crosses_two_hops_to_a_waiting_service() {
    let (_servers, cands) = relays().await;
    let signer = SigningKey::generate(&mut OsRng);
    let auth_key = signer.verifying_key().to_bytes();

    // D registra a chave de serviço por um circuito de espera.
    let (mut d, mut d_buf) = (circuit(&cands, INTRO_DEST).await, Vec::new());
    say(&mut d, &IntroRequest::Establish { auth_key }.encode()).await;
    let Some(IntroReply::Challenge(nonce)) = IntroReply::parse(&hear(&mut d, &mut d_buf).await) else { panic!("sem desafio") };
    let signature = signer.sign(&hidden::proof_message(&nonce, &auth_key)).to_bytes();
    say(&mut d, &IntroRequest::Prove { signature }.encode()).await;
    assert_eq!(IntroReply::parse(&hear(&mut d, &mut d_buf).await), Some(IntroReply::Established));

    // C, por outro circuito, entrega um blob que o ponto não lê.
    let (mut c, mut c_buf) = (circuit(&cands, INTRO_DEST).await, Vec::new());
    let blob = vec![0x5a; hidden::MAX_BLOB];
    say(&mut c, &IntroRequest::Introduce { auth_key, blob: blob.clone() }.encode()).await;
    assert_eq!(IntroReply::parse(&hear(&mut c, &mut c_buf).await), Some(IntroReply::Delivered));
    assert_eq!(IntroReply::parse(&hear(&mut d, &mut d_buf).await), Some(IntroReply::Push(blob)));
}

#[tokio::test]
async fn two_circuits_meet_at_the_rendezvous_and_talk() {
    let (_servers, cands) = relays().await;
    let cookie = [0x77; 32];

    let (mut c, mut c_buf) = (circuit(&cands, RV_DEST).await, Vec::new());
    say(&mut c, &RvRequest::Establish { cookie }.encode()).await;
    assert_eq!(RvReply::parse(&hear(&mut c, &mut c_buf).await), Some(RvReply::Waiting));

    let (mut d, mut d_buf) = (circuit(&cands, RV_DEST).await, Vec::new());
    say(&mut d, &RvRequest::Join { cookie }.encode()).await;
    assert_eq!(RvReply::parse(&hear(&mut d, &mut d_buf).await), Some(RvReply::Joined));
    assert_eq!(RvReply::parse(&hear(&mut c, &mut c_buf).await), Some(RvReply::Joined));

    // Depois do encontro, os bytes passam crus de um circuito ao outro.
    c.send(b"oi, D").await.unwrap();
    d.send(b"oi, C").await.unwrap();
    let mut at_d = Vec::new();
    let mut at_c = Vec::new();
    timeout(Duration::from_secs(5), async {
        while at_d.len() < 5 || at_c.len() < 5 {
            // Evento que não é dado (ou um lado já completo) não pode desativar o ramo: o `select!` entraria em pânico.
            tokio::select! {
                ev = d.next_event(), if at_d.len() < 5 => if let Some(Event::Data(x)) = ev { at_d.extend_from_slice(&x) },
                ev = c.next_event(), if at_c.len() < 5 => if let Some(Event::Data(x)) = ev { at_c.extend_from_slice(&x) },
            }
        }
    })
    .await
    .expect("os dados não atravessaram o encontro");
    assert_eq!((at_d.as_slice(), at_c.as_slice()), (&b"oi, D"[..], &b"oi, C"[..]));
}
