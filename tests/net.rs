#![cfg(feature = "net")]

use std::time::Duration;

use clam::{
    ed25519_dalek::SigningKey,
    net::{NetConfig, Node},
    pairing::PresenceKeys,
    rand_core::OsRng,
    service::{self, PresenceConfig},
    x25519_dalek::{PublicKey, StaticSecret},
};
use libp2p::{Multiaddr, multiaddr::Protocol};

const TIMEOUT: Duration = Duration::from_secs(20);

async fn listening(cfg_bootstrap: Vec<Multiaddr>) -> (Node, Multiaddr) {
    let node = Node::start(NetConfig {
        listen: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
        bootstrap: cfg_bootstrap,
        get_timeout: Some(Duration::from_secs(3)),
        ..Default::default()
    })
    .await
    .unwrap();
    let addr = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Some(a) = node.listen_addrs().await.unwrap().into_iter().next() {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("nó não começou a escutar");
    let full = addr.with(Protocol::P2p(node.peer_id()));
    (node, full)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_roundtrip_over_dht() {
    let (alice_node, alice_addr) = listening(vec![]).await;
    let (bob_node, _) = listening(vec![alice_addr]).await;

    let a = StaticSecret::random_from_rng(OsRng);
    let b = StaticSecret::random_from_rng(OsRng);
    let signer = SigningKey::generate(&mut OsRng);
    let (ka, kb) = (
        PresenceKeys::derive(&a, &PublicKey::from(&b)),
        PresenceKeys::derive(&b, &PublicKey::from(&a)),
    );
    let cfg = PresenceConfig::default();
    let now = 1_795_000_000;
    let intros = vec![b"relay.example:4001".to_vec()];

    let (astore, bstore) = (alice_node.store(), bob_node.store());
    let (cfg2, intros2, verifier) = (cfg, intros.clone(), signer.verifying_key());
    let found = tokio::task::spawn_blocking(move || {
        service::publish(&astore, &cfg2, &ka, &signer, now, &intros2).unwrap();
        // A conexão entre os nós pode levar um instante: tenta algumas vezes.
        for _ in 0..10 {
            if let Some(r) = service::lookup(&bstore, &cfg2, &kb, &verifier, now) {
                return Some(r);
            }
        }
        None
    })
    .await
    .unwrap();
    assert_eq!(found, Some(intros));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gossip_delivers_on_shared_topic() {
    let (alice, alice_addr) = listening(vec![]).await;
    let (mut bob, _) = listening(vec![alice_addr]).await;
    alice.subscribe("topico-opaco").unwrap();
    bob.subscribe("topico-opaco").unwrap();

    let got = tokio::time::timeout(TIMEOUT, async {
        loop {
            // A malha leva alguns segundos para se formar; republica até chegar.
            alice.publish("topico-opaco", b"call-anuncio".to_vec()).unwrap();
            if let Ok(Some(m)) = tokio::time::timeout(Duration::from_secs(1), bob.next_message()).await {
                return m;
            }
        }
    })
    .await
    .expect("mensagem não chegou");
    assert_eq!(got.topic, "topico-opaco");
    assert_eq!(got.data, b"call-anuncio");
}
