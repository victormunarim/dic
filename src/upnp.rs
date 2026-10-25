//! Abertura automática da porta do relay no roteador, por UPnP (feature `net`).
//!
//! Quem está atrás de um roteador doméstico não recebe conexões de fora. O roteador, se aceitar UPnP IGD,
//! encaminha uma porta TCP para a máquina quando ela pede. Assim o relay fica alcançável sem firewall
//! manual e sem servidor no caminho: os outros nós conectam direto. A concessão dura [`LEASE_SECS`] e é
//! renovada pelo [`Mapping::renew`]; se o programa morrer, o roteador a esquece sozinho.
//!
//! Não funciona sem UPnP no roteador, nem com CGNAT (a operadora compartilha o IP público): nesses
//! casos o IP externo do roteador é privado ou da faixa 100.64.0.0/10, e [`map`] recusa.

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use igd_next::{PortMappingProtocol, SearchOptions, aio::tokio::search_gateway};
use tokio::time::timeout;

/// Validade de cada concessão no roteador.
pub const LEASE_SECS: u32 = 300;
const SEARCH: Duration = Duration::from_secs(3);
const DESCRIPTION: &str = "clam";

/// Porta aberta no roteador.
pub struct Mapping {
    gateway: igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>,
    local: SocketAddr,
    port: u16,
    /// `ip:porta` público, para publicar no diretório.
    pub external: String,
}

/// IPv4 que não serve como endereço público: privado, loopback, link-local ou CGNAT (100.64.0.0/10).
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified() || (v.octets()[0] == 100 && (64..128).contains(&v.octets()[1])),
        IpAddr::V6(v) => v.is_loopback() || v.is_unspecified(),
    }
}

/// Pede ao roteador que encaminhe a porta TCP `port` para esta máquina.
pub async fn map(port: u16) -> Result<Mapping, String> {
    let mut opts = SearchOptions::default();
    opts.timeout = Some(SEARCH);
    opts.single_search_timeout = Some(SEARCH);
    let gateway = timeout(SEARCH * 2, search_gateway(opts))
        .await
        .map_err(|_| "roteador não respondeu".to_string())?
        .map_err(|e| format!("sem roteador UPnP: {e}"))?;
    // Descobre o IP local pelo qual o roteador é alcançado (sem enviar nada).
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    probe.connect(gateway.addr).map_err(|e| e.to_string())?;
    let local_ip = probe.local_addr().map_err(|e| e.to_string())?.ip();
    let local = SocketAddr::new(local_ip, port);
    let ext = gateway.get_external_ip().await.map_err(|e| format!("roteador não informou o IP externo: {e}"))?;
    if is_private(ext) {
        return Err(format!("o IP externo do roteador é {ext} (CGNAT ou outro roteador na frente): ele não aceita conexões de fora"));
    }
    gateway
        .add_port(PortMappingProtocol::TCP, port, local, LEASE_SECS, DESCRIPTION)
        .await
        .map_err(|e| format!("roteador recusou abrir a porta {port}: {e}"))?;
    Ok(Mapping { gateway, local, port, external: SocketAddr::new(ext, port).to_string() })
}

impl Mapping {
    /// Renova a concessão. Erro ignorado de propósito: a próxima renovação tenta de novo.
    pub async fn renew(&self) -> Result<(), String> {
        self.gateway.add_port(PortMappingProtocol::TCP, self.port, self.local, LEASE_SECS, DESCRIPTION).await.map_err(|e| e.to_string())
    }

    /// Fecha a porta no roteador.
    pub async fn close(&self) {
        let _ = self.gateway.remove_port(PortMappingProtocol::TCP, self.port).await;
    }
}

/// O host do diretório (`host:porta`) é da rede local (IP privado, loopback ou nome que resolve para isso)?
/// Então todos estão na mesma LAN e abrir porta no roteador só atrapalharia (o IP público talvez nem seja
/// alcançável de dentro).
pub async fn directory_is_local(directory: &str) -> bool {
    match tokio::net::lookup_host(directory).await {
        Ok(mut it) => it.next().is_some_and(|a| is_private(a.ip())),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        for s in ["10.0.0.1", "192.168.15.9", "172.16.0.1", "127.0.0.1", "169.254.1.1", "100.64.0.1", "100.127.255.255"] {
            assert!(is_private(s.parse().unwrap()), "{s}");
        }
        for s in ["8.8.8.8", "100.63.0.1", "100.128.0.1", "203.0.113.5"] {
            assert!(!is_private(s.parse().unwrap()), "{s}");
        }
    }

    #[tokio::test]
    async fn local_directory_detected() {
        assert!(directory_is_local("127.0.0.1:7777").await);
        assert!(directory_is_local("192.168.0.20:7777").await);
        assert!(!directory_is_local("203.0.113.5:7777").await);
    }
}
