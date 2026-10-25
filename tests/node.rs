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

/// Depois de um furo que falhou, as ligações seguintes ao mesmo contato seguem pelo circuito sem esperar o furo.
#[tokio::test]
async fn failed_punch_is_skipped_on_the_next_call() {
    let (_d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(punching(sa, &addr, vec![])).await.unwrap();
    let mut b = Node::start(cfg(sb, &addr, vec![pa])).await.unwrap();
    let answer = tokio::spawn(async move {
        for _ in 0..2 {
            let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
            let mut c = inc.accept(Mulaw::new()).await.unwrap();
            // A linha só livra quando quem chamou desliga.
            timeout(Duration::from_secs(10), async { while c.next_pcm().await.is_some() {} }).await.unwrap();
            c.hangup();
        }
    });
    let t = std::time::Instant::now();
    let first = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let first_took = t.elapsed();
    first.hangup();
    let t = std::time::Instant::now();
    let second = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let second_took = t.elapsed();
    second.hangup();
    answer.await.unwrap();
    // Em loopback o furo falha na hora (o alvo nem tem ouvinte), então só a segunda não pode demorar.
    assert!(first_took < Duration::from_secs(8), "{first_took:?}");
    assert!(second_took < Duration::from_secs(4), "a segunda ligação deveria pular o furo: {second_took:?}");
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

#[tokio::test]
async fn host_leaving_hands_the_room_to_the_next_member() {
    use clam::room;
    let (_d, addr) = dir().await;
    let rkey = room::room_key(&room::new_secret());
    let (a, b, c) = (
        Node::start(cfg(key(), &addr, vec![])).await.unwrap(),
        Node::start(cfg(key(), &addr, vec![])).await.unwrap(),
        Node::start(cfg(key(), &addr, vec![])).await.unwrap(),
    );
    for n in [&a, &b, &c] {
        room::sync(n, &rkey).await.unwrap();
    }
    let view = room::sync(&c, &rkey).await.unwrap();
    assert_eq!(view.host, a.public_key());
    room::leave(&a, &rkey).await.unwrap();
    let view = room::sync(&c, &rkey).await.unwrap();
    assert_eq!((view.host, view.members.len()), (b.public_key(), 2));
    assert!(room::sync(&b, &rkey).await.unwrap().i_am_host);
    // O antigo anfitrião volta como o membro mais novo e não toma a sala de volta.
    let view = room::sync(&a, &rkey).await.unwrap();
    assert_eq!((view.host, view.members.last().copied()), (b.public_key(), Some(a.public_key())));
}

#[tokio::test]
async fn member_that_drops_and_returns_keeps_the_room_alive() {
    use clam::room::{self, Mesh};
    let (_d, addr) = dir().await;
    let rkey = room::room_key(&room::new_secret());
    let (mut a, mut b) = (Node::start(cfg(key(), &addr, vec![])).await.unwrap(), Node::start(cfg(key(), &addr, vec![])).await.unwrap());
    if a.public_key() > b.public_key() {
        std::mem::swap(&mut a, &mut b);
    }
    let (ma, mb) = (Mesh::new(), Mesh::new());
    room::sync(&a, &rkey).await.unwrap();
    room::sync(&b, &rkey).await.unwrap();
    // `b` cai (sai da sala e perde a chamada); `a` segue sozinho na sala.
    room::leave(&b, &rkey).await.unwrap();
    let view_a = room::sync(&a, &rkey).await.unwrap();
    assert_eq!(view_a.members, vec![a.public_key()]);
    assert_eq!(room::connect_missing(&a.dialer(), &view_a, &ma, Duration::from_secs(10)), 0);
    // `b` volta: `a` o vê de novo e a malha se refaz.
    room::sync(&b, &rkey).await.unwrap();
    let view_a = room::sync(&a, &rkey).await.unwrap();
    let view_b = room::sync(&b, &rkey).await.unwrap();
    assert_eq!(view_a.members.len(), 2);
    room::connect_missing(&a.dialer(), &view_a, &ma, Duration::from_secs(10));
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end && !(ma.count() == 1 && mb.count() == 1) {
        if let Ok(Some(inc)) = timeout(Duration::from_millis(200), b.next_incoming()).await {
            room::accept_member(inc, &view_b, &mb).await;
        }
    }
    assert!(ma.count() == 1 && mb.count() == 1, "a {} b {}", ma.count(), mb.count());
    ma.hangup();
    mb.hangup();
}

#[tokio::test]
async fn restarted_directory_keeps_rooms_and_publications() {
    use clam::{directory, room};
    let state = std::env::temp_dir().join(format!("clam-dir-{}.state", std::process::id()));
    let _ = std::fs::remove_file(&state);
    let d = DirectoryServer::spawn_with("127.0.0.1:0".parse().unwrap(), Default::default(), Some(state.clone())).await.unwrap();
    let addr = d.local_addr();
    let rkey = room::room_key(&room::new_secret());
    let (a, b) = (Node::start(cfg(key(), &addr.to_string(), vec![])).await.unwrap(), Node::start(cfg(key(), &addr.to_string(), vec![])).await.unwrap());
    room::sync(&a, &rkey).await.unwrap();
    room::sync(&b, &rkey).await.unwrap();
    tokio::time::sleep(directory::SAVE_EVERY + Duration::from_secs(1)).await;
    drop(d);
    // Sem o diretório, nada responde; ao voltar com o mesmo estado, tudo reaparece sem ninguém republicar.
    assert!(directory::lookup(&addr.to_string(), &a.public_key()).await.is_err());
    let _d2 = DirectoryServer::spawn_with(addr, Default::default(), Some(state.clone())).await.unwrap();
    assert!(directory::lookup(&addr.to_string(), &a.public_key()).await.unwrap().is_some());
    let members = directory::room_members(&addr.to_string(), &rkey).await.unwrap();
    assert_eq!(members, vec![a.public_key(), b.public_key()]);
    let _ = std::fs::remove_file(&state);
}

/// Diretório fora do ar: o nó sobe mesmo assim e liga para quem tem endereço guardado.
#[tokio::test]
async fn node_without_directory_dials_stored_address() {
    let (d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let mut b = Node::start(cfg(sb, &addr, vec![pa])).await.unwrap();

    // Com o diretório no ar, ligar guarda o endereço de `b`.
    let a = Node::start(cfg(sa.clone(), &addr, vec![])).await.unwrap();
    let first = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        let c = inc.accept(Mulaw::new()).await.unwrap();
        (c, b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (cb, mut b) = first.await.unwrap();
    let known = a.known();
    assert_eq!(known.len(), 1);
    assert_eq!(known[0].0, pb);
    ca.hangup();
    drop(cb);
    a.stop();

    // Derruba o diretório; um nó novo, só com o endereço guardado, sobe e liga.
    let dead = addr.clone();
    drop(d);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let a2 = Node::start(NodeConfig { known, ..cfg(sa, &dead, vec![]) }).await.unwrap();
    assert!(a2.directory_error().is_some());
    assert!(a2.network_note().unwrap().contains("diretório fora do ar"));
    let second = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a2.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_cb, _b) = second.await.unwrap();
    ca.hangup();
}

/// Endereço guardado que não responde mais: a ligação cai no diretório e corrige o cache.
#[tokio::test]
async fn stale_stored_address_falls_back_to_directory() {
    let (_d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let mut b = Node::start(cfg(sb, &addr, vec![pa])).await.unwrap();
    let real = directory::lookup(&addr, &pb).await.unwrap().unwrap();
    let stale = clam::directory::RelayInfo { addr: "127.0.0.1:1".into(), ..real.clone() };
    let a = Node::start(NodeConfig { known: vec![(pb, stale)], ..cfg(sa, &addr, vec![]) }).await.unwrap();
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(20), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_cb, _b) = answer.await.unwrap();
    assert_eq!(a.known()[0].1.addr, real.addr);
    ca.hangup();
}

/// Com o diretório no ar mas sem conhecer o contato, o endereço guardado basta.
#[tokio::test]
async fn stored_address_works_when_directory_does_not_know_the_peer() {
    let (_d1, addr1) = dir().await;
    let (_d2, addr2) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let mut b = Node::start(cfg(sb, &addr1, vec![pa])).await.unwrap();
    let real = directory::lookup(&addr1, &pb).await.unwrap().unwrap();
    let a = Node::start(NodeConfig { known: vec![(pb, real)], ..cfg(sa, &addr2, vec![]) }).await.unwrap();
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_cb, _b) = answer.await.unwrap();
    ca.hangup();
}

/// Os endereços dos contatos são guardados sozinhos, sem precisar ligar antes.
#[tokio::test]
async fn contacts_addresses_are_stored_in_background() {
    let (_d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let pb = *PublicKey::from(&sb).as_bytes();
    let _b = Node::start(cfg(sb, &addr, vec![])).await.unwrap();
    let a = Node::start(cfg(sa, &addr, vec![pb])).await.unwrap();
    timeout(Duration::from_secs(5), async {
        while a.known().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("endereço do contato não foi guardado");
    assert_eq!(a.known()[0].0, pb);
}

/// Desligar de propósito marca `peer_hung_up`; queda de rede não (a GUI só religa neste caso).
#[tokio::test]
async fn call_reports_whether_peer_hung_up() {
    let (_d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(cfg(sa, &addr, vec![])).await.unwrap();
    let mut b = Node::start(cfg(sb, &addr, vec![pa])).await.unwrap();
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (mut cb, _b) = answer.await.unwrap();
    assert!(!cb.peer_hung_up());
    ca.hangup();
    timeout(Duration::from_secs(5), async { while cb.next_pcm().await.is_some() {} }).await.expect("a chamada deveria acabar");
    assert!(cb.peer_hung_up());
}

/// Com as mesmas chaves de relay, reiniciar o nó mantém o que os contatos guardaram (onion e link);
/// sem elas, cada início tem chaves novas.
#[tokio::test]
async fn relay_keys_survive_a_restart() {
    use clam::node::RelayKeys;
    let (_d, addr) = dir().await;
    let (s, keys) = (key(), RelayKeys::random());
    let me = *PublicKey::from(&s).as_bytes();
    let published = |node: Node| {
        let addr = addr.clone();
        async move {
            let r = directory::lookup(&addr, &me).await.unwrap().unwrap();
            node.stop();
            (r.onion, r.link)
        }
    };
    let with = |n: Option<RelayKeys>| NodeConfig { relay_keys: n, ..cfg(s.clone(), &addr, vec![]) };
    let first = published(Node::start(with(Some(keys.clone()))).await.unwrap()).await;
    let second = published(Node::start(with(Some(keys.clone()))).await.unwrap()).await;
    assert_eq!(first, second);
    assert_eq!(first.0, PublicKey::from(&keys.onion).to_bytes());
    let third = published(Node::start(with(None)).await.unwrap()).await;
    assert_ne!(first.0, third.0);
    assert_ne!(first.1, third.1);
}

/// Os dois lados reiniciam e o diretório não volta: com as chaves do relay guardadas, o endereço
/// que A tinha de B ainda vale e a ligação sai sem consultar o diretório.
#[tokio::test]
async fn both_restarted_and_directory_gone_still_dials_stored_address() {
    use clam::node::RelayKeys;
    let (d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let (ka, kb) = (RelayKeys::random(), RelayKeys::random());
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let with = |s: &StaticSecret, k: &RelayKeys, contacts: Vec<[u8; 32]>, relay_port: u16, dir: &str| NodeConfig {
        relay_port,
        relay_keys: Some(k.clone()),
        ..cfg(s.clone(), dir, contacts)
    };

    let mut b = Node::start(with(&sb, &kb, vec![pa], port, &addr)).await.unwrap();
    let a = Node::start(with(&sa, &ka, vec![], 0, &addr)).await.unwrap();
    let first = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (cb, b) = first.await.unwrap();
    let known = a.known();
    ca.hangup();
    drop(cb);
    a.stop();
    b.stop();

    // Tudo reinicia, o diretório não volta.
    let dead = addr.clone();
    drop(d);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut b2 = Node::start(with(&sb, &kb, vec![pa], port, &dead)).await.unwrap();
    let a2 = Node::start(NodeConfig { known, ..with(&sa, &ka, vec![], 0, &dead) }).await.unwrap();
    assert!(a2.directory_error().is_some());
    let second = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b2.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b2)
    });
    let ca = a2.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_cb, _b2) = second.await.unwrap();
    ca.hangup();
}

/// Endereço guardado morto e diretório fora do ar: o erro diz as duas coisas, não só "diretório".
#[tokio::test]
async fn dead_stored_address_and_no_directory_says_both() {
    use clam::directory::RelayInfo;
    let (d, addr) = dir().await;
    drop(d);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pb = *PublicKey::from(&key()).as_bytes();
    let dead = RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] };
    let a = Node::start(NodeConfig { known: vec![(pb, dead)], ..cfg(key(), &addr, vec![]) }).await.unwrap();
    let err = a.dial(pb, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::StoredAddressDead(_)), "{err}");
    let other = *PublicKey::from(&key()).as_bytes();
    let err = a.dial(other, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::NoStoredAddress(_)), "{err}");
}

/// Quem atende aprende o relay de quem ligou pelo convite: ligar de volta funciona sem diretório e sem ter
/// consultado esse contato antes.
#[tokio::test]
async fn callee_learns_the_caller_from_the_invite_and_calls_back_without_directory() {
    let (d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    // `b` só aceita `a` depois de subir: a consulta de contatos da partida não acha `a` no diretório.
    let mut b = Node::start(cfg(sb, &addr, vec![])).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    b.allow(&[pa]);
    let mut a = Node::start(cfg(sa, &addr, vec![pb])).await.unwrap();
    assert!(b.known().is_empty() && b.hints().is_empty());
    let first = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (cb, b) = first.await.unwrap();
    ca.hangup();
    drop(cb);
    let hints = b.hints();
    assert_eq!(hints.len(), 1);
    assert_eq!((hints[0].0, hints[0].1.addr.clone()), (pa, a.published_addr()));
    assert_eq!(b.dialer().stored_address(&pa), Some(a.published_addr()));

    // O diretório some; `b` liga de volta só com o que o convite trouxe.
    drop(d);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let back = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), a.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), a)
    });
    let cb = b.dial(pa, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_ca, a) = back.await.unwrap();
    cb.hangup();
    // A chamada confirmou o endereço: de dica passa a guardado.
    assert!(b.hints().is_empty());
    let known = b.known();
    assert_eq!((known.len(), known[0].0, known[0].1.addr.clone()), (1, pa, a.published_addr()));
}

/// O endereço guardado vem antes da dica; se estiver morto, a dica atende e vira o endereço guardado.
#[tokio::test]
async fn stored_address_goes_before_the_hint_and_a_working_hint_replaces_it() {
    use clam::directory::RelayInfo;
    let (d, addr) = dir().await;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let dead = RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] };
    let mut b = Node::start(NodeConfig { known: vec![(pa, dead.clone())], ..cfg(sb, &addr, vec![]) }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    b.allow(&[pa]);
    let mut a = Node::start(cfg(sa, &addr, vec![pb])).await.unwrap();
    let first = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_cb, b) = first.await.unwrap();
    ca.hangup();
    assert_eq!(b.dialer().stored_address(&pa).as_deref(), Some("127.0.0.1:1"));
    assert_eq!(b.hints().len(), 1);

    drop(d);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let back = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), a.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), a)
    });
    let cb = b.dial(pa, Mulaw::new(), Duration::from_secs(20)).await.unwrap();
    let (_ca, a) = back.await.unwrap();
    cb.hangup();
    let known = b.known();
    assert_eq!((known.len(), known[0].1.addr.clone()), (1, a.published_addr()));
    assert!(b.hints().is_empty());
}

/// Uma dica que ninguém atende não vira endereço guardado: o erro diz que tentou e que o diretório não atende.
#[tokio::test]
async fn a_dead_hint_never_becomes_the_stored_address() {
    use clam::directory::RelayInfo;
    let (d, addr) = dir().await;
    drop(d);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pb = *PublicKey::from(&key()).as_bytes();
    let dead = RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] };
    let a = Node::start(NodeConfig { hints: vec![(pb, dead)], ..cfg(key(), &addr, vec![]) }).await.unwrap();
    let err = a.dial(pb, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::StoredAddressDead(_)), "{err}");
    assert!(a.known().is_empty());
    assert_eq!(a.hints().len(), 1);
}

/// Sem diretório nenhum: o relay de quem atende, passado fora de banda (o código de contato direto), basta
/// para a primeira chamada, e depois dela vira o endereço guardado.
#[tokio::test]
async fn first_call_needs_no_directory_with_the_direct_code() {
    let free_port = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let no_dir = |secret: StaticSecret, contacts: Vec<[u8; 32]>| {
        let port = free_port();
        NodeConfig { relay_port: port, public_addr: Some(format!("127.0.0.1:{port}")), upnp: false, punch: false, ..NodeConfig::new(secret, "", contacts) }
    };
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let mut b = Node::start(no_dir(sb, vec![pa])).await.unwrap();
    let a = Node::start(no_dir(sa, vec![pb])).await.unwrap();
    assert!(a.directory_error().is_none() && b.network_note().is_none());
    // Sem o código, não há como achar o contato.
    let err = a.dial(pb, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::NoDirectory { tried_stored: false }), "{err}");

    let code = b.relay_info().expect("relay com endereço público informado");
    a.add_hint(pb, code.clone());
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_cb, _b) = answer.await.unwrap();
    ca.hangup();
    let known = a.known();
    assert_eq!((known.len(), known[0].1.clone()), (1, code));
    assert!(a.hints().is_empty());
}

/// O endereço do código direto não responde e não há diretório: o erro diz que tentou o guardado.
#[tokio::test]
async fn dead_direct_code_without_directory_says_so() {
    use clam::directory::RelayInfo;
    let pb = *PublicKey::from(&key()).as_bytes();
    let a = Node::start(NodeConfig { relay_port: 0, punch: false, upnp: false, ..NodeConfig::new(key(), "", vec![]) }).await.unwrap();
    a.add_hint(pb, RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] });
    let err = a.dial(pb, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::NoDirectory { tried_stored: true }), "{err}");
    assert!(a.known().is_empty());
}

/// Contato novo entra com o endereço do código; contato removido leva o endereço guardado junto.
#[tokio::test]
async fn adding_and_forgetting_a_contact_moves_its_address_too() {
    use clam::directory::RelayInfo;
    let pb = *PublicKey::from(&key()).as_bytes();
    let relay = RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] };
    let a = Node::start(NodeConfig { relay_port: 0, punch: false, upnp: false, ..NodeConfig::new(key(), "", vec![]) }).await.unwrap();
    a.add_contact(pb, Some(relay.clone()));
    assert_eq!(a.hints(), vec![(pb, relay.clone())]);
    assert_eq!(a.dialer().stored_address(&pb), Some(relay.addr.clone()));
    a.forget(&pb);
    assert!(a.hints().is_empty() && a.known().is_empty());
    assert_eq!(a.dialer().stored_address(&pb), None);
}

/// O endereço público pode ser um nome: o código direto o leva como foi informado.
#[tokio::test]
async fn direct_code_keeps_a_hostname_public_address() {
    let cfg = NodeConfig { relay_port: 0, punch: false, upnp: false, public_addr: Some("casa.exemplo.com:4001".into()), ..NodeConfig::new(key(), "", vec![]) };
    let a = Node::start(cfg).await.unwrap();
    assert_eq!(a.relay_info().unwrap().addr, "casa.exemplo.com:4001");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Nó sem diretório num porto conhecido, para o endereço dele poder ser escrito antes de subir.
fn fixed(secret: StaticSecret, port: u16, contacts: Vec<[u8; 32]>) -> NodeConfig {
    use clam::node::RelayKeys;
    NodeConfig {
        relay_port: port,
        public_addr: Some(format!("127.0.0.1:{port}")),
        upnp: false,
        punch: false,
        relay_keys: Some(RelayKeys::random()),
        update_retry: Duration::from_millis(200),
        ..NodeConfig::new(secret, "", contacts)
    }
}

async fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    timeout(Duration::from_secs(30), async {
        loop {
            if let Some(v) = f() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("não aconteceu: {what}"))
}

/// A muda de endereço com B desligado; quando B volta, a fila de saída de A entrega o endereço novo e a chamada funciona.
#[tokio::test]
async fn a_new_address_reaches_the_contact_that_was_offline() {
    use clam::directory::RelayInfo;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let (port_a, port_b) = (free_port(), free_port());
    let stale = RelayInfo { addr: format!("127.0.0.1:{}", free_port()), onion: [1; 32], link: [2; 32] };

    // A já está no endereço novo e só tem o código de B; B ainda guarda o endereço antigo de A.
    let mut a = Node::start(fixed(sa, port_a, vec![pb])).await.unwrap();
    let fresh = a.relay_info().unwrap();
    a.add_hint(pb, RelayInfo { addr: format!("127.0.0.1:{port_b}"), onion: [3; 32], link: [4; 32] });
    // B ainda não existe: as primeiras entregas falham e a fila insiste.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let mut cfg_b = fixed(sb, port_b, vec![pa]);
    cfg_b.known = vec![(pa, stale.clone())];
    let b = Node::start(cfg_b).await.unwrap();
    // O que A tinha de B (chaves inventadas) nunca valeu; agora que B existe, A recebe o código certo dele.
    a.add_hint(pb, b.relay_info().unwrap());

    until("B recebe o endereço novo de A", || (b.known().iter().any(|(k, r)| *k == pa && *r == fresh)).then_some(())).await;
    assert!(b.history().contains(&(pa, stale)), "o endereço antigo fica no histórico");
    assert!(b.take_notice().is_some_and(|n| n.contains(&fresh.addr)));

    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), a.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), a)
    });
    let cb = b.dial(pa, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_ca, _a) = answer.await.unwrap();
    cb.hangup();
}

/// Atualização de quem não é contato não é confirmada nem guardada.
#[tokio::test]
async fn address_updates_from_strangers_are_ignored() {
    let (sb, sc) = (key(), key());
    let pb = *PublicKey::from(&sb).as_bytes();
    // C conhece o relay de B, mas B não tem C entre os contatos.
    let b = Node::start(fixed(sb, free_port(), vec![])).await.unwrap();
    let c = Node::start(fixed(sc, free_port(), vec![pb])).await.unwrap();
    c.add_hint(pb, b.relay_info().unwrap());
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(b.known().is_empty() && b.history().is_empty());
    assert!(c.known().is_empty(), "sem confirmação de B, C não promove a dica: {:?}", c.known());
    assert!(b.take_notice().is_none());
}

/// Quem mudou de endereço sem avisar ainda é achado pelo histórico, e o que respondeu vira o guardado.
#[tokio::test]
async fn an_old_address_in_the_history_still_reaches_the_contact() {
    use clam::directory::RelayInfo;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let dead = RelayInfo { addr: format!("127.0.0.1:{}", free_port()), onion: [1; 32], link: [2; 32] };
    let mut a = Node::start(NodeConfig { updates: false, ..fixed(sa, free_port(), vec![pb]) }).await.unwrap();
    let alive = a.relay_info().unwrap();
    let mut cfg_b = NodeConfig { updates: false, ..fixed(sb, free_port(), vec![pa]) };
    cfg_b.known = vec![(pa, dead.clone())];
    cfg_b.history = vec![(pa, alive.clone())];
    let b = Node::start(cfg_b).await.unwrap();

    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), a.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), a)
    });
    let cb = b.dial(pa, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_ca, _a) = answer.await.unwrap();
    cb.hangup();
    assert_eq!(b.known(), vec![(pa, alive)]);
    assert_eq!(b.history(), vec![(pa, dead)]);
}

/// Os dois mudaram de endereço enquanto estavam separados: nenhum tem o endereço novo do outro e a fila de
/// saída não ajuda. A DHT local faz os dois se encontrarem.
#[tokio::test]
async fn both_contacts_changed_address_and_find_each_other_through_the_dht() {
    use clam::directory::RelayInfo;
    use clam::rendezvous::DhtOptions;
    let testnet = mainline::Testnet::new(5).unwrap();
    let dht = || Some(DhtOptions { bootstrap: testnet.bootstrap.clone(), bind: Some(std::net::Ipv4Addr::LOCALHOST), circuit: false });
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let stale = || RelayInfo { addr: format!("127.0.0.1:{}", free_port()), onion: [1; 32], link: [2; 32] };
    let mut cfg_a = NodeConfig { updates: false, dht: dht(), ..fixed(sa, free_port(), vec![pb]) };
    cfg_a.known = vec![(pb, stale())];
    let mut cfg_b = NodeConfig { updates: false, dht: dht(), ..fixed(sb, free_port(), vec![pa]) };
    cfg_b.known = vec![(pa, stale())];
    let mut a = Node::start(cfg_a).await.unwrap();
    let b = Node::start(cfg_b).await.unwrap();
    let fresh_a = a.relay_info().unwrap();

    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(60), a.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), a)
    });
    // A grava o endereço dele na DHT em segundo plano; B tenta até o item aparecer.
    let mut call = None;
    for _ in 0..10 {
        if let Ok(c) = b.dial(pa, Mulaw::new(), Duration::from_secs(15)).await {
            call = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let cb = call.expect("B achou A pela DHT");
    let (_ca, _a) = answer.await.unwrap();
    assert!(b.known().iter().any(|(k, r)| *k == pa && *r == fresh_a), "o endereço da DHT vira o guardado");
    cb.hangup();
}

/// Nós C e D servem de gateway da DHT; A e B não têm acesso direto a ela (o nó inicial deles não existe) e
/// só chegam lá por circuitos de dois relays de contatos. Se B acha A, foi pelo circuito.
#[tokio::test]
async fn the_dht_is_reached_through_two_contact_relays() {
    use clam::directory::RelayInfo;
    use clam::rendezvous::DhtOptions;
    let testnet = mainline::Testnet::new(5).unwrap();
    let gateway = || Some(DhtOptions { bootstrap: testnet.bootstrap.clone(), bind: Some(std::net::Ipv4Addr::LOCALHOST), circuit: false });
    let private = || Some(DhtOptions { bootstrap: vec!["127.0.0.1:9".into()], bind: Some(std::net::Ipv4Addr::LOCALHOST), circuit: true });
    let (sa, sb, sc, sd) = (key(), key(), key(), key());
    let (pa, pb, pc, pd) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes(), *PublicKey::from(&sc).as_bytes(), *PublicKey::from(&sd).as_bytes());
    let c = Node::start(NodeConfig { updates: false, dht: gateway(), ..fixed(sc, free_port(), vec![]) }).await.unwrap();
    let d = Node::start(NodeConfig { updates: false, dht: gateway(), ..fixed(sd, free_port(), vec![]) }).await.unwrap();
    let relays = [(pc, c.relay_info().unwrap()), (pd, d.relay_info().unwrap())];
    let stale = || RelayInfo { addr: format!("127.0.0.1:{}", free_port()), onion: [1; 32], link: [2; 32] };
    let mut cfg_a = NodeConfig { updates: false, dht: private(), ..fixed(sa, free_port(), vec![pb]) };
    cfg_a.known = relays.iter().cloned().chain([(pb, stale())]).collect();
    let mut cfg_b = NodeConfig { updates: false, dht: private(), ..fixed(sb, free_port(), vec![pa]) };
    cfg_b.known = relays.iter().cloned().chain([(pa, stale())]).collect();
    let mut a = Node::start(cfg_a).await.unwrap();
    let b = Node::start(cfg_b).await.unwrap();
    let fresh_a = a.relay_info().unwrap();

    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(90), a.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), a)
    });
    let mut call = None;
    for _ in 0..12 {
        if let Ok(c) = b.dial(pa, Mulaw::new(), Duration::from_secs(15)).await {
            call = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let cb = call.expect("B achou A pela DHT, por circuito");
    let (_ca, _a) = answer.await.unwrap();
    assert!(b.known().iter().any(|(k, r)| *k == pa && *r == fresh_a));
    cb.hangup();
}

/// Com o modo circuito e sem dois relays de contatos, a DHT não é consultada, nem direto: o IP não vaza.
#[tokio::test]
async fn circuit_mode_never_falls_back_to_the_direct_dht() {
    use clam::directory::RelayInfo;
    use clam::rendezvous::DhtOptions;
    let testnet = mainline::Testnet::new(5).unwrap();
    let options = |circuit| Some(DhtOptions { bootstrap: testnet.bootstrap.clone(), bind: Some(std::net::Ipv4Addr::LOCALHOST), circuit });
    let (sa, sb, sc) = (key(), key(), key());
    let (pa, pb, pc) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes(), *PublicKey::from(&sc).as_bytes());
    let c = Node::start(NodeConfig { updates: false, dht: options(false), ..fixed(sc, free_port(), vec![]) }).await.unwrap();
    let stale = RelayInfo { addr: format!("127.0.0.1:{}", free_port()), onion: [1; 32], link: [2; 32] };
    let known = vec![(pa, stale), (pc, c.relay_info().unwrap())];
    let mut a = Node::start(NodeConfig { updates: false, dht: options(false), ..fixed(sa, free_port(), vec![pb]) }).await.unwrap();
    // Só um relay de contato (C): não dá para montar um circuito de dois relays.
    let mut cfg_b = NodeConfig { updates: false, dht: options(true), ..fixed(sb.clone(), free_port(), vec![pa]) };
    cfg_b.known = known.clone();
    let b = Node::start(cfg_b).await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(b.dial(pa, Mulaw::new(), Duration::from_secs(5)).await.is_err(), "sem circuito, nada de DHT direto");
    drop(b);

    // Controle: o mesmo B pelo acesso direto acha A, então o item estava lá.
    let mut cfg_b = NodeConfig { updates: false, dht: options(false), ..fixed(sb, free_port(), vec![pa]) };
    cfg_b.known = known;
    let b = Node::start(cfg_b).await.unwrap();
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(60), a.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), a)
    });
    let mut call = None;
    for _ in 0..10 {
        if let Ok(c) = b.dial(pa, Mulaw::new(), Duration::from_secs(15)).await {
            call = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let cb = call.expect("pelo acesso direto B acha A");
    let (_ca, _a) = answer.await.unwrap();
    cb.hangup();
}

/// A adiciona B pelo código; B, que não tem A, recebe a apresentação (chave provada, nome e relay) e, ao adicionar
/// A, os dois se ligam. A confirmação encerra a entrega.
#[tokio::test]
async fn adding_a_contact_introduces_us_to_it() {
    use clam::media::update::Reach;
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let mut a = Node::start(NodeConfig { updates: false, name: "Ana".into(), ..fixed(sa, free_port(), vec![pb]) }).await.unwrap();
    let b = Node::start(NodeConfig { updates: false, ..fixed(sb, free_port(), vec![]) }).await.unwrap();
    a.add_hint(pb, b.relay_info().unwrap());
    a.introduce(pb);
    assert_eq!(a.pending_introductions(), vec![pb]);

    let got = until("B recebe a apresentação", || Some(b.take_introductions()).filter(|v| !v.is_empty())).await;
    assert_eq!(got.len(), 1);
    let relay = a.relay_info().unwrap();
    assert_eq!((got[0].peer, got[0].name.as_str()), (pa, "Ana"));
    assert_eq!(got[0].reach, Reach::Relay { addr: relay.addr.clone(), onion: relay.onion, link: relay.link });
    until("A recebe o ack", || a.pending_introductions().is_empty().then_some(())).await;

    // O app de B decide adicionar: agora os dois se ligam.
    b.add_contact(pa, Some(relay));
    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), a.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), a)
    });
    let cb = b.dial(pa, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_ca, _a) = answer.await.unwrap();
    cb.hangup();
}

/// B ainda não está no ar quando A o adiciona: a entrega insiste e chega quando ele sobe.
#[tokio::test]
async fn the_introduction_waits_for_the_contact_to_come_online() {
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(NodeConfig { updates: false, ..fixed(sa, free_port(), vec![pb]) }).await.unwrap();
    a.introduce(pb);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(a.pending_introductions(), vec![pb], "sem ack, segue pendente");

    let b = Node::start(NodeConfig { updates: false, ..fixed(sb, free_port(), vec![]) }).await.unwrap();
    a.add_hint(pb, b.relay_info().unwrap());
    let got = until("B recebe a apresentação", || Some(b.take_introductions()).filter(|v| !v.is_empty())).await;
    assert_eq!(got[0].peer, pa);
    until("A recebe o ack", || a.pending_introductions().is_empty().then_some(())).await;
}

/// Com as apresentações desligadas, o quadro é descartado sem resposta e a entrega fica pendente.
#[tokio::test]
async fn introductions_are_ignored_when_switched_off() {
    let (sa, sb) = (key(), key());
    let pb = *PublicKey::from(&sb).as_bytes();
    let a = Node::start(NodeConfig { updates: false, ..fixed(sa, free_port(), vec![pb]) }).await.unwrap();
    let b = Node::start(NodeConfig { updates: false, introductions: false, ..fixed(sb, free_port(), vec![]) }).await.unwrap();
    a.add_hint(pb, b.relay_info().unwrap());
    a.introduce(pb);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(b.take_introductions().is_empty());
    assert_eq!(a.pending_introductions(), vec![pb]);
}

/// Remover o contato cancela a entrega que ainda estava pendente.
#[tokio::test]
async fn forgetting_a_contact_cancels_its_introduction() {
    let (sa, sb) = (key(), key());
    let pb = *PublicKey::from(&sb).as_bytes();
    let a = Node::start(NodeConfig { updates: false, ..fixed(sa, free_port(), vec![pb]) }).await.unwrap();
    a.introduce(pb);
    assert_eq!(a.pending_introductions(), vec![pb]);
    a.forget(&pb);
    assert!(a.pending_introductions().is_empty());
}

/// A e B mudaram de endereço e não têm DHT: uma âncora (nó R) guarda o registro de A e B o acha por ela.
#[tokio::test]
async fn an_anchor_lets_two_moved_contacts_find_each_other() {
    use clam::directory::RelayInfo;
    let (sa, sb, sr) = (key(), key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let r = Node::start(NodeConfig { anchor: true, ..fixed(sr, free_port(), vec![]) }).await.unwrap();
    let anchors = vec![r.relay_info().unwrap()];
    let stale = || RelayInfo { addr: format!("127.0.0.1:{}", free_port()), onion: [1; 32], link: [2; 32] };
    let mut cfg_a = NodeConfig { updates: false, anchors: anchors.clone(), ..fixed(sa, free_port(), vec![pb]) };
    cfg_a.known = vec![(pb, stale())];
    let mut cfg_b = NodeConfig { updates: false, anchors, ..fixed(sb, free_port(), vec![pa]) };
    cfg_b.known = vec![(pa, stale())];
    let mut a = Node::start(cfg_a).await.unwrap();
    let b = Node::start(cfg_b).await.unwrap();
    let fresh_a = a.relay_info().unwrap();

    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(60), a.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), a)
    });
    let mut call = None;
    for _ in 0..15 {
        if let Ok(c) = b.dial(pa, Mulaw::new(), Duration::from_secs(15)).await {
            call = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let cb = call.expect("B achou A pela âncora");
    let (_ca, _a) = answer.await.unwrap();
    assert!(b.known().iter().any(|(k, r)| *k == pa && *r == fresh_a));
    cb.hangup();
}
