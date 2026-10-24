#![cfg(feature = "net")]

use std::{sync::Arc, time::Duration};

use clam::{
    descriptor::{DirectoryConfig, RelayDescriptor},
    discovery::{Announcer, Discovery, DiscoveryConfig, DiscoveryRunner, RELAY_TOPIC},
    ed25519_dalek::SigningKey,
    net::{NetConfig, Node},
    rand_core::OsRng,
    runner::system_clock,
};
use libp2p::{Multiaddr, multiaddr::Protocol};
use tokio::time::timeout;

const RELAY_ADDR: &str = "127.0.0.1:4001";

fn lab_policy() -> DirectoryConfig {
    DirectoryConfig { allow_private_addrs: true, ..Default::default() }
}

async fn start(bootstrap: Vec<Multiaddr>) -> (Node, Multiaddr) {
    let node = Node::start(NetConfig {
        listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
        bootstrap,
        relay_policy: lab_policy(),
        ..Default::default()
    })
    .await
    .unwrap();
    let addr = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = node.listen_addrs().await.unwrap().into_iter().next() {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("não escutou");
    let full = addr.with(Protocol::P2p(node.peer_id()));
    (node, full)
}

fn announcer() -> Announcer {
    let identity = SigningKey::generate(&mut OsRng);
    Arc::new(move |now| {
        Some(RelayDescriptor::sign(&identity, [9; 32], [10; 32], RELAY_ADDR, 2_000, now, 3600).unwrap().to_bytes())
    })
}

fn discovery() -> Arc<Discovery> {
    Arc::new(Discovery::new(lab_policy(), system_clock()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_learns_relays_announced_through_a_middle_node() {
    let (mut mid, mid_addr) = start(vec![]).await;
    let mid_disc = discovery();
    let _mid_run = DiscoveryRunner::spawn(
        mid_disc.clone(),
        mid.gossip(),
        mid.take_descriptors().unwrap(),
        None,
        DiscoveryConfig::default(),
    );

    let (mut client, _) = start(vec![mid_addr.clone()]).await;
    let client_disc = discovery();
    let _client_run = DiscoveryRunner::spawn(
        client_disc.clone(),
        client.gossip(),
        client.take_descriptors().unwrap(),
        None,
        DiscoveryConfig::default(),
    );

    let (mut relay, _) = start(vec![mid_addr]).await;
    let _relay_run = DiscoveryRunner::spawn(
        discovery(),
        relay.gossip(),
        relay.take_descriptors().unwrap(),
        Some(announcer()),
        DiscoveryConfig::default(),
    );

    timeout(Duration::from_secs(40), async {
        loop {
            if client_disc.candidates().iter().any(|c| c.addr == RELAY_ADDR) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("cliente não descobriu o relay");
    assert!(mid_disc.candidates().iter().any(|c| c.addr == RELAY_ADDR));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_and_garbage_descriptors_are_not_forwarded() {
    let (mut mid, mid_addr) = start(vec![]).await;
    let _mid_run = DiscoveryRunner::spawn(
        discovery(),
        mid.gossip(),
        mid.take_descriptors().unwrap(),
        None,
        DiscoveryConfig::default(),
    );

    // Observador cru: assina o tópico e olha o que o nó do meio repassa.
    let (mut watcher, _) = start(vec![mid_addr.clone()]).await;
    let mut raw = watcher.take_descriptors().unwrap();
    watcher.gossip().subscribe(RELAY_TOPIC).unwrap();

    let (attacker, _) = start(vec![mid_addr]).await;
    let gossip = attacker.gossip();
    gossip.subscribe(RELAY_TOPIC).unwrap();

    let identity = SigningKey::generate(&mut OsRng);
    let now = system_clock()();
    let good = RelayDescriptor::sign(&identity, [1; 32], [2; 32], "127.0.0.1:5001", 100, now, 3600).unwrap().to_bytes();
    let mut forged = good.clone();
    let last = forged.len() - 1;
    forged[last] ^= 1; // assinatura quebrada
    let expired = RelayDescriptor::sign(&identity, [1; 32], [2; 32], "127.0.0.1:5002", 100, now - 7200, 3600).unwrap().to_bytes();

    let mut seen = vec![];
    // Reenvia por alguns segundos até a malha se formar; o válido tem que chegar, os outros não.
    let result = timeout(Duration::from_secs(40), async {
        loop {
            let _ = gossip.publish(RELAY_TOPIC, b"lixo".to_vec());
            let _ = gossip.publish(RELAY_TOPIC, forged.clone());
            let _ = gossip.publish(RELAY_TOPIC, expired.clone());
            let _ = gossip.publish(RELAY_TOPIC, good.clone());
            if let Ok(Some(m)) = timeout(Duration::from_millis(700), raw.recv()).await {
                seen.push(m.clone());
                if m == good {
                    return;
                }
            }
        }
    })
    .await;
    assert!(result.is_ok(), "o descritor válido deveria chegar");
    assert!(seen.iter().all(|m| *m == good), "chegou descritor inválido: {} mensagens", seen.len());
}
