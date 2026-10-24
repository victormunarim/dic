//! Círculos: tópicos de presença e de anúncio de call derivados do segredo do grupo.

use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

const SALT: &[u8] = b"clam/v1/circle";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopicKind {
    Presence,
    CallAnnounce,
}

impl TopicKind {
    fn label(self) -> &'static [u8] {
        match self {
            TopicKind::Presence => b"topic-presence",
            TopicKind::CallAnnounce => b"topic-call",
        }
    }
}

/// Segredo do círculo (ex.: exportado da época MLS). Só membros o possuem.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct CircleSecret([u8; 32]);

impl CircleSecret {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Nome de tópico opaco (hex). Quem não é membro não o deriva nem o reconhece.
    /// Gira com a época, como o endereço de presença.
    pub fn topic(&self, kind: TopicKind, epoch: u64) -> String {
        let hk = Hkdf::<Sha256>::new(Some(SALT), &self.0);
        let label = kind.label();
        let mut info = Vec::with_capacity(label.len() + 8);
        info.extend_from_slice(label);
        info.extend_from_slice(&epoch.to_be_bytes());
        let mut out = [0u8; 32];
        hk.expand(&info, &mut out).expect("32 bytes é válido");
        out.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_deterministic_and_distinct() {
        let s = CircleSecret::from_bytes([1; 32]);
        let t = s.topic(TopicKind::Presence, 5);
        assert_eq!(t, s.topic(TopicKind::Presence, 5));
        assert_ne!(t, s.topic(TopicKind::CallAnnounce, 5));
        assert_ne!(t, s.topic(TopicKind::Presence, 6));
        assert_ne!(t, CircleSecret::from_bytes([2; 32]).topic(TopicKind::Presence, 5));
        assert_eq!(t.len(), 64);
    }
}
