#![cfg(feature = "net")]
//! Chamadas entre nós sem servidor nenhum, tudo em 127.0.0.1: o relay do outro vem do código de contato direto,
//! do último endereço guardado, do aviso que ele mandou, da DHT ou de uma âncora.

use std::time::Duration;

use clam::media::codec::Mulaw;
use clam::node::{DialError, Node, NodeConfig, RelayInfo, RelayKeys};
use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};
use tokio::time::{Instant, timeout};

fn key() -> StaticSecret {
    StaticSecret::random_from_rng(OsRng)
}

fn tone(n: usize) -> Vec<i16> {
    (0..160).map(|i| (9000.0 * (std::f64::consts::TAU * 440.0 * (n * 160 + i) as f64 / 8000.0).sin()) as i16).collect()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Nó num porto conhecido, para o endereço dele poder ser escrito antes de subir.
fn fixed(secret: StaticSecret, port: u16, contacts: Vec<[u8; 32]>) -> NodeConfig {
    NodeConfig {
        relay_port: port,
        public_addr: Some(format!("127.0.0.1:{port}")),
        upnp: false,
        relay_keys: Some(RelayKeys::random()),
        update_retry: Duration::from_millis(200),
        ..NodeConfig::new(secret, contacts)
    }
}

/// Como [`fixed`], sem a fila de saída: o endereço novo não é entregue aos contatos, que só têm o que se informa à mão.
fn quiet(secret: StaticSecret, port: u16, contacts: Vec<[u8; 32]>) -> NodeConfig {
    NodeConfig { updates: false, ..fixed(secret, port, contacts) }
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

/// Sem endereço nenhum do contato (nem código, nem guardado), não há como ligar, e o erro diz isso.
#[tokio::test]
async fn dial_without_an_address_says_so() {
    let a = Node::start(quiet(key(), free_port(), vec![])).await.unwrap();
    let err = a.dial([7; 32], Mulaw::new(), Duration::from_secs(2)).await.err().unwrap();
    assert!(matches!(err, DialError::NoAddress { tried_stored: false }), "{err}");
}

/// Duas pessoas, só o código de contato direto: liga e conversa nos dois sentidos.
#[tokio::test]
async fn two_nodes_talk_with_the_direct_code() {
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(fixed(sa, free_port(), vec![pb])).await.unwrap();
    let mut b = Node::start(fixed(sb, free_port(), vec![pa])).await.unwrap();
    a.add_hint(pb, b.relay_info().unwrap());

    let answer = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        assert_eq!(inc.caller().as_bytes(), &pa);
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (cb, _b) = answer.await.unwrap();
    talk(ca, cb).await;
}

/// Desligar de propósito marca `peer_hung_up`; queda de rede não (a GUI só religa neste caso).
#[tokio::test]
async fn call_reports_whether_peer_hung_up() {
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let a = Node::start(fixed(sa, free_port(), vec![pb])).await.unwrap();
    let mut b = Node::start(fixed(sb, free_port(), vec![pa])).await.unwrap();
    a.add_hint(pb, b.relay_info().unwrap());
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
    let (s, keys) = (key(), RelayKeys::random());
    let start = |k: Option<RelayKeys>| NodeConfig { relay_keys: k, ..quiet(s.clone(), free_port(), vec![]) };
    let seen = |node: Node| {
        let r = node.relay_info().unwrap();
        node.stop();
        (r.onion, r.link)
    };
    let first = seen(Node::start(start(Some(keys.clone()))).await.unwrap());
    let second = seen(Node::start(start(Some(keys.clone()))).await.unwrap());
    assert_eq!(first, second);
    assert_eq!(first.0, PublicKey::from(&keys.onion).to_bytes());
    let third = seen(Node::start(start(None)).await.unwrap());
    assert_ne!(first.0, third.0);
    assert_ne!(first.1, third.1);
}

/// Os dois lados reiniciam: com as chaves do relay guardadas, o endereço que A tinha de B ainda vale e a
/// ligação sai sem código novo.
#[tokio::test]
async fn both_restarted_still_dials_stored_address() {
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let (ka, kb) = (RelayKeys::random(), RelayKeys::random());
    let port_b = free_port();
    let with = |s: &StaticSecret, k: &RelayKeys, contacts: Vec<[u8; 32]>, port: u16| NodeConfig { relay_keys: Some(k.clone()), ..quiet(s.clone(), port, contacts) };

    let mut b = Node::start(with(&sb, &kb, vec![pa], port_b)).await.unwrap();
    let a = Node::start(with(&sa, &ka, vec![pb], free_port())).await.unwrap();
    a.add_hint(pb, b.relay_info().unwrap());
    let first = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (cb, b) = first.await.unwrap();
    let known = a.known();
    assert_eq!(known.len(), 1);
    ca.hangup();
    drop(cb);
    a.stop();
    b.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut b2 = Node::start(with(&sb, &kb, vec![pa], port_b)).await.unwrap();
    let a2 = Node::start(NodeConfig { known, ..with(&sa, &ka, vec![pb], free_port()) }).await.unwrap();
    let second = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b2.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b2)
    });
    let ca = a2.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_cb, _b2) = second.await.unwrap();
    ca.hangup();
}

/// Endereço guardado que não responde e nenhum outro caminho: o erro diz que tentou o guardado.
#[tokio::test]
async fn dead_stored_address_says_it_was_tried() {
    let pb = *PublicKey::from(&key()).as_bytes();
    let dead = RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] };
    let a = Node::start(NodeConfig { known: vec![(pb, dead)], ..quiet(key(), free_port(), vec![]) }).await.unwrap();
    let err = a.dial(pb, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::NoAddress { tried_stored: true }), "{err}");
    let other = *PublicKey::from(&key()).as_bytes();
    let err = a.dial(other, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::NoAddress { tried_stored: false }), "{err}");
}

/// Quem atende aprende o relay de quem ligou pelo convite: ligar de volta funciona sem ter o código dele.
#[tokio::test]
async fn callee_learns_the_caller_from_the_invite_and_calls_back() {
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    // `b` só aceita `a` depois de subir e não tem o relay dele.
    let mut b = Node::start(quiet(sb, free_port(), vec![])).await.unwrap();
    b.allow(&[pa]);
    let mut a = Node::start(quiet(sa, free_port(), vec![pb])).await.unwrap();
    a.add_hint(pb, b.relay_info().unwrap());
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
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let dead = RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] };
    let mut b = Node::start(NodeConfig { known: vec![(pa, dead.clone())], ..quiet(sb, free_port(), vec![]) }).await.unwrap();
    b.allow(&[pa]);
    let mut a = Node::start(quiet(sa, free_port(), vec![pb])).await.unwrap();
    a.add_hint(pb, b.relay_info().unwrap());
    let first = tokio::spawn(async move {
        let inc = timeout(Duration::from_secs(15), b.next_incoming()).await.unwrap().unwrap();
        (inc.accept(Mulaw::new()).await.unwrap(), b)
    });
    let ca = a.dial(pb, Mulaw::new(), Duration::from_secs(15)).await.unwrap();
    let (_cb, b) = first.await.unwrap();
    ca.hangup();
    assert_eq!(b.dialer().stored_address(&pa).as_deref(), Some("127.0.0.1:1"));
    assert_eq!(b.hints().len(), 1);

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

/// Uma dica que ninguém atende não vira endereço guardado: o erro diz que tentou.
#[tokio::test]
async fn a_dead_hint_never_becomes_the_stored_address() {
    let pb = *PublicKey::from(&key()).as_bytes();
    let dead = RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] };
    let a = Node::start(NodeConfig { hints: vec![(pb, dead)], ..quiet(key(), free_port(), vec![]) }).await.unwrap();
    let err = a.dial(pb, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::NoAddress { tried_stored: true }), "{err}");
    assert!(a.known().is_empty());
    assert_eq!(a.hints().len(), 1);
}

/// O relay de quem atende, passado fora de banda (o código de contato direto), basta para a primeira
/// chamada, e depois dela vira o endereço guardado.
#[tokio::test]
async fn first_call_needs_only_the_direct_code() {
    let (sa, sb) = (key(), key());
    let (pa, pb) = (*PublicKey::from(&sa).as_bytes(), *PublicKey::from(&sb).as_bytes());
    let mut b = Node::start(quiet(sb, free_port(), vec![pa])).await.unwrap();
    let a = Node::start(quiet(sa, free_port(), vec![pb])).await.unwrap();
    assert!(a.network_note().is_none() && b.network_note().is_none());
    // Sem o código, não há como achar o contato.
    let err = a.dial(pb, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::NoAddress { tried_stored: false }), "{err}");

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

/// O endereço do código direto não responde: o erro diz que tentou o guardado.
#[tokio::test]
async fn dead_direct_code_says_so() {
    let pb = *PublicKey::from(&key()).as_bytes();
    let a = Node::start(quiet(key(), free_port(), vec![])).await.unwrap();
    a.add_hint(pb, RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] });
    let err = a.dial(pb, Mulaw::new(), Duration::from_secs(3)).await.err().unwrap();
    assert!(matches!(err, DialError::NoAddress { tried_stored: true }), "{err}");
    assert!(a.known().is_empty());
}

/// Contato novo entra com o endereço do código; contato removido leva o endereço guardado junto.
#[tokio::test]
async fn adding_and_forgetting_a_contact_moves_its_address_too() {
    let pb = *PublicKey::from(&key()).as_bytes();
    let relay = RelayInfo { addr: "127.0.0.1:1".into(), onion: [7; 32], link: [8; 32] };
    let a = Node::start(quiet(key(), free_port(), vec![])).await.unwrap();
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
    let cfg = NodeConfig { relay_port: 0, upnp: false, public_addr: Some("casa.exemplo.com:4001".into()), ..NodeConfig::new(key(), vec![]) };
    let a = Node::start(cfg).await.unwrap();
    assert_eq!(a.relay_info().unwrap().addr, "casa.exemplo.com:4001");
}

/// A muda de endereço com B desligado; quando B volta, a fila de saída de A entrega o endereço novo e a chamada funciona.
#[tokio::test]
async fn a_new_address_reaches_the_contact_that_was_offline() {
    use clam::node::RelayInfo;
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
    use clam::node::RelayInfo;
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
    use clam::node::RelayInfo;
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
    use clam::node::RelayInfo;
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
    use clam::node::RelayInfo;
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
    assert_eq!(got[0].reach, Reach { addr: relay.addr.clone(), onion: relay.onion, link: relay.link });
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
    use clam::node::RelayInfo;
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
