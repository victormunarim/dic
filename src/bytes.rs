//! Leitor de bytes sem pânico, para decodificar o que chega da rede.
//!
//! Cada leitura devolve `None` se faltam bytes: quem decodifica troca o `None` pelo erro do seu formato, sem
//! índice nem `unwrap` no caminho.

// Existe para tirar índice e `unwrap` dos leitores: nada aqui pode usá-los.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

/// Cursor sobre um trecho de bytes que só anda para a frente.
#[derive(Clone, Copy)]
pub(crate) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    /// Os próximos `n` bytes, ou `None` (e nada é consumido) se faltam.
    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_be_bytes)
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_be_bytes)
    }

    pub(crate) fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_be_bytes)
    }

    /// Tudo o que falta ler, sem consumir.
    pub(crate) fn rest(&self) -> &'a [u8] {
        self.0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_in_order_and_refuses_to_run_past_the_end() {
        let mut r = Reader::new(&[1, 0, 2, 0, 0, 0, 3, 9]);
        assert_eq!((r.u8(), r.u16(), r.u32()), (Some(1), Some(2), Some(3)));
        assert_eq!(r.rest(), &[9]);
        assert_eq!(r.take(2), None, "faltando, não consome");
        assert_eq!(r.rest(), &[9]);
        assert_eq!(r.array::<1>(), Some([9]));
        assert!(r.is_empty() && r.u8().is_none() && r.u64().is_none());
    }
}
