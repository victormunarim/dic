//! Como o [`Dialer`](super::Dialer) acha o relay de um contato que não responde no endereço guardado: gateways da DHT, âncoras e o ponto de encontro.

use crate::sync::LockExt;
use std::time::Duration;

use x25519_dalek::PublicKey;

use crate::{
    circuit::manager::{Candidate, CircuitManager, ManagerConfig},
    media::update::{self, AddressRecord},
    rendezvous::{self, ANCHOR_DEST, GATEWAY_DEST, Remote, Signed},
    transport::{CircuitClient, ClientOptions},
};
use super::dialer::{Dialer};
use super::{RelayInfo};
use super::util::{ready_within, shuffle, unix_secs};

/// Tempo máximo de uma consulta à DHT ao discar (montar o circuito, quando é o caso, e ler os três dias).
const DHT_LOOKUP_LIMIT: Duration = Duration::from_secs(45);


impl Dialer {
    /// Relays de contatos para montar o circuito até um gateway da DHT, sem o
    /// deste nó e sem os dos contatos em `exclude`.
    fn gateway_pool(&self, exclude: &[[u8; 32]]) -> Vec<RelayInfo> {
        let mut found: Vec<([u8; 32], RelayInfo)> = self.known.lock_unpoisoned().iter().map(|(k, r)| (*k, r.clone())).collect();
        found.extend(self.hints.lock_unpoisoned().iter().map(|(k, r)| (*k, r.clone())));
        for (k, list) in self.history.lock_unpoisoned().iter() {
            found.extend(list.iter().map(|r| (*k, r.clone())));
        }
        let mut pool: Vec<RelayInfo> = Vec::new();
        let mut add = |r: RelayInfo| {
            if r.addr != self.mine.addr && !pool.iter().any(|p| p.addr == r.addr) {
                pool.push(r);
            }
        };
        for (k, r) in found {
            if !exclude.contains(&k) {
                add(r);
            }
        }
        pool
    }

    /// Sessão com um gateway da DHT por um circuito de dois relays que não são deste nó: o primeiro salto vê
    /// o IP e não o pedido, a saída vê o pedido e não o IP. `None`: não há dois relays, ou nenhuma saída serve de gateway.
    pub(super) async fn gateway(&self, exclude: &[[u8; 32]]) -> Option<Remote> {
        let mut pool = self.gateway_pool(exclude);
        if pool.len() < 2 {
            return None;
        }
        shuffle(&mut pool);
        for n in 0..pool.len().min(3) {
            let exit = pool[n].clone();
            let relays: Vec<Candidate> = pool.iter().filter(|r| r.addr != exit.addr).take(3).map(RelayInfo::candidate).collect();
            if let Some(remote) = self.remote_to(GATEWAY_DEST, &exit, relays).await {
                return Some(remote);
            }
        }
        None
    }

    /// Sessão com o serviço `dest` da saída `exit` por um circuito que passa por `relays` antes dela.
    async fn remote_to(&self, dest: &str, exit: &RelayInfo, mut relays: Vec<Candidate>) -> Option<Remote> {
        relays.push(exit.candidate());
        let cfg = ManagerConfig { exit_addr: Some(exit.addr.clone()), ..Default::default() };
        let client = CircuitClient::spawn_with(CircuitManager::new(cfg, dest, relays), Duration::from_secs(3), ClientOptions::default());
        if ready_within(&client, Duration::from_secs(8)).await {
            return Some(Remote::new(client));
        }
        client.stop();
        None
    }

    /// Sessão com a âncora `anchor`, por relays de contatos e, na falta deles, pelo relay deste nó (por
    /// loopback): nesse caso a âncora vê o IP de quem pergunta.
    async fn anchor_remote(&self, anchor: &RelayInfo, exclude: &[[u8; 32]]) -> Option<Remote> {
        let mut pool = self.gateway_pool(exclude);
        pool.retain(|r| r.addr != anchor.addr);
        shuffle(&mut pool);
        let mut relays: Vec<Candidate> = pool.iter().take(2).map(RelayInfo::candidate).collect();
        relays.push(Candidate { addr: format!("127.0.0.1:{}", self.relay_port), public: self.mine.onion, link_public: self.mine.link, score: 0.5 });
        self.remote_to(ANCHOR_DEST, anchor, relays).await
    }

    /// Valores guardados nas âncoras para os alvos `(chave, sal)`: todos os que alguma âncora devolveu.
    async fn anchor_values(&self, targets: &[([u8; 32], [u8; 32])], exclude: &[[u8; 32]]) -> Vec<Vec<u8>> {
        let sessions = futures::future::join_all(self.anchors.iter().map(|a| async move {
            let mut found = Vec::new();
            if let Some(mut remote) = self.anchor_remote(a, exclude).await {
                for (key, salt) in targets {
                    match remote.get(key, salt).await {
                        Ok(Some(value)) => found.push(value),
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
                remote.stop();
            }
            found
        }))
        .await;
        sessions.into_iter().flatten().collect()
    }

    /// Grava os itens em cada âncora; um `bool` por item: `true` se alguma âncora guardou.
    async fn anchor_put(&self, items: &[Signed]) -> Vec<bool> {
        let sessions = futures::future::join_all(self.anchors.iter().map(|a| async move {
            let mut done = vec![false; items.len()];
            if let Some(mut remote) = self.anchor_remote(a, &[]).await {
                for (i, item) in items.iter().enumerate() {
                    match remote.put(item).await {
                        Ok(ok) => done[i] = ok,
                        Err(_) => break,
                    }
                }
                remote.stop();
            }
            done
        }))
        .await;
        (0..items.len()).map(|i| sessions.iter().any(|s| s[i])).collect()
    }

    /// Grava os itens na DHT e nas âncoras ao mesmo tempo; um `bool` por item: `true` se algum lugar guardou.
    pub(super) async fn store_put(&self, items: &[Signed]) -> Vec<bool> {
        let (dht, anchors) = futures::future::join(self.dht_put(items), self.anchor_put(items)).await;
        dht.into_iter().zip(anchors).map(|(a, b)| a || b).collect()
    }

    /// Lê os alvos `(chave, sal)` da DHT, um valor (ou `None`) por alvo, direto ou pelo circuito conforme a
    /// opção. Sem caminho pelo circuito não lê nada: nunca cai para o acesso direto.
    async fn dht_get(&self, targets: &[([u8; 32], [u8; 32])], exclude: &[[u8; 32]]) -> Vec<Option<Vec<u8>>> {
        let Some(dht) = &self.dht else { return vec![None; targets.len()] };
        if !self.dht_circuit {
            return futures::future::join_all(targets.iter().map(|(key, salt)| dht.get(key, salt))).await;
        }
        let mut out = vec![None; targets.len()];
        if let Some(mut remote) = self.gateway(exclude).await {
            for (i, (key, salt)) in targets.iter().enumerate() {
                match remote.get(key, salt).await {
                    Ok(value) => out[i] = value,
                    Err(_) => break,
                }
            }
            remote.stop();
        }
        out
    }

    /// Grava os itens na DHT, direto ou pelo circuito; um `bool` por item.
    async fn dht_put(&self, items: &[Signed]) -> Vec<bool> {
        let Some(dht) = &self.dht else { return vec![false; items.len()] };
        if !self.dht_circuit {
            return futures::future::join_all(items.iter().map(|item| async move { dht.put(item).await.is_ok() })).await;
        }
        let mut out = vec![false; items.len()];
        if let Some(mut remote) = self.gateway(&[]).await {
            for (i, item) in items.iter().enumerate() {
                match remote.put(item).await {
                    Ok(done) => out[i] = done,
                    Err(_) => break,
                }
            }
            remote.stop();
        }
        out
    }

    /// O endereço mais novo que `peer` deixou para nós na DHT e nas âncoras: consulta o dia anterior, o atual e o
    /// seguinte (o relógio de um dos dois pode estar errado) e fica com o registro de maior `seq`.
    pub(super) async fn rendezvous_record(&self, peer: &[u8; 32]) -> Option<AddressRecord> {
        let pk = PublicKey::from(*peer);
        let today = unix_secs() / 86400;
        let targets: Vec<([u8; 32], [u8; 32])> = [today, today.saturating_sub(1), today + 1]
            .into_iter()
            .filter_map(|period| update::rendezvous(&self.secret, &pk, &pk, period))
            .map(|(seed, salt)| (rendezvous::public_key(&seed), salt))
            .collect();
        self.lookup_values(&targets, peer).await.iter().filter_map(|v| update::open_record(&self.secret, &pk, v)).max_by_key(|r| r.seq)
    }

    /// Valores guardados nos `targets` `(chave, sal)` da DHT e das âncoras, sem passar pelos relays de `excluded`.
    async fn lookup_values(&self, targets: &[([u8; 32], [u8; 32])], excluded: &[u8; 32]) -> Vec<Vec<u8>> {
        if self.dht.is_none() && self.anchors.is_empty() {
            return Vec::new();
        }
        let exclude = [*excluded];
        let lookup = futures::future::join(self.dht_get(targets, &exclude), self.anchor_values(targets, &exclude));
        let Ok((dht, anchors)) = tokio::time::timeout(DHT_LOOKUP_LIMIT, lookup).await else { return Vec::new() };
        dht.into_iter().flatten().chain(anchors).collect()
    }

    /// O endereço mais novo que `member` deixou, na DHT e nas âncoras, para os membros dos servidores que os dois
    /// têm (`shared`: id e segredo atual de cada um). Vira a dica de relay de `member` e devolve se achou algum.
    pub async fn find_member(&self, member: &[u8; 32], shared: &[([u8; 16], [u8; 32])]) -> bool {
        let today = unix_secs() / 86400;
        let mut targets: Vec<([u8; 32], [u8; 32])> = Vec::new();
        for (server, secret) in shared {
            for period in [today, today.saturating_sub(1), today + 1] {
                let (seed, salt) = update::server_rendezvous(secret, server, member, period);
                targets.push((rendezvous::public_key(&seed), salt));
            }
        }
        let values = self.lookup_values(&targets, member).await;
        let best = values.iter().filter_map(|v| shared.iter().find_map(|(server, secret)| update::open_server_record(secret, server, member, v))).max_by_key(|r| r.seq);
        match best {
            Some(r) => {
                self.add_hint(*member, RelayInfo { addr: r.addr, onion: r.onion, link: r.link });
                true
            }
            None => false,
        }
    }
}
