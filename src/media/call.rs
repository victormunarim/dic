//! [`VoiceCall`]: uma chamada de voz pronta para ligar ao circuito.

use super::{
    MediaError,
    codec::Codec,
    jitter::{JitterBuffer, Playout},
    keys::CallKeys,
    packet::{Opened, Opener, PACKET_LEN, Sealer},
};

/// Contadores da chamada.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CallStats {
    pub sent: u64,
    pub received: u64,
    /// Pacotes que não autenticaram, repetidos ou de tamanho errado.
    pub rejected: u64,
    pub played: u64,
    pub concealed: u64,
    pub late: u64,
    /// Atraso de reprodução atual, em ms.
    pub delay_ms: u64,
}

/// Lado de uma chamada: cifra o que sai, decifra e reordena o que entra.
///
/// O relógio é do chamador (`now_ms`, qualquer base monotônica): chame [`playout`](Self::playout)
/// a cada `frame_ms` e despeje o PCM no alto-falante.
pub struct VoiceCall<C: Codec> {
    codec: C,
    sealer: Sealer,
    opener: Opener,
    jitter: JitterBuffer,
    sent: u64,
    received: u64,
    rejected: u64,
    concealed: u64,
    hung_up: bool,
}

impl<C: Codec> VoiceCall<C> {
    /// Atraso de reprodução entre 2 quadros e 200 ms.
    pub fn new(keys: &CallKeys, codec: C) -> Self {
        let f = codec.frame_ms();
        Self::with_delay(keys, codec, 2 * f, 200)
    }

    pub fn with_delay(keys: &CallKeys, codec: C, min_ms: u64, max_ms: u64) -> Self {
        let jitter = JitterBuffer::new(codec.frame_ms(), min_ms, max_ms);
        Self { codec, sealer: Sealer::new(keys), opener: Opener::new(keys), jitter, sent: 0, received: 0, rejected: 0, concealed: 0, hung_up: false }
    }

    /// Um quadro do microfone → um pacote de 200 B para `CircuitClient::send`.
    pub fn send(&mut self, pcm: &[i16]) -> Result<[u8; PACKET_LEN], MediaError> {
        let frame = self.codec.encode(pcm)?;
        let packet = self.sealer.seal(&frame)?;
        self.sent += 1;
        Ok(packet)
    }

    /// Pacote de desligar para o outro lado.
    pub fn hangup(&mut self) -> Result<[u8; PACKET_LEN], MediaError> {
        self.sealer.seal_hangup()
    }

    /// O outro lado mandou (e autenticou) o desligar.
    pub fn is_hung_up(&self) -> bool {
        self.hung_up
    }

    /// Um pacote que chegou (`Event::Data`) em `now_ms`.
    pub fn receive(&mut self, now_ms: u64, packet: &[u8]) -> Result<(), MediaError> {
        match self.opener.open(packet) {
            Ok((seq, Opened::Voice(frame))) => {
                self.received += 1;
                self.jitter.push(now_ms, seq, frame);
                Ok(())
            }
            Ok((_, Opened::Hangup)) => {
                self.hung_up = true;
                Ok(())
            }
            Err(e) => {
                self.rejected += 1;
                Err(e)
            }
        }
    }

    /// Quadros PCM vencidos em `now_ms`, em ordem; perdas já vêm escondidas pelo codec.
    pub fn playout(&mut self, now_ms: u64) -> Vec<Vec<i16>> {
        let mut out = vec![];
        while let Some(p) = self.jitter.poll(now_ms) {
            out.push(match p {
                Playout::Frame(d) => self.codec.decode(Some(&d)),
                Playout::Lost => {
                    self.concealed += 1;
                    self.codec.decode(None)
                }
            });
        }
        out
    }

    pub fn stats(&self) -> CallStats {
        let j = self.jitter.stats();
        CallStats { sent: self.sent, received: self.received, rejected: self.rejected, played: j.played, concealed: self.concealed, late: j.late, delay_ms: j.delay_ms }
    }
}
