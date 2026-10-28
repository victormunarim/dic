//! Cripto por salto: handshake X25519 com autenticação do relay (estilo ntor)
//! e camadas AEAD com contador explícito e janela anti-replay.
//!
//! O cliente conhece a chave estática `B` do relay (do descritor). Mistura-se
//! `DH(x, B)` (autentica o relay) com `DH(x, Y)` (sigilo adiante, `Y` efêmera).
//! Cada salto tem chaves próprias para ida e volta; o relay prova que possui
//! `B` com uma tag sobre a transcrição.

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit},
};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::OsRng;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::CircuitError;

const SALT: &[u8] = b"clam/v1/circuit";

#[derive(Zeroize, ZeroizeOnDrop)]
struct HopSecrets {
    fwd: [u8; 32],
    back: [u8; 32],
}

/// Chaves de um salto, de onde saem as camadas de cada lado.
pub struct HopKeys(HopSecrets);

impl HopKeys {
    /// Lado do cliente: cifra para o relay (`fwd`), decifra a volta (`back`).
    #[must_use]
    pub fn client_layers(&self) -> (LayerSender, LayerReceiver) {
        (LayerSender::new(self.0.fwd), LayerReceiver::new(self.0.back))
    }

    /// Lado do relay: decifra o que vem do cliente, cifra a volta.
    #[must_use]
    pub fn relay_layers(&self) -> (LayerReceiver, LayerSender) {
        (LayerReceiver::new(self.0.fwd), LayerSender::new(self.0.back))
    }
}

fn derive(ss1: &[u8; 32], ss2: &[u8; 32], x: &[u8; 32], y: &[u8; 32], b: &[u8; 32]) -> (HopKeys, [u8; 32]) {
    let mut ikm = Zeroizing::new(Vec::with_capacity(160));
    for part in [ss1, ss2, x, y, b] {
        ikm.extend_from_slice(part);
    }
    let hk = Hkdf::<Sha256>::new(Some(SALT), &ikm);
    let mut secrets = HopSecrets { fwd: [0; 32], back: [0; 32] };
    let mut auth = Zeroizing::new([0u8; 32]);
    hk.expand(b"fwd", &mut secrets.fwd).expect("32 bytes é válido");
    hk.expand(b"back", &mut secrets.back).expect("32 bytes é válido");
    hk.expand(b"auth", &mut *auth).expect("32 bytes é válido");

    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&*auth).expect("HMAC aceita qualquer tamanho de chave");
    mac.update(b"clam/v1/circuit/server");
    for part in [x, y, b] {
        mac.update(part);
    }
    (HopKeys(secrets), mac.finalize().into_bytes().into())
}

/// Lado do cliente do handshake de um salto.
pub struct ClientHandshake {
    secret: StaticSecret,
    public: PublicKey,
    relay: PublicKey,
}

impl ClientHandshake {
    #[must_use]
    pub fn new(relay_public: &[u8; 32]) -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public, relay: PublicKey::from(*relay_public) }
    }

    /// `X`: o que vai para o relay.
    #[must_use]
    pub fn public(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// Conclui com `Y` e a tag do relay. Falha se o relay não possui `B`.
    ///
    /// # Errors
    ///
    /// [`CircuitError::Handshake`] se `Y` é ponto fraco ou a tag não confere (o relay não possui `B`).
    pub fn finish(self, y: &[u8; 32], tag: &[u8; 32]) -> Result<HopKeys, CircuitError> {
        let y_pub = PublicKey::from(*y);
        let ss1 = self.secret.diffie_hellman(&self.relay);
        let ss2 = self.secret.diffie_hellman(&y_pub);
        if !(ss1.was_contributory() && ss2.was_contributory()) {
            return Err(CircuitError::Handshake);
        }
        let (keys, expected) =
            derive(ss1.as_bytes(), ss2.as_bytes(), self.public.as_bytes(), y, self.relay.as_bytes());
        if !bool::from(expected.ct_eq(tag)) {
            return Err(CircuitError::Handshake);
        }
        Ok(keys)
    }
}

pub struct ServerReply {
    pub y: [u8; 32],
    pub tag: [u8; 32],
    pub keys: HopKeys,
}

/// Lado do relay: responde ao `X` do cliente usando a chave estática do relay.
///
/// # Errors
///
/// [`CircuitError::Handshake`] se `x` é ponto fraco (ordem baixa).
pub fn server_handshake(relay_secret: &StaticSecret, x: &[u8; 32]) -> Result<ServerReply, CircuitError> {
    let x_pub = PublicKey::from(*x);
    let b_pub = PublicKey::from(relay_secret);
    let y_secret = StaticSecret::random_from_rng(OsRng);
    let y_pub = PublicKey::from(&y_secret);
    let ss1 = relay_secret.diffie_hellman(&x_pub);
    let ss2 = y_secret.diffie_hellman(&x_pub);
    if !(ss1.was_contributory() && ss2.was_contributory()) {
        return Err(CircuitError::Handshake);
    }
    let (keys, tag) = derive(ss1.as_bytes(), ss2.as_bytes(), x, y_pub.as_bytes(), b_pub.as_bytes());
    Ok(ServerReply { y: y_pub.to_bytes(), tag, keys })
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::clone_from_slice(&n)
}

/// Cifra uma camada: `[contador:8][AEAD(plaintext)]`. Cada chave só vê contadores novos.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct LayerSender {
    key: [u8; 32],
    counter: u64,
}

impl LayerSender {
    fn new(key: [u8; 32]) -> Self {
        Self { key, counter: 0 }
    }

    /// Cifra uma camada com o próximo nonce.
    ///
    /// # Errors
    ///
    /// [`CircuitError::CounterExhausted`] quando o contador de nonce acaba: o circuito precisa ser refeito.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, CircuitError> {
        if self.counter == u64::MAX {
            return Err(CircuitError::CounterExhausted);
        }
        let c = self.counter;
        self.counter += 1;
        let ct = ChaCha20Poly1305::new(Key::from_slice(&self.key))
            .encrypt(&nonce(c), plaintext)
            .map_err(|_| CircuitError::Decrypt)?;
        let mut out = Vec::with_capacity(8 + ct.len());
        out.extend_from_slice(&c.to_be_bytes());
        out.extend_from_slice(&ct);
        Ok(out)
    }
}

/// Janela deslizante de 64 contadores (como no IPsec).
#[derive(Default)]
struct ReplayWindow {
    top: Option<u64>,
    bits: u64,
}

impl ReplayWindow {
    fn accepts(&self, c: u64) -> bool {
        match self.top {
            None => true,
            Some(t) if c > t => true,
            Some(t) => {
                let d = t - c;
                d < 64 && (self.bits >> d) & 1 == 0
            }
        }
    }

    fn commit(&mut self, c: u64) {
        match self.top {
            Some(t) if c <= t => self.bits |= 1 << (t - c),
            Some(t) => {
                let shift = c - t;
                self.bits = if shift >= 64 { 0 } else { self.bits << shift } | 1;
                self.top = Some(c);
            }
            None => {
                self.top = Some(c);
                self.bits = 1;
            }
        }
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct LayerReceiver {
    key: [u8; 32],
    #[zeroize(skip)]
    window: ReplayWindow,
}

impl LayerReceiver {
    fn new(key: [u8; 32]) -> Self {
        Self { key, window: ReplayWindow::default() }
    }

    /// Verifica o replay, decifra e só então registra o contador.
    ///
    /// # Errors
    ///
    /// [`CircuitError::Replay`] se o contador já foi visto ou saiu da janela; [`CircuitError::Decrypt`] se a autenticação falha.
    #[deny(clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn open(&mut self, data: &[u8]) -> Result<Vec<u8>, CircuitError> {
        if data.len() < 8 + 16 {
            return Err(CircuitError::Decrypt);
        }
        let (counter, sealed) = data.split_first_chunk::<8>().ok_or(CircuitError::Decrypt)?;
        let c = u64::from_be_bytes(*counter);
        if !self.window.accepts(c) {
            return Err(CircuitError::Replay);
        }
        let pt = ChaCha20Poly1305::new(Key::from_slice(&self.key))
            .decrypt(&nonce(c), sealed)
            .map_err(|_| CircuitError::Decrypt)?;
        self.window.commit(c);
        Ok(pt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> (StaticSecret, [u8; 32]) {
        let s = StaticSecret::random_from_rng(OsRng);
        let p = PublicKey::from(&s).to_bytes();
        (s, p)
    }

    #[test]
    fn handshake_agrees_and_layers_work() {
        let (sec, public) = relay();
        let hs = ClientHandshake::new(&public);
        let reply = server_handshake(&sec, &hs.public()).unwrap();
        let ck = hs.finish(&reply.y, &reply.tag).unwrap();
        let (mut c_send, mut c_recv) = ck.client_layers();
        let (mut r_recv, mut r_send) = reply.keys.relay_layers();

        assert_eq!(r_recv.open(&c_send.seal(b"ida").unwrap()).unwrap(), b"ida");
        assert_eq!(c_recv.open(&r_send.seal(b"volta").unwrap()).unwrap(), b"volta");
    }

    #[test]
    fn wrong_relay_key_fails_auth() {
        let (sec, _) = relay();
        let (_, other_public) = relay();
        let hs = ClientHandshake::new(&other_public);
        let reply = server_handshake(&sec, &hs.public()).unwrap();
        assert_eq!(hs.finish(&reply.y, &reply.tag).err(), Some(CircuitError::Handshake));
    }

    #[test]
    fn tampered_tag_or_y_fails() {
        let (sec, public) = relay();
        let hs = ClientHandshake::new(&public);
        let reply = server_handshake(&sec, &hs.public()).unwrap();
        let mut tag = reply.tag;
        tag[0] ^= 1;
        let hs2 = ClientHandshake::new(&public);
        let reply2 = server_handshake(&sec, &hs2.public()).unwrap();
        assert!(hs.finish(&reply.y, &tag).is_err());
        let mut y = reply2.y;
        y[0] ^= 1;
        assert!(hs2.finish(&y, &reply2.tag).is_err());
    }

    #[test]
    fn rejects_low_order_points() {
        let (sec, public) = relay();
        assert_eq!(server_handshake(&sec, &[0; 32]).err(), Some(CircuitError::Handshake));
        let hs = ClientHandshake::new(&public);
        assert_eq!(hs.finish(&[0; 32], &[0; 32]).err(), Some(CircuitError::Handshake));
    }

    #[test]
    fn replay_and_tamper_are_rejected() {
        let (sec, public) = relay();
        let hs = ClientHandshake::new(&public);
        let reply = server_handshake(&sec, &hs.public()).unwrap();
        let (mut send, _) = hs.finish(&reply.y, &reply.tag).unwrap().client_layers();
        let (mut recv, _) = reply.keys.relay_layers();

        let m = send.seal(b"x").unwrap();
        assert!(recv.open(&m).is_ok());
        assert_eq!(recv.open(&m), Err(CircuitError::Replay));

        let mut bad = send.seal(b"y").unwrap();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(recv.open(&bad), Err(CircuitError::Decrypt));
    }

    #[test]
    fn window_accepts_reordering_but_not_old_or_duplicate() {
        let mut w = ReplayWindow::default();
        for c in [5, 3, 4, 100] {
            assert!(w.accepts(c));
            w.commit(c);
        }
        assert!(!w.accepts(100));
        assert!(!w.accepts(5));
        assert!(!w.accepts(30), "fora da janela de 64");
        assert!(w.accepts(99));
        assert!(w.accepts(101));
    }
}
