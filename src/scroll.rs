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
//!
//! Janela que rola dentro da tela (menu lateral, barra de título e rodapé fixos) não é rolagem da página
//! inteira. Aí vale a **rolagem de região** ([`detect_region`]): só um retângulo desloca, o resto fica.
//!
//! ```text
//! tag (1) ‖ deslocamento i16 (2) ‖ x (2) ‖ y (2) ‖ largura (2) ‖ altura (2) ‖ largura da imagem (2) ‖ altura da imagem (2) ‖ faixa
//! ```

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
/// Primeiro byte de uma rolagem de região.
pub const REGION_TAG: u8 = 0xA7;
const REGION_HEAD: usize = 1 + 2 + 4 * 2 + 2 * 2;
/// A imagem é cortada em tantas colunas para achar o retângulo que rola.
const BANDS: usize = 16;
/// Linhas seguidas sem coincidir (cursor, destaque) que ainda não cortam a região.
const ROW_GAP: usize = 96;
/// Quantas linhas, no máximo, a ponta de uma região procura além da última prova de movimento.
const EDGE_SEARCH: usize = 64;
/// Fração mínima da imagem que a região precisa cobrir.
const MIN_AREA: f32 = 0.15;
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

/// Retângulo da imagem, em pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// Rolagem de região lida da rede.
#[derive(Debug, PartialEq, Eq)]
pub struct Region<'a> {
    pub rect: Rect,
    /// Como em [`Message::dy`].
    pub dy: i32,
    pub width: usize,
    pub height: usize,
    /// Faixa H.264 com as linhas novas no topo, com a largura do retângulo.
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

/// `data` é uma rolagem de região?
#[must_use]
pub fn is_region(data: &[u8]) -> bool {
    data.first() == Some(&REGION_TAG)
}

/// Monta uma rolagem de região. `None` se algum campo não cabe ou o retângulo não comporta o deslocamento.
#[must_use]
pub fn write_region(rect: Rect, dy: i32, width: usize, height: usize, strip: &[u8]) -> Option<Vec<u8>> {
    let n = dy.unsigned_abs() as usize;
    if dy == 0 || n > MAX_SHIFT || n >= rect.h || rect.w == 0 || rect.x.checked_add(rect.w)? > width || rect.y.checked_add(rect.h)? > height {
        return None;
    }
    let mut out = Vec::with_capacity(REGION_HEAD + strip.len());
    out.push(REGION_TAG);
    out.extend_from_slice(&i16::try_from(dy).ok()?.to_be_bytes());
    for v in [rect.x, rect.y, rect.w, rect.h, width, height] {
        out.extend_from_slice(&u16::try_from(v).ok()?.to_be_bytes());
    }
    out.extend_from_slice(strip);
    Some(out)
}

/// Lê uma rolagem de região; `None` se `data` não é uma ou está malformada.
#[must_use]
pub fn parse_region(data: &[u8]) -> Option<Region<'_>> {
    let mut b = data;
    if take::<1>(&mut b)? != [REGION_TAG] {
        return None;
    }
    let dy = i32::from(i16::from_be_bytes(take(&mut b)?));
    let mut v = [0usize; 6];
    for slot in &mut v {
        *slot = usize::from(u16::from_be_bytes(take(&mut b)?));
    }
    let [x, y, w, h, width, height] = v;
    let n = dy.unsigned_abs() as usize;
    if dy == 0 || n > MAX_SHIFT || n >= h || w == 0 || x + w > width || y + h > height || b.is_empty() {
        return None;
    }
    Some(Region { rect: Rect { x, y, w, h }, dy, width, height, strip: b })
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
    let dy = vote(&hp, &hc)?;
    let (overlap, same) = overlap_same(&hp, &hc, dy)?;
    (overlap as f32 >= cur.height as f32 * MIN_OVERLAP && same as f32 >= overlap as f32 * MIN_MATCH).then_some(dy)
}

/// Deslocamento em que mais linhas únicas de `hp` reaparecem em `hc`, se votos bastam.
fn vote(hp: &[u64], hc: &[u64]) -> Option<i32> {
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
    (n >= MIN_VOTES).then_some(dy)
}

/// Linhas de `hc` que ainda têm par em `hp` com o deslocamento `dy`, e quantas coincidem.
fn overlap_same(hp: &[u64], hc: &[u64], dy: i32) -> Option<(u32, u32)> {
    let height = i32::try_from(hc.len()).ok()?;
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
    Some((overlap, same))
}

/// Resumo das linhas de `frame` só nas colunas `x0..x1`.
fn hashes_cols(frame: &RawFrame, x0: usize, x1: usize) -> Vec<u64> {
    let stride = (frame.width * 4).max(4);
    frame.rgba.chunks_exact(stride).map(|row| row_hash_all(row.get(x0 * 4..x1 * 4).unwrap_or(&[]))).collect()
}

/// Como [`row_hash`], mas sem cortar a ponta: o recorte já é o que importa.
fn row_hash_all(row: &[u8]) -> u64 {
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

/// Retângulo que andou na vertical de `prev` para `cur` enquanto o resto da imagem ficou (janela que rola
/// com menu lateral, barra de título e rodapé fixos), e quanto andou. Só vale se a página inteira não rolou
/// ([`detect`] falhou) e o retângulo cobre ao menos [`MIN_AREA`] da imagem.
#[must_use]
pub fn detect_region(prev: &RawFrame, cur: &RawFrame) -> Option<(Rect, i32)> {
    if (prev.width, prev.height) != (cur.width, cur.height) || cur.height < MIN_HEIGHT || cur.width < BANDS || cur.rgba.len() != prev.rgba.len() {
        return None;
    }
    let edge = |b: usize| cur.width * b / BANDS;
    let cols: Vec<(Vec<u64>, Vec<u64>)> = (0..BANDS).map(|b| (hashes_cols(prev, edge(b), edge(b + 1)), hashes_cols(cur, edge(b), edge(b + 1)))).collect();
    // O deslocamento é o que mais colunas escolhem.
    let mut tally: HashMap<i32, u32> = HashMap::new();
    for (hp, hc) in &cols {
        if let Some(d) = vote(hp, hc) {
            *tally.entry(d).or_default() += 1;
        }
    }
    let (&dy, _) = tally.iter().max_by_key(|(d, n)| (**n, std::cmp::Reverse(d.unsigned_abs())))?;
    // Colunas que andaram junto: votam em `dy`, ou são lisas (as linhas coincidem quase todas).
    let ok: Vec<(bool, bool)> = cols
        .iter()
        .map(|(hp, hc)| {
            let Some((overlap, same)) = overlap_same(hp, hc, dy) else { return (false, false) };
            let voted = vote(hp, hc) == Some(dy);
            let fits = overlap > 0 && same as f32 >= overlap as f32 * if voted { MIN_MATCH } else { 0.95 };
            (fits, fits && voted)
        })
        .collect();
    let (mut best, mut start) = ((0usize, 0usize), 0usize);
    let mut voters = false;
    for b in 0..=BANDS {
        match ok.get(b) {
            Some(&(true, v)) => voters |= v,
            _ => {
                if voters && b - start > best.1 - best.0 {
                    best = (start, b);
                }
                start = b + 1;
                voters = false;
            }
        }
    }
    if best.1 <= best.0 {
        return None;
    }
    // As faixas de colunas são grossas: a borda do menu fixo cai no meio de uma. Pixel a pixel, uma coluna
    // é fixa quando seus pixels ficaram no lugar mais vezes do que andaram `dy`; coluna em branco empata
    // e não decide nada (é igual nos dois casos).
    // Só contam as linhas que a faixa do meio da corrida prova que andaram (única e coincidente): título e
    // rodapé ficam de fora, senão a coluna em branco ao lado do texto pareceria fixa.
    let (mp, mc) = cols.get((best.0 + best.1) / 2).map(|(p, c)| (p, c))?;
    let mut count: HashMap<u64, u32> = HashMap::new();
    for h in mp {
        *count.entry(*h).or_default() += 1;
    }
    let proof: Vec<(usize, usize)> = (0..cur.height)
        .filter_map(|y| {
            let py = usize::try_from(i32::try_from(y).ok()?.checked_add(dy)?).ok()?;
            let h = mp.get(py)?;
            (count.get(h) == Some(&1) && mc.get(y) == Some(h)).then_some((y, py))
        })
        .collect();
    // Positivo: a coluna anda com a região; negativo: fica; zero: indiferente (em branco).
    let lean = |c: usize| -> i64 {
        fn at(f: &RawFrame, row: usize, c: usize) -> Option<&[u8]> {
            let i = (row * f.width + c) * 4;
            f.rgba.get(i..i + 4)
        }
        let (mut moved, mut still) = (0u32, 0u32);
        for &(y, py) in &proof {
            let cur_px = at(cur, y, c);
            still += u32::from(at(prev, y, c) == cur_px);
            moved += u32::from(at(prev, py, c) == cur_px);
        }
        i64::from(moved) - i64::from(still)
    };
    let (mut x0, mut x1) = (edge(best.0), edge(best.1));
    while x0 > edge(best.0.saturating_sub(1)) && lean(x0 - 1) > 0 {
        x0 -= 1;
    }
    while x0 < x1 && lean(x0) < 0 {
        x0 += 1;
    }
    while x1 < edge((best.1 + 1).min(BANDS)) && lean(x1) > 0 {
        x1 += 1;
    }
    while x1 > x0 && lean(x1 - 1) < 0 {
        x1 -= 1;
    }
    if x1 <= x0 {
        return None;
    }
    // Extensão vertical. Só linha única do quadro anterior prova movimento: linha em branco coincide em
    // qualquer deslocamento e engoliria o menu e o rodapé fixos. As pontas não avançam por linhas em branco:
    // sem prova, uma margem em branco fica como estava (igual nos dois quadros) em vez de arriscar mover o
    // que é fixo.
    let (hp, hc) = (hashes_cols(prev, x0, x1), hashes_cols(cur, x0, x1));
    let mut count: HashMap<u64, u32> = HashMap::new();
    for h in &hp {
        *count.entry(*h).or_default() += 1;
    }
    let unique = |y: usize| hp.get(y).is_some_and(|h| count.get(h) == Some(&1));
    let height = i32::try_from(cur.height).ok()?;
    let moved = |y: usize| {
        let py = i32::try_from(y).unwrap_or(i32::MAX).saturating_add(dy);
        py >= 0 && py < height && usize::try_from(py).is_ok_and(|py| unique(py) && hp.get(py) == hc.get(y))
    };
    let mut clusters: Vec<(usize, usize, usize)> = Vec::new();
    for y in (0..cur.height).filter(|&y| moved(y)) {
        match clusters.last_mut() {
            Some(c) if y - c.1 <= ROW_GAP => {
                c.1 = y;
                c.2 += 1;
            }
            _ => clusters.push((y, y, 1)),
        }
    }
    let &(a, b, n) = clusters.iter().max_by_key(|c| c.2)?;
    let shift = dy.unsigned_abs() as usize;
    let (mut top, mut bottom) = if dy > 0 { (a, b + 1 + shift) } else { (a.checked_sub(shift)?, b + 1) };
    // Linha única que não saiu do lugar: é do que fica fixo, e as pontas do retângulo não passam dela.
    let fixed = |y: usize| unique(y) && hp.get(y) == hc.get(y);
    let low = (top.saturating_sub(EDGE_SEARCH)..top).rev().take_while(|&y| !fixed(y)).last().unwrap_or(top);
    let high = (bottom..(bottom + EDGE_SEARCH).min(cur.height)).take_while(|&y| !fixed(y)).last().map_or(bottom, |y| y + 1);
    // Entre a última prova de movimento e a primeira linha fixa só há linhas em branco, que não dizem de que
    // lado da borda estão: fica a ponta em que a imagem prevista erra menos linhas.
    let miss = |top: usize, bottom: usize| -> usize {
        (0..cur.height)
            .filter(|&y| {
                let pred = if y < top || y >= bottom {
                    hp.get(y)
                } else if dy > 0 && y + shift < bottom {
                    hp.get(y + shift)
                } else if dy < 0 && y >= top + shift {
                    hp.get(y - shift)
                } else {
                    hc.get(y)
                };
                pred != hc.get(y)
            })
            .count()
    };
    let mut best = miss(top, bottom);
    for b2 in bottom + 1..=high {
        let m = miss(top, b2);
        if m < best {
            (best, bottom) = (m, b2);
        }
    }
    for t2 in (low..top).rev() {
        let m = miss(t2, bottom);
        if m < best {
            (best, top) = (m, t2);
        }
    }
    let rect = Rect { x: x0, y: top, w: x1 - x0, h: bottom.checked_sub(top)? };
    let area = (rect.w * rect.h) as f32 / (cur.width * cur.height) as f32;
    (n as u32 >= MIN_VOTES && bottom <= cur.height && rect.h >= MIN_HEIGHT && shift < rect.h && area >= MIN_AREA).then_some((rect, dy))
}

/// A faixa de `cur` que a rolagem de `rect` por `dy` descobriu: as `|dy|` linhas novas do retângulo.
#[must_use]
pub fn strip_region(cur: &RawFrame, rect: Rect, dy: i32) -> Option<RawFrame> {
    let n = dy.unsigned_abs() as usize;
    let stride = cur.width * 4;
    if n == 0 || n > MAX_SHIFT || n >= rect.h || cur.rgba.len() != stride * cur.height || rect.x.checked_add(rect.w)? > cur.width || rect.y.checked_add(rect.h)? > cur.height {
        return None;
    }
    let first = if dy > 0 { rect.y + rect.h - n } else { rect.y };
    let mut rows = Vec::with_capacity(n * rect.w * 4);
    for y in first..first + n {
        rows.extend_from_slice(cur.rgba.get(y * stride + rect.x * 4..y * stride + (rect.x + rect.w) * 4)?);
    }
    padded(rect.w, &rows, n)
}

/// Desloca o retângulo de `canvas` por `dy` e cola as `|dy|` primeiras linhas de `strip` na ponta descoberta.
/// `false` (e `canvas` intacto) se as medidas não batem.
pub fn apply_region(canvas: &mut RawFrame, rect: Rect, dy: i32, strip: &RawFrame) -> bool {
    let n = dy.unsigned_abs() as usize;
    let stride = canvas.width * 4;
    let (Some(x_end), Some(y_end)) = (rect.x.checked_add(rect.w), rect.y.checked_add(rect.h)) else { return false };
    if n == 0 || n >= rect.h || rect.w == 0 || x_end > canvas.width || y_end > canvas.height || strip.width != rect.w || strip.height < n || canvas.rgba.len() != stride * canvas.height {
        return false;
    }
    let (row_bytes, x_off) = (rect.w * 4, rect.x * 4);
    let Some(new) = strip.rgba.get(..n * row_bytes) else { return false };
    let moved = rect.h - n;
    let copy = |canvas: &mut RawFrame, from: usize, to: usize| {
        canvas.rgba.copy_within(from * stride + x_off..from * stride + x_off + row_bytes, to * stride + x_off);
    };
    if dy > 0 {
        for r in 0..moved {
            copy(canvas, rect.y + r + n, rect.y + r);
        }
    } else {
        for r in (0..moved).rev() {
            copy(canvas, rect.y + r, rect.y + r + n);
        }
    }
    let first = if dy > 0 { rect.y + moved } else { rect.y };
    for (i, src) in new.chunks_exact(row_bytes).enumerate() {
        let at = (first + i) * stride + x_off;
        match canvas.rgba.get_mut(at..at + row_bytes) {
            Some(dst) => dst.copy_from_slice(src),
            None => return false,
        }
    }
    true
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

    /// Janela 320x240: menu fixo (x < 80), título (y < 24) e rodapé (y >= 216) fixos; o miolo rola `off`.
    fn window(off: usize) -> RawFrame {
        let fixed = page(320, 240, 7000);
        let moving = page(320, 240, off);
        let mut f = fixed.clone();
        for y in 24..216 {
            for x in 80..320 {
                let i = (y * 320 + x) * 4;
                f.rgba[i..i + 4].copy_from_slice(&moving.rgba[i..i + 4]);
            }
        }
        f
    }

    #[test]
    fn a_window_scrolling_inside_fixed_chrome_is_a_region() {
        let a = window(100);
        assert_eq!(detect(&a, &window(130)), None, "a página inteira não rolou");
        for dy in [30i32, -25, 1, -1] {
            let b = window((100 + dy) as usize);
            let (rect, got) = detect_region(&a, &b).unwrap_or_else(|| panic!("dy {dy}"));
            assert_eq!(got, dy);
            assert!(rect.x >= 64 && rect.x <= 96 && rect.x + rect.w == 320, "{rect:?}");
            assert_eq!((rect.y, rect.y + rect.h), (24, 216), "{rect:?}");
            let mut canvas = a.clone();
            let s = strip_region(&b, rect, dy).unwrap();
            assert!(apply_region(&mut canvas, rect, dy, &s));
            // Fora do retângulo nada muda; dentro, é a página rolada (o recorte pode ter engolido parte do menu).
            assert_eq!(canvas, b, "dy {dy}");
        }
        assert_eq!(detect_region(&a, &a), None, "imagem parada");
        assert_eq!(detect_region(&a, &window(100 + 5000)), None, "outro conteúdo");
    }

    #[test]
    fn region_apply_refuses_what_does_not_fit() {
        let mut canvas = page(320, 200, 0);
        let before = canvas.clone();
        let rect = Rect { x: 100, y: 20, w: 100, h: 100 };
        assert!(!apply_region(&mut canvas, rect, 0, &page(100, 32, 0)));
        assert!(!apply_region(&mut canvas, rect, 10, &page(90, 32, 0)), "largura");
        assert!(!apply_region(&mut canvas, rect, 40, &page(100, 32, 0)), "faixa baixa demais");
        assert!(!apply_region(&mut canvas, Rect { x: 300, ..rect }, 10, &page(100, 32, 0)), "fora da imagem");
        assert!(!apply_region(&mut canvas, rect, 100, &page(100, 128, 0)), "o retângulo todo");
        assert_eq!(canvas, before);
    }

    #[test]
    fn region_messages_round_trip_and_malformed_ones_are_refused() {
        let rect = Rect { x: 80, y: 24, w: 240, h: 192 };
        let m = write_region(rect, -25, 320, 240, &[1, 2, 3]).unwrap();
        assert!(is_region(&m) && !is_scroll(&m) && !is_patch(&m));
        assert_eq!(parse_region(&m), Some(Region { rect, dy: -25, width: 320, height: 240, strip: &[1, 2, 3] }));
        assert!(write_region(rect, 0, 320, 240, &[1]).is_none());
        assert!(write_region(rect, 192, 320, 240, &[1]).is_none(), "deslocamento do tamanho do retângulo");
        assert!(write_region(Rect { x: 200, ..rect }, 5, 320, 240, &[1]).is_none(), "fora da imagem");
        for cut in 0..m.len().min(REGION_HEAD + 1) {
            assert!(parse_region(&m[..cut]).is_none(), "cortada em {cut}");
        }
        assert!(parse_region(&[0xA5, 0, 1]).is_none());
    }

    /// Janela 640x240 com parágrafos separados por 40 linhas em branco e menu fixo até a coluna 100 (no
    /// meio de uma faixa de 40 colunas): o retângulo tem de sair exato, sem resíduo.
    fn sparse_window(off: usize) -> RawFrame {
        let (w, h) = (640, 240);
        let mut f = page(w, h, 7000);
        let moving = page(w, h, off);
        for y in 24..h - 24 {
            for x in 100..w {
                let i = (y * w + x) * 4;
                if (y + off) % 80 < 40 {
                    f.rgba[i..i + 4].copy_from_slice(&[250, 250, 250, 255]);
                } else {
                    f.rgba[i..i + 4].copy_from_slice(&moving.rgba[i..i + 4]);
                }
            }
        }
        f
    }

    #[test]
    fn a_sparse_window_with_a_side_menu_off_the_band_grid_has_no_residue() {
        let (a, b) = (sparse_window(800), sparse_window(860));
        let (rect, dy) = detect_region(&a, &b).expect("região");
        assert_eq!(dy, 60);
        let mut p = a.clone();
        let s = strip_region(&b, rect, dy).unwrap();
        assert!(apply_region(&mut p, rect, dy, &s));
        assert_eq!(p, b, "{rect:?}");
        assert!(rect.x >= 80 && rect.x <= 120, "{rect:?}");
    }
}
