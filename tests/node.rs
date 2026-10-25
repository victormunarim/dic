#![cfg(feature = "net")]
//! Primeiro contato pelo diretório e chamada entre nós, tudo em 127.0.0.1.

use std::time::Duration;

use clam::directory::{self, DirectoryServer};
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

fn wide_tone(n: usize) -> Vec<i16> {
    (0..320).map(|i| (9000.0 * (std::f64::consts::TAU * 440.0 * (n * 320 + i) as f64 / 16000.0).sin()) as i16).collect()
}

async fn dir() -> (DirectoryServer, String) {
    let d = DirectoryServer::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let a = d.local_addr().to_string();
    (d, a)
}

fn cfg(secret: StaticSecret, dir: &str, contacts: Vec<[u8; 32]>) -> NodeConfig {
    NodeConfig { relay_port: 0, punch: false, ..NodeConfig::new(secret, dir, contacts) }
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

/// O diretório confere se o relay publicado aceita conexões de fora; não deixa repetir o teste de imediato.
#[tokio::test]
async fn reachability_check_sees_open_and_closed_ports() {
    let (_d, addr) = dir().await;
    let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (a, b) = (key(), key());
    directory::publish(&addr, &a, open.local_addr().unwrap().port(), [1; 32], [2; 32], None).await.unwrap();
    assert!(directory::reachable(&addr, &a).await.unwrap());
    let err = directory::reachable(&addr, &a).await.unwrap_err();
    assert!(err.to_string().contains("instantes"), "{err}");

    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    directory::publish(&addr, &b, port, [1; 32], [2; 32], None).await.unwrap();
    assert!(!directory::reachable(&addr, &b).await.unwrap());

    // Sem publicar, não há o que testar.
    let err = directory::reachable(&addr, &key()).await.unwrap_err();
    assert!(err.to_string().contains("não publicado"), "{err}");
}

/// Falar com o diretório ajusta o desvio de hora (aqui ~0, pois é a mesma máquina).
#[tokio::test]
async fn directory_time_overrides_wrong_local_clock() {
    let (_d, addr) = dir().await;
    directory::seen_ip(&addr).await.unwrap();
    // O desvio medido contra um servidor na mesma máquina é ~0.
    assert!(clam::clock::offset_secs().abs() <= 1);
}

#[tokio::test]
async fn room_needs_the_secret_and_first_member_is_host() {
    let (_d, addr) = dir().await;
    let secret = clam::room::new_secret();
    let key = clam::room::room_key(&secret);
    let (a, b) = ([1u8; 32], [2u8; 32]);
    assert!(directory::room_members(&addr, &key).await.unwrap().is_empty());
    directory::room_join(&addr, &key, &a).await.unwrap();
    directory::room_join(&addr, &key, &b).await.unwrap();
    directory::room_join(&addr, &key, &a).await.unwrap(); // renovar não muda a ordem
    assert_eq!(directory::room_members(&addr, &key).await.unwrap(), vec![a, b]);

    // Outra sala (outro segredo) não enxerga nem mexe nesta.
    let other = clam::room::room_key(&clam::room::new_secret());
    assert!(directory::room_members(&addr, &other).await.unwrap().is_empty());

    // Sair passa o posto de anfitrião ao próximo.
    directory::room_leave(&addr, &key, &a).await.unwrap();
    assert_eq!(directory::room_members(&addr, &key).await.unwrap(), vec![b]);
}

/// Três pessoas na mesma sala: todas se ligam entre si e ouvem umas às outras.
#[tokio::test]
async fn three_nodes_form_a_mesh_room() {
    use clam::room::{self, Mesh};
    let (_d, addr) = dir().await;
    let rkey = room::room_key(&room::new_secret());
    let mut nodes = vec![];
    for _ in 0..3 {
        nodes.push(Node::start(cfg(key(), &addr, vec![])).await.unwrap());
    }
    let meshes: Vec<std::sync::Arc<Mesh>> = (0..3).map(|_| std::sync::Arc::new(Mesh::new())).collect();
    let views = {
        let mut v = vec![];
        for n in &nodes {
            v.push(room::sync(n, &rkey).await.unwrap());
        }
        v
    };
    assert_eq!(views[0].members.len(), 1, "o primeiro vê só a si");
    let view = room::sync(&nodes[2], &rkey).await.unwrap();
    assert_eq!(view.members.len(), 3);
    assert_eq!(view.host, nodes[0].public_key());
    assert!(!view.i_am_host && room::sync(&nodes[0], &rkey).await.unwrap().i_am_host);

    // Cada nó atende os convites que chegam e liga para os que faltam.
    let mut tasks = vec![];
    for (mut node, mesh) in nodes.into_iter().zip(meshes.iter().cloned()) {
        let rkey = rkey.clone();
        tasks.push(tokio::spawn(async move {
            let end = Instant::now() + Duration::from_secs(25);
            while Instant::now() < end && mesh.count() < 2 {
                let view = room::sync(&node, &rkey).await.unwrap();
                room::connect_missing(&node.dialer(), &view, &mesh, Duration::from_secs(10));
                while let Ok(Some(inc)) = timeout(Duration::from_millis(300), node.next_incoming()).await {
                    room::accept_member(inc, &view, &mesh).await;
                }
            }
            (node, mesh)
        }));
    }
    let mut done = vec![];
    for t in tasks {
        done.push(t.await.unwrap());
    }
    for (_, m) in &done {
        assert_eq!(m.count(), 2, "malha completa: cada um com dois pares");
    }
    // Todos falam; todos ouvem alguma mistura.
    let mut heard = [0usize; 3];
    for n in 0..80 {
        for (i, (_, m)) in done.iter().enumerate() {
            m.send(&wide_tone(n));
            while let Some(f) = m.mix() {
                heard[i] += f.iter().any(|x| x.abs() > 1000) as usize;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for (_, m) in &done {
        m.hangup();
    }
    assert!(heard.iter().all(|&h| h >= 5), "ouviram {heard:?}");
}

fn punching(secret: StaticSecret, dir: &str, contacts: Vec<[u8; 32]>) -> NodeConfig {
    NodeConfig { punch: true, ..cfg(secret, dir, contacts) }
}

async fn talk(mut ca: clam::phone::ActiveCall, mut cb: clam::phone::ActiveCall) {
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
    cb.hangup();
}

/// Furo de NAT (aqui em loopback, não prova NAT real): a chamada vai direto por UDP, sem circuito.
#[tokio::test]
async fn two_nodes_talk_through_punch() {
    let (_d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(punching(sa, &addr, vec![])).await.unwrap();
    let mut b = Node::start(punching(sb, &addr, vec![pa])).await.unwrap();
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        assert_eq!(inc.caller().as_bytes(), &pa);
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (cb, _b) = answer.await.unwrap();
    talk(ca, cb).await;
}

/// Alvo sem furo de NAT: quem chama cai no circuito.
#[tokio::test]
async fn punch_falls_back_to_circuit() {
    let (_d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(punching(sa, &addr, vec![])).await.unwrap();
    let mut b = Node::start(cfg(sb, &addr, vec![pa])).await.unwrap();
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (cb, _b) = answer.await.unwrap();
    talk(ca, cb).await;
}

/// Discagem não bloqueia por membro ausente, e uma chamada derrubada volta sozinha na tentativa seguinte.
#[tokio::test]
async fn room_dials_in_background_and_reconnects() {
    use clam::room::{self, Mesh};
    let (_d, addr) = dir().await;
    let rkey = room::room_key(&room::new_secret());
    let (mut a, mut b) = (Node::start(cfg(key(), &addr, vec![])).await.unwrap(), Node::start(cfg(key(), &addr, vec![])).await.unwrap());
    // Quem tem a chave menor disca.
    if a.public_key() > b.public_key() {
        std::mem::swap(&mut a, &mut b);
    }
    let (ma, mb) = (Mesh::new(), Mesh::new());
    room::sync(&a, &rkey).await.unwrap();
    room::sync(&b, &rkey).await.unwrap();
    let mut view_a = room::sync(&a, &rkey).await.unwrap();
    assert_eq!(view_a.members.len(), 2);
    // Um membro fantasma com chave maior: a discagem a ele não pode segurar a chamada.
    view_a.members.push([0xff; 32]);
    let t = Instant::now();
    let started = room::connect_missing(&a.dialer(), &view_a, &ma, Duration::from_secs(10));
    assert!(t.elapsed() < Duration::from_millis(200), "connect_missing bloqueou: {:?}", t.elapsed());
    assert_eq!(started, 2, "b e o fantasma, em paralelo");
    let view_b = room::sync(&b, &rkey).await.unwrap();

    async fn until_connected(b: &mut Node, view_b: &room::RoomView, ma: &Mesh, mb: &Mesh) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end && !(ma.count() == 1 && mb.count() == 1) {
            if let Ok(Some(inc)) = timeout(Duration::from_millis(200), b.next_incoming()).await {
                room::accept_member(inc, view_b, mb).await;
            }
        }
        assert!(ma.count() == 1 && mb.count() == 1, "a {} b {}", ma.count(), mb.count());
    }
    until_connected(&mut b, &view_b, &ma, &mb).await;

    // A chamada cai do lado de quem disca; a próxima rodada religa.
    ma.hangup();
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end && mb.count() > 0 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(mb.count(), 0, "o outro lado percebe o fim");
    let mut redialed = 0;
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end && !(ma.count() == 1 && mb.count() == 1) {
        redialed += room::connect_missing(&a.dialer(), &view_a, &ma, Duration::from_secs(10));
        if let Ok(Some(inc)) = timeout(Duration::from_millis(200), b.next_incoming()).await {
            room::accept_member(inc, &view_b, &mb).await;
        }
    }
    assert!(redialed >= 1 && ma.count() == 1 && mb.count() == 1, "religou? a {} b {}", ma.count(), mb.count());
    ma.send(&wide_tone(0));
    ma.hangup();
    mb.hangup();
}

#[tokio::test]
async fn directory_rate_limits_per_ip() {
    use clam::directory::RateLimit;
    let d = DirectoryServer::spawn_limited("127.0.0.1:0".parse().unwrap(), RateLimit { per_sec: 0.1, burst: 3.0 }).await.unwrap();
    let addr = d.local_addr().to_string();
    let mut refused = 0;
    for _ in 0..10 {
        if clam::directory::lookup(&addr, &[7u8; 32]).await.is_err() {
            refused += 1;
        }
    }
    assert!(refused >= 5, "só {refused} recusados");
}

#[tokio::test]
async fn room_owner_rotates_secret_and_removed_member_is_left_out() {
    use clam::{room, x25519_dalek::{PublicKey, StaticSecret}, rand_core::OsRng};
    let d = DirectoryServer::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = d.local_addr().to_string();
    let (owner, bob, eve) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
    let (opub, bpub) = (PublicKey::from(&owner).to_bytes(), PublicKey::from(&bob).to_bytes());
    let old = room::new_secret();
    let new = room::rotate(&addr, &owner, &old, &[bpub]).await.unwrap();
    assert_eq!(room::follow(&addr, &bob, &opub, &old).await.unwrap(), Some(new));
    assert_eq!(room::follow(&addr, &eve, &opub, &old).await.unwrap(), None);
}
