//! Rolagem da tela: em vez de recodificar a página inteira a cada passo, manda "desloque N linhas" e só as
//! linhas que apareceram.
//!
//! O H.264 do openh264 não acha vetores de movimento grandes; numa rolagem rápida o quadro vira um
//! quadro-chave borrado e a imagem atrasa. Aqui quem transmite compara o quadro com o anterior
//! ([`detect`]), e se a página inteira andou na vertical manda uma mensagem de rolagem no lugar do
//! quadro de vídeo. Quem assiste desloca a imagem que já tem ([`apply`]) e cola a faixa nova.
//!
//! ```text
//! tag (1) ‖ deslocamento i16 (2) ‖ largura (2) ‖ altura (2) ‖ faixa: H.264 intra (SPS/PPS/IDR)
//! ```
//!
//! `largura` e `altura` são as da imagem de quem assiste: se não baterem, a mensagem é ignorada. A faixa
//! vai num codificador à parte, sempre intra, para não mexer nas referências do vídeo principal; por isso,
//! quando a rolagem acaba, o vídeo principal está atrás de quem assiste. Em vez de um quadro-chave inteiro,
//! o que mudou depois da rolagem (barra de rolagem, cursor, um botão) vai como **banda**: só as linhas que
//! mudaram, no mesmo tipo de faixa intra. O quadro-chave só sai se a mudança for grande.
//!
//! ```text
//! tag (1) ‖ y (2) ‖ linhas (2) ‖ largura (2) ‖ altura (2) ‖ faixa: H.264 intra (SPS/PPS/IDR)
//! ```
//!
//! A banda cola as `linhas` primeiras linhas da faixa decodificada a partir da linha `y` da imagem.

// `parse` lê bytes de quem transmite, que pode ser hostil: nada de índice nem `unwrap` que derrube o processo.
#![deny(clippy::indexing_slicing, clippy::unwrap_used)]

use std::collections::HashMap;

use crate::video::RawFrame;

/// Primeiro byte de uma mensagem de rolagem. Um quadro H.264 em byte-stream começa com `0x00`.
pub const TAG: u8 = 0xA5;
/// Maior deslocamento, em linhas, por mensagem.
pub const MAX_SHIFT: usize = 512;
const HEAD: usize = 1 + 2 + 2 + 2;
/// Primeiro byte de uma banda (linhas que mudaram depois de uma rolagem).
pub const PATCH_TAG: u8 = 0xA6;
const PATCH_HEAD: usize = 1 + 2 + 2 + 2 + 2;
/// Uma banda só vale se tem no máximo esta fração da altura; acima disso o quadro-chave sai mais barato.
const MAX_PATCH: f32 = 0.25;
/// Linhas menores que isso não valem a conta.
const MIN_HEIGHT: usize = 64;
/// Bytes do fim de cada linha fora da comparação: a barra de rolagem da janela anda sozinha.
const SKIP_TAIL: usize = 32 * 4;
/// Linhas únicas que precisam votar no mesmo deslocamento.
const MIN_VOTES: u32 = 6;
/// Fração das linhas sobrepostas que precisa coincidir exatamente com o deslocamento escolhido.
const MIN_MATCH: f32 = 0.7;
/// Fração mínima da altura que continua na tela depois do deslocamento.
const MIN_OVERLAP: f32 = 0.25;
/// Menor altura da faixa codificada (as alturas crescem em potências de dois, para reabrir pouco o codificador).
const MIN_STRIP: usize = 16;

/// Mensagem de rolagem lida da rede.
#[derive(Debug, PartialEq, Eq)]
pub struct Message<'a> {
    /// Positivo: o conteúdo subiu `dy` linhas (apareceram linhas embaixo); negativo: desceu.
    pub dy: i32,
    pub width: usize,
    pub height: usize,
    /// Faixa H.264 com as linhas novas no topo da imagem decodificada.
    pub strip: &'a [u8],
}

/// Banda lida da rede.
#[derive(Debug, PartialEq, Eq)]
pub struct Patch<'a> {
    /// Primeira linha da imagem que a banda substitui.
    pub y: usize,
    /// Quantas linhas da faixa valem.
    pub rows: usize,
    pub width: usize,
    pub height: usize,
    pub strip: &'a [u8],
}

fn take<const N: usize>(b: &mut &[u8]) -> Option<[u8; N]> {
    let (head, rest) = b.split_at_checked(N)?;
    *b = rest;
    head.try_into().ok()
}

/// `data` é uma mensagem de rolagem (e não um quadro H.264)?
#[must_use]
pub fn is_scroll(data: &[u8]) -> bool {
    data.first() == Some(&TAG)
}

/// `data` é uma banda?
#[must_use]
pub fn is_patch(data: &[u8]) -> bool {
    data.first() == Some(&PATCH_TAG)
}

/// Monta a mensagem. `None` se algum campo não cabe.
#[must_use]
pub fn write(dy: i32, width: usize, height: usize, strip: &[u8]) -> Option<Vec<u8>> {
    if dy == 0 || dy.unsigned_abs() as usize > MAX_SHIFT {
        return None;
    }
    let mut out = Vec::with_capacity(HEAD + strip.len());
    out.push(TAG);
    out.extend_from_slice(&i16::try_from(dy).ok()?.to_be_bytes());
    out.extend_from_slice(&u16::try_from(width).ok()?.to_be_bytes());
    out.extend_from_slice(&u16::try_from(height).ok()?.to_be_bytes());
    out.extend_from_slice(strip);
    Some(out)
}

/// Lê uma mensagem de rolagem; `None` se `data` não é uma ou está malformada.
#[must_use]
pub fn parse(data: &[u8]) -> Option<Message<'_>> {
    let mut b = data;
    if take::<1>(&mut b)? != [TAG] {
        return None;
    }
    let dy = i32::from(i16::from_be_bytes(take(&mut b)?));
    let width = usize::from(u16::from_be_bytes(take(&mut b)?));
    let height = usize::from(u16::from_be_bytes(take(&mut b)?));
    if dy == 0 || dy.unsigned_abs() as usize > MAX_SHIFT || b.is_empty() {
        return None;
    }
    Some(Message { dy, width, height, strip: b })
}

/// Monta uma banda. `None` se algum campo não cabe ou a banda é vazia.
#[must_use]
pub fn write_patch(y: usize, rows: usize, width: usize, height: usize, strip: &[u8]) -> Option<Vec<u8>> {
    if rows == 0 || y.checked_add(rows)? > height {
        return None;
    }
    let mut out = Vec::with_capacity(PATCH_HEAD + strip.len());
    out.push(PATCH_TAG);
    for v in [y, rows, width, height] {
        out.extend_from_slice(&u16::try_from(v).ok()?.to_be_bytes());
    }
    out.extend_from_slice(strip);
    Some(out)
}

/// Lê uma banda; `None` se `data` não é uma ou está malformada.
#[must_use]
pub fn parse_patch(data: &[u8]) -> Option<Patch<'_>> {
    let mut b = data;
    if take::<1>(&mut b)? != [PATCH_TAG] {
        return None;
    }
    let y = usize::from(u16::from_be_bytes(take(&mut b)?));
    let rows = usize::from(u16::from_be_bytes(take(&mut b)?));
    let width = usize::from(u16::from_be_bytes(take(&mut b)?));
    let height = usize::from(u16::from_be_bytes(take(&mut b)?));
    if rows == 0 || y + rows > height || b.is_empty() {
        return None;
    }
    Some(Patch { y, rows, width, height, strip: b })
}

/// Resumo de uma linha, sem a ponta direita.
fn row_hash(row: &[u8]) -> u64 {
    let row = if row.len() > SKIP_TAIL * 4 { row.get(..row.len() - SKIP_TAIL).unwrap_or(row) } else { row };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let (words, rest) = row.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
    }
    for &b in rest {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn hashes(frame: &RawFrame) -> Vec<u64> {
    frame.rgba.chunks_exact((frame.width * 4).max(4)).map(row_hash).collect()
}

/// Quanto a página andou na vertical de `prev` para `cur`, se andou como um todo (linhas inteiras idênticas,
/// só deslocadas). Positivo: o conteúdo subiu. Barra de rolagem na ponta direita é ignorada; colunas fixas
/// (menu lateral) não são: aí as linhas não coincidem e a rolagem não é reconhecida.
#[must_use]
pub fn detect(prev: &RawFrame, cur: &RawFrame) -> Option<i32> {
    if (prev.width, prev.height) != (cur.width, cur.height) || cur.height < MIN_HEIGHT || cur.rgba.len() != prev.rgba.len() {
        return None;
    }
    let (hp, hc) = (hashes(prev), hashes(cur));
    // Só linhas que aparecem uma vez no quadro anterior dizem de onde vieram (linhas em branco se repetem).
    let mut seen: HashMap<u64, (usize, u32)> = HashMap::new();
    for (i, h) in hp.iter().enumerate() {
        seen.entry(*h).or_insert((i, 0)).1 += 1;
    }
    let mut votes: HashMap<i32, u32> = HashMap::new();
    for (y, h) in hc.iter().enumerate() {
        if let Some(&(py, 1)) = seen.get(h) {
            let d = i32::try_from(py).ok()? - i32::try_from(y).ok()?;
            if d != 0 && d.unsigned_abs() as usize <= MAX_SHIFT {
                *votes.entry(d).or_default() += 1;
            }
        }
    }
    let (&dy, &n) = votes.iter().max_by_key(|(d, v)| (**v, std::cmp::Reverse(d.unsigned_abs())))?;
    if n < MIN_VOTES {
        return None;
    }
    let height = i32::try_from(cur.height).ok()?;
    let (mut overlap, mut same) = (0u32, 0u32);
    for (y, h) in hc.iter().enumerate() {
        let py = i32::try_from(y).ok()? + dy;
        if py < 0 || py >= height {
            continue;
        }
        overlap += 1;
        if hp.get(usize::try_from(py).ok()?) == Some(h) {
            same += 1;
        }
    }
    (overlap as f32 >= cur.height as f32 * MIN_OVERLAP && same as f32 >= overlap as f32 * MIN_MATCH).then_some(dy)
}

/// A faixa de `cur` que a rolagem `dy` descobriu, com altura arredondada para cima (potência de dois) repetindo
/// a última linha: as linhas novas ficam no topo e só elas valem.
#[must_use]
pub fn strip(cur: &RawFrame, dy: i32) -> Option<RawFrame> {
    let n = dy.unsigned_abs() as usize;
    let stride = cur.width * 4;
    if n == 0 || n > MAX_SHIFT || n >= cur.height || cur.rgba.len() != stride * cur.height {
        return None;
    }
    let rows = if dy > 0 { cur.rgba.get((cur.height - n) * stride..)? } else { cur.rgba.get(..n * stride)? };
    padded(cur.width, rows, n)
}

/// `rows` (`n` linhas de `width` pixels) com altura arredondada para cima, repetindo a última linha.
fn padded(width: usize, rows: &[u8], n: usize) -> Option<RawFrame> {
    let stride = width * 4;
    let height = n.next_power_of_two().max(MIN_STRIP);
    let mut rgba = Vec::with_capacity(height * stride);
    rgba.extend_from_slice(rows);
    let last = rows.get(rows.len().checked_sub(stride)?..)?;
    for _ in n..height {
        rgba.extend_from_slice(last);
    }
    RawFrame::new(width, height, rgba).ok()
}

/// Faixa contínua de linhas que difere entre `prev` e `cur`: `(primeira, quantas)`. `None` se as imagens são
/// iguais, têm medidas diferentes, ou a faixa é grande demais para valer mais que um quadro-chave.
#[must_use]
pub fn changed_rows(prev: &RawFrame, cur: &RawFrame) -> Option<(usize, usize)> {
    let stride = cur.width * 4;
    if (prev.width, prev.height) != (cur.width, cur.height) || stride == 0 || cur.rgba.len() != stride * cur.height || prev.rgba.len() != cur.rgba.len() {
        return None;
    }
    let mut diff = prev.rgba.chunks_exact(stride).zip(cur.rgba.chunks_exact(stride)).map(|(a, b)| a != b).enumerate().filter(|(_, d)| *d);
    let first = diff.next()?.0;
    let last = diff.next_back().map_or(first, |(i, _)| i);
    let rows = last - first + 1;
    (rows as f32 <= cur.height as f32 * MAX_PATCH && rows <= MAX_SHIFT).then_some((first, rows))
}

/// A faixa de `cur` com as `rows` linhas a partir de `y`, pronta para o codificador de faixas.
#[must_use]
pub fn band(cur: &RawFrame, y: usize, rows: usize) -> Option<RawFrame> {
    let stride = cur.width * 4;
    if rows == 0 || cur.rgba.len() != stride * cur.height {
        return None;
    }
    padded(cur.width, cur.rgba.get(y * stride..y.checked_add(rows)? * stride)?, rows)
}

/// Cola as `rows` primeiras linhas de `strip` em `canvas` a partir da linha `y`. `false` (e `canvas` intacto)
/// se as medidas não batem.
pub fn apply_patch(canvas: &mut RawFrame, y: usize, rows: usize, strip: &RawFrame) -> bool {
    let stride = canvas.width * 4;
    if rows == 0 || strip.width != canvas.width || strip.height < rows || canvas.rgba.len() != stride * canvas.height {
        return false;
    }
    let (Some(new), Some(end)) = (strip.rgba.get(..rows * stride), y.checked_add(rows)) else { return false };
    match canvas.rgba.get_mut(y * stride..end * stride) {
        Some(dst) => {
            dst.copy_from_slice(new);
            true
        }
        None => false,
    }
}

/// Desloca `canvas` por `dy` e cola as `|dy|` primeiras linhas de `strip` na ponta que ficou descoberta.
/// `false` (e `canvas` intacto) se as medidas não batem.
pub fn apply(canvas: &mut RawFrame, dy: i32, strip: &RawFrame) -> bool {
    let n = dy.unsigned_abs() as usize;
    let stride = canvas.width * 4;
    if n == 0 || n >= canvas.height || strip.width != canvas.width || strip.height < n || canvas.rgba.len() != stride * canvas.height {
        return false;
    }
    let cut = n * stride;
    let total = canvas.rgba.len();
    let Some(new) = strip.rgba.get(..cut) else { return false };
    if dy > 0 {
        canvas.rgba.copy_within(cut.., 0);
        match canvas.rgba.get_mut(total - cut..) {
            Some(dst) => dst.copy_from_slice(new),
            None => return false,
        }
    } else {
        canvas.rgba.copy_within(..total - cut, cut);
        match canvas.rgba.get_mut(..cut) {
            Some(dst) => dst.copy_from_slice(new),
            None => return false,
        }
    }
    true
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Página em que cada linha é diferente das outras e, dentro dela, os pixels variam.
    fn page(w: usize, h: usize, off: usize) -> RawFrame {
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let sy = y + off;
            for x in 0..w {
                let v = ((sy * 2654435761usize).wrapping_add(x * 40503) >> 8) as u8;
                rgba.extend_from_slice(&[v, v ^ 0x5a, sy as u8, 255]);
            }
        }
        RawFrame::new(w, h, rgba).unwrap()
    }

    #[test]
    fn a_scrolled_page_is_recognised_in_both_directions() {
        let a = page(320, 200, 100);
        assert_eq!(detect(&a, &page(320, 200, 137)), Some(37));
        assert_eq!(detect(&a, &page(320, 200, 60)), Some(-40));
        assert_eq!(detect(&a, &a), None, "imagem parada não é rolagem");
        assert_eq!(detect(&a, &page(320, 200, 100 + 5000)), None, "outra página");
        assert_eq!(detect(&a, &page(160, 200, 137)), None, "outra largura");
    }

    #[test]
    fn the_scrollbar_column_does_not_hide_the_scroll() {
        let a = page(320, 200, 100);
        let mut b = page(320, 200, 130);
        for y in 0..200 {
            b.rgba[(y * 320 + 316) * 4] = 7;
        }
        assert_eq!(detect(&a, &b), Some(30));
    }

    #[test]
    fn apply_reproduces_the_scrolled_page() {
        for dy in [37, -40, 1, -1, 200 - 1] {
            let a = page(320, 200, 100);
            let b = page(320, 200, (100 + dy) as usize);
            let mut canvas = a.clone();
            let s = strip(&b, dy).unwrap();
            assert!(s.height.is_power_of_two() && s.height >= dy.unsigned_abs() as usize);
            assert!(apply(&mut canvas, dy, &s), "dy {dy}");
            assert_eq!(canvas, b, "dy {dy}");
        }
    }

    #[test]
    fn apply_refuses_what_does_not_fit() {
        let mut canvas = page(320, 200, 0);
        let before = canvas.clone();
        assert!(!apply(&mut canvas, 0, &page(320, 32, 0)));
        assert!(!apply(&mut canvas, 20, &page(160, 32, 0)), "largura");
        assert!(!apply(&mut canvas, 40, &page(320, 32, 0)), "faixa baixa demais");
        assert!(!apply(&mut canvas, 200, &page(320, 256, 0)), "a imagem toda");
        assert_eq!(canvas, before);
    }

    #[test]
    fn message_round_trips_and_malformed_ones_are_refused() {
        let msg = write(-37, 1920, 1080, b"faixa").unwrap();
        assert_eq!(parse(&msg), Some(Message { dy: -37, width: 1920, height: 1080, strip: b"faixa" }));
        assert!(write(0, 10, 10, b"x").is_none());
        assert!(write(MAX_SHIFT as i32 + 1, 10, 10, b"x").is_none());
        assert!(write(5, 70_000, 10, b"x").is_none());
        for cut in 0..msg.len().min(HEAD) {
            assert!(parse(&msg[..cut]).is_none(), "cortado em {cut}");
        }
        assert!(parse(&msg[..HEAD]).is_none(), "sem faixa");
        assert!(parse(&[0, 0, 0, 1, 0x65]).is_none(), "quadro H.264");
        assert!(parse(&[TAG, 0, 0, 0, 1, 0, 1, 9]).is_none(), "deslocamento zero");
    }

    #[test]
    fn a_patch_reproduces_the_changed_rows_only() {
        let a = page(320, 200, 100);
        let mut b = a.clone();
        for y in 50..58 {
            b.rgba[(y * 320 + 10) * 4] ^= 0xff;
        }
        assert_eq!(changed_rows(&a, &a), None);
        assert_eq!(changed_rows(&a, &b), Some((50, 8)));
        let mut c = a.clone();
        for y in 10..150 {
            c.rgba[y * 320 * 4] ^= 0xff;
        }
        assert_eq!(changed_rows(&a, &c), None, "mudança grande vale um quadro-chave");
        let s = band(&b, 50, 8).unwrap();
        let mut canvas = a.clone();
        assert!(apply_patch(&mut canvas, 50, 8, &s));
        assert_eq!(canvas, b);
        assert!(!apply_patch(&mut canvas, 195, 8, &s), "passa do fim");
        assert!(!apply_patch(&mut canvas, 50, 0, &s));
    }

    #[test]
    fn patch_round_trips_and_malformed_ones_are_refused() {
        let msg = write_patch(40, 8, 1920, 1080, b"faixa").unwrap();
        assert_eq!(parse_patch(&msg), Some(Patch { y: 40, rows: 8, width: 1920, height: 1080, strip: b"faixa" }));
        assert!(is_patch(&msg) && !is_scroll(&msg));
        assert!(write_patch(1075, 8, 1920, 1080, b"x").is_none());
        assert!(write_patch(0, 0, 10, 10, b"x").is_none());
        for cut in 0..=PATCH_HEAD {
            assert!(parse_patch(msg.get(..cut).unwrap()).is_none(), "cortado em {cut}");
        }
        assert!(parse_patch(&[PATCH_TAG, 0, 9, 0, 3, 0, 4, 0, 10, 1]).is_none(), "passa da altura");
        assert!(parse(&msg).is_none() && parse_patch(&[TAG, 0, 0, 0, 1, 0, 1, 0, 1, 9]).is_none());
    }
}
