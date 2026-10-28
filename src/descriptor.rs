//! Descritores assinados de relays e o diretório que os guarda.
//!
//! Um relay publica um [`RelayDescriptor`]: endereço, chave de circuito
//! (X25519), banda declarada e validade, tudo assinado pela **identidade** do
//! relay (ed25519). Quem recebe o descritor, por qualquer canal (GossipSub,
//! arquivo de sementes, outro nó), o confere com [`RelayDescriptor::verify`]
//! ou o entrega ao [`RelayDirectory`], que valida, guarda a versão mais nova
//! de cada identidade e monta a lista de [`Candidate`] para o gerente de
//! circuitos.
//!
//! # Formato (v1)
//!
//! ```text
//! [versão 1][identidade 32][chave de circuito 32][banda kbps u32][emitido u64]
//! [ttl s u32][tam. addr u8][addr UTF-8][assinatura 64]
//! ```
//!
//! Inteiros em big-endian. A assinatura cobre `"clam-relay-desc-v2" || corpo`.
//!
//! # O que a assinatura garante
//!
//! - A chave de circuito e o endereço pertencem à identidade que assinou:
//!   ninguém troca a chave de um relay em trânsito.
//! - O descritor expira (`ttl`) e uma versão mais nova da mesma identidade
//!   substitui a antiga; um descritor velho reenviado não ressuscita.
//!
//! # O que ela **não** garante
//!
//! - Identidade não é pessoa: qualquer um gera quantas identidades quiser
//!   (ataque Sybil). O diretório limita o tamanho e o uso de cada endereço,
//!   mas a defesa de verdade é reputação e relays semeados pela equipe.
//! - A banda é declarada, não medida; por isso a pontuação a limita
//!   ([`BANDWIDTH_CAP_KBPS`]).

// O descritor chega da rede: nada de índice nem `unwrap` que derrube o processo.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use std::{collections::HashMap, net::IpAddr};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use thiserror::Error;

use crate::{bytes::Reader, circuit::manager::Candidate};

const VERSION: u8 = 2;
const DOMAIN: &[u8] = b"clam-relay-desc-v2";
const FIXED_LEN: usize = 1 + 32 + 32 + 32 + 4 + 8 + 4 + 1;
/// Maior endereço aceito. Cabe com folga no pedido de extensão do circuito.
pub const MAX_ADDR_LEN: usize = 100;
/// Banda acima disto não aumenta a pontuação (o valor é autodeclarado).
pub const BANDWIDTH_CAP_KBPS: u32 = 10_000;
/// Validade padrão ao assinar: 6 h.
pub const DEFAULT_TTL_SECS: u32 = 6 * 3600;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DescriptorError {
    #[error("descritor malformado")]
    Malformed,
    #[error("versão {0} não suportada")]
    Version(u8),
    #[error("assinatura inválida")]
    BadSignature,
    #[error("descritor expirado")]
    Expired,
    #[error("descritor emitido no futuro")]
    FromTheFuture,
    #[error("endereço inválido ou proibido")]
    BadAddress,
    #[error("validade fora do permitido")]
    BadTtl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayDescriptor {
    /// Identidade do relay: chave de verificação ed25519.
    pub identity: [u8; 32],
    /// Chave pública X25519 do circuito (handshake estilo ntor).
    pub onion_key: [u8; 32],
    /// Chave pública X25519 do link (Noise NK). Separada da `onion_key`: comprometer
    /// uma não revela a outra, e dá para girar o link sem mexer nos circuitos.
    pub link_key: [u8; 32],
    /// Banda declarada, em kbps.
    pub bandwidth_kbps: u32,
    /// Instante da emissão, segundos Unix. Versões mais novas substituem as antigas.
    pub issued_at: u64,
    pub ttl_secs: u32,
    /// `host:porta` onde o relay escuta.
    pub addr: String,
    signature: [u8; 64],
}

impl RelayDescriptor {
    /// Cria e assina um descritor.
    pub fn sign(
        identity: &SigningKey,
        onion_key: [u8; 32],
        link_key: [u8; 32],
        addr: &str,
        bandwidth_kbps: u32,
        issued_at: u64,
        ttl_secs: u32,
    ) -> Result<Self, DescriptorError> {
        if addr.is_empty() || addr.len() > MAX_ADDR_LEN {
            return Err(DescriptorError::BadAddress);
        }
        let mut d = Self {
            identity: identity.verifying_key().to_bytes(),
            onion_key,
            link_key,
            bandwidth_kbps,
            issued_at,
            ttl_secs,
            addr: addr.to_string(),
            signature: [0; 64],
        };
        d.signature = identity.sign(&d.signed_bytes()).to_bytes();
        Ok(d)
    }

    fn body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FIXED_LEN + self.addr.len());
        out.push(VERSION);
        out.extend_from_slice(&self.identity);
        out.extend_from_slice(&self.onion_key);
        out.extend_from_slice(&self.link_key);
        out.extend_from_slice(&self.bandwidth_kbps.to_be_bytes());
        out.extend_from_slice(&self.issued_at.to_be_bytes());
        out.extend_from_slice(&self.ttl_secs.to_be_bytes());
        out.push(self.addr.len() as u8);
        out.extend_from_slice(self.addr.as_bytes());
        out
    }

    fn signed_bytes(&self) -> Vec<u8> {
        let mut out = DOMAIN.to_vec();
        out.extend_from_slice(&self.body());
        out
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.body();
        out.extend_from_slice(&self.signature);
        out
    }

    /// Decodifica sem verificar a assinatura; use [`Self::verify`] em seguida.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, DescriptorError> {
        if buf.len() < FIXED_LEN + 64 {
            return Err(DescriptorError::Malformed);
        }
        let bad = || DescriptorError::Malformed;
        let mut r = Reader::new(buf);
        let version = r.u8().ok_or_else(bad)?;
        if version != VERSION {
            return Err(DescriptorError::Version(version));
        }
        let identity = r.array().ok_or_else(bad)?;
        let onion_key = r.array().ok_or_else(bad)?;
        let link_key = r.array().ok_or_else(bad)?;
        let bandwidth_kbps = r.u32().ok_or_else(bad)?;
        let issued_at = r.u64().ok_or_else(bad)?;
        let ttl_secs = r.u32().ok_or_else(bad)?;
        let addr_len = usize::from(r.u8().ok_or_else(bad)?);
        if addr_len == 0 || addr_len > MAX_ADDR_LEN {
            return Err(bad());
        }
        let addr = std::str::from_utf8(r.take(addr_len).ok_or_else(bad)?).map_err(|_| bad())?.to_string();
        let signature = r.array().ok_or_else(bad)?;
        if !r.is_empty() {
            return Err(bad());
        }
        Ok(Self { identity, onion_key, link_key, bandwidth_kbps, issued_at, ttl_secs, addr, signature })
    }

    /// Confere assinatura e validade em `now` (segundos Unix).
    /// `skew_secs`: tolerância a relógios adiantados.
    pub fn verify(&self, now: u64, skew_secs: u64) -> Result<(), DescriptorError> {
        let vk = VerifyingKey::from_bytes(&self.identity).map_err(|_| DescriptorError::Malformed)?;
        vk.verify(&self.signed_bytes(), &Signature::from_bytes(&self.signature))
            .map_err(|_| DescriptorError::BadSignature)?;
        if self.issued_at > now.saturating_add(skew_secs) {
            return Err(DescriptorError::FromTheFuture);
        }
        if now >= self.issued_at.saturating_add(self.ttl_secs as u64) {
            return Err(DescriptorError::Expired);
        }
        Ok(())
    }

    pub fn expires_at(&self) -> u64 {
        self.issued_at.saturating_add(self.ttl_secs as u64)
    }
}

/// `host:porta` com porta ≠ 0. Literais de IP loopback, privados, locais de
/// link, não especificados ou multicast só passam com `allow_private`.
fn address_ok(addr: &str, allow_private: bool) -> bool {
    let Some((host, port)) = addr.rsplit_once(':') else { return false };
    if host.is_empty() || !matches!(port.parse::<u16>(), Ok(p) if p != 0) {
        return false;
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.chars().any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == '@') {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => allow_private || is_public(ip),
        Err(_) => allow_private || !host.eq_ignore_ascii_case("localhost"),
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            !(v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified() || v.is_multicast() || v.is_broadcast())
        }
        IpAddr::V6(v) => {
            let seg0 = v.segments()[0];
            !(v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // unique local
                || (seg0 & 0xffc0) == 0xfe80) // link local
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DirectoryConfig {
    /// Descritores guardados no máximo. Padrão: 4096.
    pub max_entries: usize,
    /// Validade máxima aceita. Padrão: 24 h.
    pub max_ttl_secs: u32,
    /// Tolerância a relógio adiantado. Padrão: 300 s.
    pub skew_secs: u64,
    /// Aceita endereços privados/loopback (redes de teste). Padrão: `false`.
    pub allow_private_addrs: bool,
}

impl Default for DirectoryConfig {
    fn default() -> Self {
        Self { max_entries: 4096, max_ttl_secs: 24 * 3600, skew_secs: 300, allow_private_addrs: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// Descritor novo.
    New,
    /// Substituiu uma versão mais antiga da mesma identidade.
    Updated,
    /// Já havia versão igual ou mais nova.
    Stale,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InsertError {
    #[error(transparent)]
    Invalid(#[from] DescriptorError),
    #[error("o endereço já pertence a outra identidade")]
    AddressTaken,
    #[error("diretório cheio")]
    Full,
}

impl DirectoryConfig {
    /// Verificações sem estado: assinatura, validade, endereço. Serve a quem
    /// só precisa decidir se vale repassar um descritor (ex.: GossipSub).
    pub fn check(&self, d: &RelayDescriptor, now: u64) -> Result<(), DescriptorError> {
        d.verify(now, self.skew_secs)?;
        if d.ttl_secs == 0 || d.ttl_secs > self.max_ttl_secs {
            return Err(DescriptorError::BadTtl);
        }
        if !address_ok(&d.addr, self.allow_private_addrs) {
            return Err(DescriptorError::BadAddress);
        }
        Ok(())
    }

    /// [`Self::check`] sobre bytes recebidos.
    pub fn check_bytes(&self, buf: &[u8], now: u64) -> Result<RelayDescriptor, DescriptorError> {
        let d = RelayDescriptor::from_bytes(buf)?;
        self.check(&d, now)?;
        Ok(d)
    }
}

/// Guarda descritores verificados, um por identidade.
pub struct RelayDirectory {
    cfg: DirectoryConfig,
    by_id: HashMap<[u8; 32], RelayDescriptor>,
}

impl RelayDirectory {
    pub fn new(cfg: DirectoryConfig) -> Self {
        Self { cfg, by_id: HashMap::new() }
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Decodifica, valida e guarda um descritor recebido.
    pub fn insert_bytes(&mut self, buf: &[u8], now: u64) -> Result<Inserted, InsertError> {
        self.insert(RelayDescriptor::from_bytes(buf)?, now)
    }

    pub fn insert(&mut self, d: RelayDescriptor, now: u64) -> Result<Inserted, InsertError> {
        self.cfg.check(&d, now)?;
        self.expire(now);
        // Um endereço vale para uma identidade só (quem chegou primeiro, enquanto válido).
        if self.by_id.values().any(|o| o.addr == d.addr && o.identity != d.identity) {
            return Err(InsertError::AddressTaken);
        }
        match self.by_id.get(&d.identity) {
            Some(old) if old.issued_at >= d.issued_at => Ok(Inserted::Stale),
            Some(_) => {
                self.by_id.insert(d.identity, d);
                Ok(Inserted::Updated)
            }
            None if self.by_id.len() >= self.cfg.max_entries => Err(InsertError::Full),
            None => {
                self.by_id.insert(d.identity, d);
                Ok(Inserted::New)
            }
        }
    }

    /// Remove descritores vencidos.
    pub fn expire(&mut self, now: u64) {
        self.by_id.retain(|_, d| now < d.expires_at());
    }

    pub fn remove(&mut self, identity: &[u8; 32]) {
        self.by_id.remove(identity);
    }

    pub fn get(&self, identity: &[u8; 32]) -> Option<&RelayDescriptor> {
        self.by_id.get(identity)
    }

    /// Descritores válidos em `now`, no formato do gerente de circuitos.
    /// A pontuação cresce com o log da banda declarada, limitada a [`BANDWIDTH_CAP_KBPS`].
    pub fn candidates(&mut self, now: u64) -> Vec<Candidate> {
        self.expire(now);
        self.by_id
            .values()
            .map(|d| Candidate {
                addr: d.addr.clone(),
                public: d.onion_key,
                link_public: d.link_key,
                score: (1.0 + d.bandwidth_kbps.min(BANDWIDTH_CAP_KBPS) as f32).ln(),
            })
            .collect()
    }

    /// Descritores guardados, prontos para repassar a outros nós.
    pub fn export(&self) -> Vec<Vec<u8>> {
        self.by_id.values().map(RelayDescriptor::to_bytes).collect()
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::circuit::max_data;
    use rand_core::OsRng;

    const NOW: u64 = 1_800_000_000;

    fn key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn desc(k: &SigningKey, addr: &str, at: u64) -> RelayDescriptor {
        RelayDescriptor::sign(k, [7; 32], [8; 32], addr, 2_000, at, 3600).unwrap()
    }

    fn dir() -> RelayDirectory {
        RelayDirectory::new(DirectoryConfig::default())
    }

    #[test]
    fn round_trip_and_verify() {
        let d = desc(&key(), "203.0.113.5:4001", NOW);
        let back = RelayDescriptor::from_bytes(&d.to_bytes()).unwrap();
        assert_eq!(back, d);
        back.verify(NOW + 10, 300).unwrap();
    }

    #[test]
    fn any_tampered_byte_is_rejected() {
        let d = desc(&key(), "203.0.113.5:4001", NOW);
        let bytes = d.to_bytes();
        for i in 1..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 1;
            let ok = RelayDescriptor::from_bytes(&b).and_then(|x| x.verify(NOW, 300));
            assert!(ok.is_err(), "byte {i} adulterado passou");
        }
    }

    #[test]
    fn rejects_expired_future_and_bad_encodings() {
        let d = desc(&key(), "203.0.113.5:4001", NOW);
        assert_eq!(d.verify(NOW + 3600, 300), Err(DescriptorError::Expired));
        assert_eq!(d.verify(NOW - 1_000, 300), Err(DescriptorError::FromTheFuture));
        assert_eq!(RelayDescriptor::from_bytes(&[]), Err(DescriptorError::Malformed));
        let mut b = d.to_bytes();
        b[0] = 9;
        assert_eq!(RelayDescriptor::from_bytes(&b), Err(DescriptorError::Version(9)));
        let mut long = d.to_bytes();
        long.push(0);
        assert_eq!(RelayDescriptor::from_bytes(&long), Err(DescriptorError::Malformed));
    }

    #[test]
    fn newer_replaces_older_and_old_replay_is_stale() {
        let (mut dir, k) = (dir(), key());
        assert_eq!(dir.insert(desc(&k, "203.0.113.5:4001", NOW), NOW), Ok(Inserted::New));
        assert_eq!(dir.insert(desc(&k, "203.0.113.5:4001", NOW + 60), NOW + 60), Ok(Inserted::Updated));
        assert_eq!(dir.insert(desc(&k, "203.0.113.5:4001", NOW), NOW + 60), Ok(Inserted::Stale));
        assert_eq!(dir.len(), 1);
    }

    #[test]
    fn address_belongs_to_one_identity() {
        let mut dir = dir();
        dir.insert(desc(&key(), "203.0.113.5:4001", NOW), NOW).unwrap();
        assert_eq!(dir.insert(desc(&key(), "203.0.113.5:4001", NOW), NOW), Err(InsertError::AddressTaken));
    }

    #[test]
    fn private_and_malformed_addresses_are_refused_unless_allowed() {
        let mut strict = dir();
        for a in ["127.0.0.1:4001", "10.0.0.1:4001", "192.168.1.1:1", "[::1]:4001", "localhost:4001", "203.0.113.5:0", "sem-porta", "a b:1"] {
            assert!(
                matches!(strict.insert(desc(&key(), a, NOW), NOW), Err(InsertError::Invalid(DescriptorError::BadAddress))),
                "{a} deveria ser recusado"
            );
        }
        strict.insert(desc(&key(), "relay.exemplo.org:4001", NOW), NOW).unwrap();
        strict.insert(desc(&key(), "[2001:db8::1]:4001", NOW), NOW).unwrap();

        let mut lab = RelayDirectory::new(DirectoryConfig { allow_private_addrs: true, ..Default::default() });
        lab.insert(desc(&key(), "127.0.0.1:4001", NOW), NOW).unwrap();
    }

    #[test]
    fn ttl_cap_and_capacity_are_enforced() {
        let mut dir = RelayDirectory::new(DirectoryConfig { max_entries: 2, ..Default::default() });
        let long = RelayDescriptor::sign(&key(), [1; 32], [2; 32], "203.0.113.1:1", 1, NOW, 48 * 3600).unwrap();
        assert_eq!(dir.insert(long, NOW), Err(DescriptorError::BadTtl.into()));
        dir.insert(desc(&key(), "203.0.113.1:1", NOW), NOW).unwrap();
        dir.insert(desc(&key(), "203.0.113.2:1", NOW), NOW).unwrap();
        assert_eq!(dir.insert(desc(&key(), "203.0.113.3:1", NOW), NOW), Err(InsertError::Full));
    }

    #[test]
    fn expired_entries_leave_candidates_and_free_capacity() {
        let mut dir = dir();
        dir.insert(desc(&key(), "203.0.113.1:1", NOW), NOW).unwrap();
        assert_eq!(dir.candidates(NOW + 10).len(), 1);
        assert!(dir.candidates(NOW + 3600).is_empty());
        assert!(dir.is_empty());
    }

    #[test]
    fn score_grows_with_bandwidth_but_is_capped() {
        let mut dir = dir();
        for (i, bw) in [100u32, 5_000, 10_000, 4_000_000].into_iter().enumerate() {
            let d = RelayDescriptor::sign(&key(), [i as u8; 32], [i as u8 ^ 0x55; 32], &format!("203.0.113.{}:1", i + 1), bw, NOW, 3600).unwrap();
            dir.insert(d, NOW).unwrap();
        }
        let mut c = dir.candidates(NOW);
        c.sort_by(|a, b| a.score.total_cmp(&b.score));
        assert!(c[0].score < c[1].score && c[1].score < c[2].score);
        assert_eq!(c[2].score, c[3].score, "banda acima do teto não pode render mais pontos");
    }

    #[test]
    fn longest_address_fits_the_extend_request() {
        // Pedido de extensão: [x 32][chave 32][addr].
        assert!(64 + MAX_ADDR_LEN <= max_data(0));
    }
}
