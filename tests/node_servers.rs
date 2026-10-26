#![cfg(feature = "net")]
//! Servidores entre nós de verdade, em 127.0.0.1: convite, entrada, presença e salas pelo telefone e pelos circuitos.

use std::time::Duration;

use clam::node::{Node, NodeConfig, RelayKeys};
use clam::server::{Event, ServerError, ServerId};
use clam::{
    rand_core::OsRng,
    x25519_dalek::{PublicKey, StaticSecret},
};
use tokio::time::timeout;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Person {
    node: Node,
    key: [u8; 32],
}

async fn person() -> Person {
    person_with(StaticSecret::random_from_rng(OsRng), None, vec![]).await
}

async fn person_with(secret: StaticSecret, servers: Option<String>, anchors: Vec<clam::node::RelayInfo>) -> Person {
    let key = *PublicKey::from(&secret).as_bytes();
    let port = free_port();
    let cfg = NodeConfig {
        relay_port: port,
        public_addr: Some(format!("127.0.0.1:{port}")),
        upnp: false,
        relay_keys: Some(RelayKeys::random()),
        updates: false,
        servers,
        anchors,
        ..NodeConfig::new(secret, vec![])
    };
    Person { node: Node::start(cfg).await.unwrap(), key }
}

async fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    timeout(Duration::from_secs(40), async {
        loop {
            if let Some(v) = f() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("não aconteceu: {what}"))
}

/// `from` convida `to` e `to` aceita; espera `to` entrar.
async fn join(from: &Person, to: &Person, server: ServerId) {
    let text = until("convite", || match from.node.servers().try_run(|s, now| s.invite(&server, to.key, now).map(|t| (t, Default::default()))) {
        Ok(t) => Some(t),
        Err(ServerError::NoAddress) => None,
        Err(e) => panic!("{e:?}"),
    })
    .await;
    let hub = to.node.servers();
    let id = until("aceitar", || hub.try_run(|s, now| s.accept(&text, now)).ok()).await;
    assert_eq!(id, server);
    until("entrou", || hub.take_events().into_iter().find(|e| matches!(e, Event::Joined(_))).map(|_| ())).await;
}

fn members_online(p: &Person, server: ServerId) -> Vec<[u8; 32]> {
    p.node.servers().read(|s, now| s.view(&server, now).map(|v| v.members.iter().filter(|m| m.online).map(|m| m.key).collect()).unwrap_or_default())
}

#[tokio::test]
async fn people_join_see_each_other_online_and_meet_in_a_room() {
    let (a, b, c) = (person().await, person().await, person().await);
    let server = until("criar", || a.node.servers().try_run(|s, now| s.create("Amigos", now)).ok()).await;

    join(&a, &b, server).await;
    until("A vê B online", || (members_online(&a, server).contains(&b.key)).then_some(())).await;
    until("B vê A online", || (members_online(&b, server).contains(&a.key)).then_some(())).await;

    // C entra convidado por B, e A fica sabendo dele sem ter falado com ele.
    join(&b, &c, server).await;
    until("A vê C", || a.node.servers().read(|s, now| s.view(&server, now).is_some_and(|v| v.members.len() == 3)).then_some(())).await;
    until("A vê C online", || members_online(&a, server).contains(&c.key).then_some(())).await;

    // Sala criada por A aparece para todos; quem entra nela aparece na sala para os outros.
    let room = a.node.servers().try_run(|s, now| s.create_room(&server, "Geral", now)).unwrap();
    until("B vê a sala", || b.node.servers().read(|s, now| s.view(&server, now).is_some_and(|v| v.rooms.iter().any(|r| r.id == room))).then_some(())).await;
    b.node.servers().try_act(|s, now| s.join_room(&server, Some(room), now)).unwrap();
    until("A vê B na sala", || {
        a.node.servers().read(|s, now| s.view(&server, now).is_some_and(|v| v.rooms.iter().any(|r| r.id == room && r.people.len() == 1))).then_some(())
    })
    .await;
    until("C vê a sala", || c.node.servers().read(|s, now| s.view(&server, now).is_some_and(|v| v.rooms.iter().any(|r| r.id == room))).then_some(())).await;
    c.node.servers().try_act(|s, now| s.join_room(&server, Some(room), now)).unwrap();
    until("B vê C na sala (malha)", || {
        b.node.servers().take_events().into_iter().find(|e| matches!(e, Event::Mesh { peers, .. } if peers.contains(&c.key))).map(|_| ())
    })
    .await;

    // A malha liga quem não é contato: B e C se aceitam por estarem na mesma sala, e a voz passa.
    b.node.set_room_peers(&[c.key]);
    c.node.set_room_peers(&[b.key]);
    let codec = clam::media::codec::CodecId::G722;
    let dialer = c.node.dialer();
    let mut b = b;
    let (dialed, answered) = tokio::join!(dialer.dial(b.key, codec.build(), Duration::from_secs(20)), async {
        let inc = timeout(Duration::from_secs(20), b.node.next_incoming()).await.expect("B recebe").expect("chamada");
        assert_eq!(inc.caller().as_bytes(), &c.key);
        inc.accept(codec.build()).await.expect("B atende")
    });
    let from_c = dialed.expect("C liga para B");
    let mut at_b = answered;
    from_c.send(vec![1000; 320]).await.unwrap();
    assert!(timeout(Duration::from_secs(10), at_b.next_pcm()).await.expect("voz chega").is_some());
}

/// Quem perdeu o endereço de um membro acha o registro que ele deixa numa âncora para os membros do servidor.
#[tokio::test]
async fn a_member_finds_another_through_the_server_record_in_an_anchor() {
    let secret_r = StaticSecret::random_from_rng(OsRng);
    let port = free_port();
    let anchor = Node::start(NodeConfig {
        relay_port: port,
        public_addr: Some(format!("127.0.0.1:{port}")),
        upnp: false,
        relay_keys: Some(RelayKeys::random()),
        updates: false,
        anchor: true,
        ..NodeConfig::new(secret_r, vec![])
    })
    .await
    .unwrap();
    let anchors = vec![anchor.relay_info().unwrap()];
    let (secret_a, secret_b) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
    let a = person_with(secret_a.clone(), None, anchors.clone()).await;
    let b = person_with(secret_b, None, anchors.clone()).await;
    let server = until("criar", || a.node.servers().try_run(|s, now| s.create("Amigos", now)).ok()).await;
    join(&a, &b, server).await;
    until("A vê B online", || (members_online(&a, server).contains(&b.key)).then_some(())).await;

    // A reabre sem o endereço de B (só com o servidor gravado) e procura pelo registro.
    let saved = a.node.servers().read(|s, _| s.export());
    drop(a);
    let a2 = person_with(secret_a, Some(saved), anchors).await;
    assert_eq!(a2.node.dialer().stored_address(&b.key), None);
    let shared = a2.node.servers().read(|s, _| s.shared_secrets(&b.key));
    assert_eq!(shared.len(), 1);
    let dialer = a2.node.dialer();
    let mut found = false;
    for _ in 0..30 {
        if dialer.find_member(&b.key, &shared).await {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(found, "o registro de B está na âncora");
    assert_eq!(dialer.stored_address(&b.key), b.node.relay_info().map(|r| r.addr));
}
