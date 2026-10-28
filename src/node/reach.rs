//! Endereço deste relay: como os outros chegam a ele, a renovação da porta no roteador e o registro que vai aos contatos.

use crate::sync::LockExt;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};


use crate::{
    upnp,
    media::{
        signal::RelayHint,
        update::{AddressRecord, MAX_ADDR},
    },
    phone::PhoneLink,
};
use super::{Current, RelayInfo};
use super::util::{unix_millis};

/// De quanto em quanto tempo o nó renova a porta aberta no roteador (UPnP) e confere se o endereço dele mudou.
const RENEW_EVERY: Duration = Duration::from_secs(40);
/// Sem porta aberta, o UPnP é tentado de novo a cada tantas conferências (o roteador pode ter demorado a subir).
const MAP_RETRY_EVERY: u32 = 3;

/// Relay a informar nos convites: o endereço público informado ou aberto por UPnP ou, na falta dele
/// (`published` falso: `mine.addr` é só o de loopback inventado aqui), o IP desta máquina na rede
/// local. `None` se não houver endereço que alguém de fora use.
pub(super) fn relay_hint(mine: &RelayInfo, published: bool) -> Option<RelayHint> {
    let addr: SocketAddr = mine.addr.parse().ok()?;
    let addr = if !published && (addr.ip().is_loopback() || addr.ip().is_unspecified()) { SocketAddr::new(local_ip()?, addr.port()) } else { addr };
    Some(RelayHint { addr, onion: mine.onion, link: mine.link })
}

/// Relay para o código de contato direto: o endereço publicado como está (pode ser um nome, como
/// `casa.exemplo.com:4001`) ou, não publicado, o IP desta máquina na rede local (veja [`relay_hint`]).
pub(super) fn own_relay(mine: &RelayInfo, published: bool) -> Option<RelayInfo> {
    if published {
        return Some(mine.clone());
    }
    relay_hint(mine, false).map(|h| RelayInfo { addr: h.addr.to_string(), onion: mine.onion, link: mine.link })
}

/// IP desta máquina na rota para fora. O "connect" de UDP só escolhe a rota: nenhum pacote sai.
fn local_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

/// O endereço pelo qual os outros chegam a este relay, que pode mudar com o programa aberto (o provedor troca
/// o IP, a máquina troca de rede). Quem muda o endereço passa por aqui: o código de contato direto, o que os
/// convites levam e o aviso aos contatos (`track_address`) seguem o novo.
#[derive(Clone)]
pub(super) struct Reachability {
    pub(super) onion: [u8; 32],
    pub(super) link: [u8; 32],
    pub(super) port: u16,
    pub(super) addr: Arc<Mutex<String>>,
    pub(super) own: Arc<Mutex<Option<RelayInfo>>>,
    pub(super) phone: PhoneLink,
    pub(super) notice: Arc<Mutex<Option<String>>>,
}

impl Reachability {
    /// Passa a usar `addr` (`published`: alguém de fora o alcança; falso, é o IP da rede local). Devolve se mudou.
    pub(super) fn set(&self, addr: String, published: bool) -> bool {
        let mine = RelayInfo { addr: addr.clone(), onion: self.onion, link: self.link };
        let own = own_relay(&mine, published);
        let mut now = self.own.lock_unpoisoned();
        if *now == own {
            return false;
        }
        if let Some(r) = &own {
            *self.notice.lock_unpoisoned() = Some(format!("seu endereço mudou: agora {}", r.addr));
        }
        *now = own;
        *self.addr.lock_unpoisoned() = addr;
        self.phone.set_relay_hint(relay_hint(&mine, published));
        true
    }
}

/// Renova a porta no roteador e confere o endereço do nó de tempos em tempos: com a porta aberta por UPnP, o IP
/// externo do roteador; sem ela, tenta abrir de novo e acompanha o IP da máquina na rede local. Só roda quando o
/// endereço público não foi informado (um endereço informado é do usuário, que o corrige se precisar).
pub(super) async fn watch_address(reach: Reachability, mut mapping: Option<upnp::Mapping>, upnp_on: bool) {
    let mut round = 0u32;
    loop {
        tokio::time::sleep(RENEW_EVERY).await;
        round += 1;
        if let Some(m) = &mapping {
            let _ = m.renew().await;
            // Roteador sem resposta ou IP que ninguém de fora alcança: fica o endereço de antes.
            if let Ok(addr) = m.external_addr().await {
                reach.set(addr, true);
            }
            continue;
        }
        if upnp_on
            && round.is_multiple_of(MAP_RETRY_EVERY)
            && let Ok(m) = upnp::map(reach.port).await
        {
            reach.set(m.external.clone(), true);
            mapping = Some(m);
            continue;
        }
        reach.set(format!("127.0.0.1:{}", reach.port), false);
    }
}

/// Mantém em `current` o registro do endereço deste relay: um `seq` novo (a hora em ms, nunca menor que o
/// anterior) cada vez que o endereço muda. A fila de saída e a gravação na DHT entregam o mesmo registro,
/// então o contato vê o mesmo `seq` por qualquer caminho e descarta o que não for mais novo.
pub(super) async fn track_address(own: Arc<Mutex<Option<RelayInfo>>>, current: Current) {
    let mut seq = 0u64;
    loop {
        let mine = own.lock_unpoisoned().clone().filter(|m| (1..=MAX_ADDR).contains(&m.addr.len()));
        if let Some(mine) = mine {
            let changed = current.lock_unpoisoned().as_ref().is_none_or(|c| c.addr != mine.addr || c.onion != mine.onion || c.link != mine.link);
            if changed {
                seq = (unix_millis() as u64).max(seq + 1);
                *current.lock_unpoisoned() = Some(AddressRecord { seq, addr: mine.addr, onion: mine.onion, link: mine.link });
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
