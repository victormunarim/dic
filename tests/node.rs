#![cfg(feature = "net")]
//! Primeiro contato pelo diretório e chamada entre nós, tudo em 127.0.0.1.

use std::time::Duration;

use clam::directory::{self, DirectoryServer, HostConfig};
use clam::media::codec::Mulaw;
use clam::node::{DialError, Node, NodeConfig};
use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{Instant, timeout},
};

fn key() -> StaticSecret {
    StaticSecret::random_from_rng(OsRng)
}

fn tone(n: usize) -> Vec<i16> {
    (0..160).map(|i| (9000.0 * (std::f64::consts::TAU * 440.0 * (n * 160 + i) as f64 / 8000.0).sin()) as i16).collect()
}

async fn dir() -> (DirectoryServer, String) {
    let d = DirectoryServer::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let a = d.local_addr().to_string();
    (d, a)
}

fn cfg(secret: StaticSecret, dir: &str, contacts: Vec<[u8; 32]>) -> NodeConfig {
    NodeConfig { relay_port: 0, ..NodeConfig::new(secret, dir, contacts) }
}

#[tokio::test]
async fn publish_lookup_and_forgery() {
    let (_d, addr) = dir().await;
    let s = key();
    let me = *PublicKey::from(&s).as_bytes();
    assert!(directory::lookup(&addr, &me).await.unwrap().is_none());
    let published = directory::publish(&addr, &s, 4001, [1; 32], [2; 32], None).await.unwrap();
    assert_eq!(published, "127.0.0.1:4001");
    let found = directory::lookup(&addr, &me).await.unwrap().unwrap();
    assert_eq!((found.onion, found.link, found.addr.as_str()), ([1; 32], [2; 32], "127.0.0.1:4001"));

    // Falsificação: pedido anunciando a pública de `me`, mas com a prova feita por outra secreta.
    let other = key();
    let mut t = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let hello = read_frame(&mut t).await;
    let e: [u8; 32] = hello[..32].try_into().unwrap();
    let mut req = vec![1u8];
    req.extend_from_slice(&me);
    req.extend_from_slice(&[9; 64]);
    req.extend_from_slice(&6666u16.to_be_bytes());
    req.push(0);
    let shared = other.diffie_hellman(&PublicKey::from(e));
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(shared.as_bytes()).unwrap();
    m.update(b"clam-dir-v1");
    m.update(&e);
    m.update(&req);
    req.extend_from_slice(&m.finalize().into_bytes());
    let mut f = (req.len() as u16).to_be_bytes().to_vec();
    f.extend_from_slice(&req);
    t.write_all(&f).await.unwrap();
    let reply = read_frame(&mut t).await;
    assert_eq!(reply[0], 0x81, "pedido forjado deveria ser recusado");
    let still = directory::lookup(&addr, &me).await.unwrap().unwrap();
    assert_eq!(still.addr, "127.0.0.1:4001");
}

async fn read_frame(t: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut len = [0u8; 2];
    t.read_exact(&mut len).await.unwrap();
    let mut b = vec![0u8; u16::from_be_bytes(len) as usize];
    t.read_exact(&mut b).await.unwrap();
    b
}

#[tokio::test]
async fn sample_excludes_and_limits() {
    let (_d, addr) = dir().await;
    let keys: Vec<_> = (0..4).map(|_| key()).collect();
    for (i, k) in keys.iter().enumerate() {
        directory::publish(&addr, k, 5000 + i as u16, [i as u8; 32], [0; 32], None).await.unwrap();
    }
    let skip = [*PublicKey::from(&keys[0]).as_bytes()];
    let got = directory::sample(&addr, 8, &skip).await.unwrap();
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|r| r.addr != "127.0.0.1:5000"));
    assert_eq!(directory::sample(&addr, 2, &[]).await.unwrap().len(), 2);
}

#[tokio::test]
async fn dial_unknown_key_says_not_found() {
    let (_d, addr) = dir().await;
    let a = Node::start(cfg(key(), &addr, vec![])).await.unwrap();
    let r = a.dial([7; 32], Mulaw::new(), Duration::from_secs(2)).await;
    assert!(matches!(r, Err(DialError::NotFound)));
}

/// Duas pessoas, só o diretório em comum: liga e conversa nos dois sentidos.
#[tokio::test]
async fn two_nodes_talk_through_directory() {
    let (_d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(cfg(sa, &addr, vec![])).await.unwrap();
    let mut b = Node::start(cfg(sb, &addr, vec![pa])).await.unwrap();

    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        assert_eq!(inc.caller().as_bytes(), &pa);
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let mut ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (mut cb, _b) = answer.await.unwrap();

    let end = Instant::now() + Duration::from_millis(1500);
    let (mut n, mut got_a, mut got_b) = (0, 0, 0);
    while Instant::now() < end {
        ca.send(tone(n)).await.unwrap();
        cb.send(tone(n)).await.unwrap();
        n += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
        while let Ok(Some(f)) = timeout(Duration::from_millis(1), cb.next_pcm()).await {
            got_b += f.iter().any(|x| x.abs() > 1000) as usize;
        }
        while let Ok(Some(f)) = timeout(Duration::from_millis(1), ca.next_pcm()).await {
            got_a += f.iter().any(|x| x.abs() > 1000) as usize;
        }
    }
    assert!(got_a >= 20 && got_b >= 20, "a {got_a} b {got_b}");
    ca.hangup();
}

/// Com um terceiro nó online, o primeiro salto é sorteado entre os outros.
#[tokio::test]
async fn third_node_serves_as_first_hop() {
    let (_d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(cfg(sa, &addr, vec![])).await.unwrap();
    let mut b = Node::start(cfg(sb, &addr, vec![pa])).await.unwrap();
    let _c = Node::start(cfg(key(), &addr, vec![])).await.unwrap();
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let _ = answer.await.unwrap();
    ca.hangup();
}

async fn hosted_call(a_hosted: bool, b_hosted: bool) {
    let d = DirectoryServer::spawn_with(
        "127.0.0.1:0".parse().unwrap(),
        Some(HostConfig { relay_listen: "127.0.0.1:0".parse().unwrap(), relay_public: None, verbose: false }),
    )
    .await
    .unwrap();
    let addr = d.local_addr().to_string();
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(NodeConfig { via_server: a_hosted, ..cfg(sa, &addr, vec![]) }).await.unwrap();
    let mut b = Node::start(NodeConfig { via_server: b_hosted, ..cfg(sb, &addr, vec![pa]) }).await.unwrap();
    if b_hosted {
        assert_eq!(b.published_addr(), d.relay_addr().unwrap().to_string());
    }
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let mut ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (mut cb, _b) = answer.await.unwrap();
    let end = Instant::now() + Duration::from_millis(1500);
    let (mut n, mut got_a, mut got_b) = (0, 0, 0);
    while Instant::now() < end {
        ca.send(tone(n)).await.unwrap();
        cb.send(tone(n)).await.unwrap();
        n += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
        while let Ok(Some(f)) = timeout(Duration::from_millis(1), cb.next_pcm()).await {
            got_b += f.iter().any(|x| x.abs() > 1000) as usize;
        }
        while let Ok(Some(f)) = timeout(Duration::from_millis(1), ca.next_pcm()).await {
            got_a += f.iter().any(|x| x.abs() > 1000) as usize;
        }
    }
    assert!(got_a >= 20 && got_b >= 20, "a {got_a} b {got_b}");
    ca.hangup();
}

/// Quem atende está atrás de NAT (sem porta aberta): o relay do servidor emenda a conexão por uma saída do nó.
#[tokio::test]
async fn hosted_callee_talks_through_server_relay() {
    hosted_call(false, true).await;
}

#[tokio::test]
async fn both_hosted_talk() {
    hosted_call(true, true).await;
}

#[tokio::test]
async fn host_needs_proof_and_server_support() {
    // Servidor sem relay recusa.
    let (_d, addr) = dir().await;
    let err = directory::host(&addr, &key(), "127.0.0.1:1".parse().unwrap(), Default::default()).await.unwrap_err();
    assert!(err.to_string().contains("sem relay"), "{err}");

    // Prova de posse falsa: pede para hospedar a chave de `me` com a prova de outra secreta.
    let d = DirectoryServer::spawn_with(
        "127.0.0.1:0".parse().unwrap(),
        Some(HostConfig { relay_listen: "127.0.0.1:0".parse().unwrap(), relay_public: None, verbose: false }),
    )
    .await
    .unwrap();
    let me = *PublicKey::from(&key()).as_bytes();
    let mut t = tokio::net::TcpStream::connect(d.local_addr()).await.unwrap();
    let hello = read_frame(&mut t).await;
    let e: [u8; 32] = hello[..32].try_into().unwrap();
    let mut req = vec![4u8];
    req.extend_from_slice(&me);
    let shared = key().diffie_hellman(&PublicKey::from(e));
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(shared.as_bytes()).unwrap();
    m.update(b"clam-dir-v1");
    m.update(&e);
    m.update(&req);
    req.extend_from_slice(&m.finalize().into_bytes());
    let mut f = (req.len() as u16).to_be_bytes().to_vec();
    f.extend_from_slice(&req);
    t.write_all(&f).await.unwrap();
    assert_eq!(read_frame(&mut t).await[0], 0x81);
    assert!(directory::lookup(&d.local_addr().to_string(), &me).await.unwrap().is_none());
}

/// Falar com o diretório ajusta o desvio de hora (aqui ~0, pois é a mesma máquina).
#[tokio::test]
async fn directory_time_overrides_wrong_local_clock() {
    let (_d, addr) = dir().await;
    directory::seen_ip(&addr).await.unwrap();
    // O desvio medido contra um servidor na mesma máquina é ~0.
    assert!(clam::clock::offset_secs().abs() <= 1);
}
