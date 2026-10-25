//! Ponto de encontro na DHT (feature `net`): onde dois contatos que mudaram de endereço se reencontram.
//!
//! Cada dono guarda o próprio registro de endereço, cifrado para o contato ([`crate::media::update::seal_record`]),
//! num item mutável da Mainline DHT do BitTorrent (BEP 44). O alvo do item vem do par de chaves e do dia
//! ([`crate::media::update::rendezvous`]), então só os dois lados do par sabem onde procurar, e o alvo
//! muda sozinho a cada dia. Nenhum servidor do projeto participa.
//!
//! Quem guarda o item na DHT vê o IP de quem escreve e o de quem lê; o conteúdo só abre com a chave do par.
//! Itens somem da DHT em cerca de 2 h sem renovação, então o depósito só existe enquanto o dono está online.

use std::{io, net::Ipv4Addr, time::Duration};

use mainline::{Dht, MutableItem, SigningKey, async_dht::AsyncDht};

/// Quanto esperar a DHT gravar um item.
const PUT_WAIT: Duration = Duration::from_secs(20);

/// Quanto esperar a DHT devolver os itens de um alvo.
const GET_WAIT: Duration = Duration::from_secs(10);

/// Como o nó entra na DHT.
#[derive(Clone, Debug, Default)]
pub struct DhtOptions {
    /// Nós iniciais (`host:porta`). Vazio: os nós públicos da Mainline.
    pub bootstrap: Vec<String>,
    /// Endereço local do socket UDP da DHT. `None`: qualquer interface (nos testes, loopback).
    pub bind: Option<Ipv4Addr>,
}

/// Cliente da DHT: grava e lê os itens do ponto de encontro.
pub struct Rendezvous {
    dht: AsyncDht,
}

fn signer(seed: &[u8; 32]) -> SigningKey {
    SigningKey::from_bytes(seed)
}

impl Rendezvous {
    /// Entra na DHT. Resolver os nós iniciais pode bloquear, então roda fora do executor.
    pub async fn start(options: &DhtOptions) -> io::Result<Self> {
        let options = options.clone();
        let dht = tokio::task::spawn_blocking(move || {
            let mut builder = Dht::builder();
            if !options.bootstrap.is_empty() {
                builder.bootstrap(&options.bootstrap);
            }
            if let Some(bind) = options.bind {
                builder.bind_address(bind);
            }
            builder.build()
        })
        .await
        .map_err(io::Error::other)??;
        Ok(Self { dht: dht.as_async() })
    }

    /// Grava `value` no alvo `(chave derivada de seed, salt)`. `seq` tem de crescer a cada gravação nova
    /// do mesmo alvo, porque a DHT recusa o mesmo `seq` com outro valor.
    pub async fn publish(&self, seed: &[u8; 32], salt: &[u8; 32], value: &[u8], seq: i64) -> Result<(), String> {
        let item = MutableItem::new(signer(seed), value, seq, Some(salt));
        match tokio::time::timeout(PUT_WAIT, self.dht.put_mutable(item, None)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("a DHT não respondeu a tempo".into()),
        }
    }

    /// Valor mais recente guardado no alvo `(chave derivada de seed, salt)`, se houver.
    pub async fn lookup(&self, seed: &[u8; 32], salt: &[u8; 32]) -> Option<Vec<u8>> {
        let key = signer(seed).verifying_key().to_bytes();
        let item = tokio::time::timeout(GET_WAIT, self.dht.get_mutable_most_recent(&key, Some(salt))).await.ok()??;
        Some(item.value().to_vec())
    }
}
