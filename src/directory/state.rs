//! Estado do servidor em disco.

use super::*;

/// De quanto em quanto tempo o estado vai para o disco.
pub const SAVE_EVERY: Duration = Duration::from_secs(5);
pub(super) const STATE_MAGIC: &[u8; 4] = b"CLD1";

pub(super) fn ago(t: Instant) -> u32 {
    t.elapsed().as_secs().min(u32::MAX as u64) as u32
}

/// Estado em disco: `CLD1 ‖ n ‖ (chave ‖ idade ‖ relay)… ‖ m ‖ (sala ‖ k ‖ (chave ‖ idade entrada ‖ idade visto)…)…`.
/// Guarda idades, não horas: o relógio do servidor pode mudar entre uma execução e outra.
pub(super) fn dump_state(table: &HashMap<[u8; 32], Entry>, rooms: &HashMap<[u8; 32], Vec<Member>>) -> Vec<u8> {
    let mut o = STATE_MAGIC.to_vec();
    o.extend_from_slice(&(table.len() as u32).to_be_bytes());
    for (k, e) in table {
        o.extend_from_slice(k);
        o.extend_from_slice(&ago(e.seen).to_be_bytes());
        e.info.encode(&mut o);
    }
    o.extend_from_slice(&(rooms.len() as u32).to_be_bytes());
    for (k, list) in rooms {
        o.extend_from_slice(k);
        o.extend_from_slice(&(list.len() as u16).to_be_bytes());
        for m in list {
            o.extend_from_slice(&m.key);
            o.extend_from_slice(&ago(m.joined).to_be_bytes());
            o.extend_from_slice(&ago(m.seen).to_be_bytes());
        }
    }
    o
}

/// Lê o que [`dump_state`] gravou; descarta o que já passou de [`TTL`] e para no primeiro defeito (arquivo
/// cortado ou de outra versão não derruba o servidor).
pub(super) fn load_state(b: &[u8], table: &mut HashMap<[u8; 32], Entry>, rooms: &mut HashMap<[u8; 32], Vec<Member>>) {
    pub(super) fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, rest) = b.split_at_checked(n)?;
        *b = rest;
        Some(head)
    }
    pub(super) fn num(b: &mut &[u8]) -> Option<u32> {
        Some(u32::from_be_bytes(take(b, 4)?.try_into().ok()?))
    }
    let back = |age: u32| Instant::now().checked_sub(Duration::from_secs(age as u64));
    let mut run = || -> Option<()> {
        let mut b = b.strip_prefix(&STATE_MAGIC[..])?;
        for _ in 0..num(&mut b)? {
            let key: [u8; 32] = take(&mut b, 32)?.try_into().ok()?;
            let age = num(&mut b)?;
            let (info, rest) = RelayInfo::decode(b)?;
            b = rest;
            if let Some(seen) = back(age).filter(|_| Duration::from_secs(age as u64) < TTL) {
                table.insert(key, Entry { info, seen });
            }
        }
        for _ in 0..num(&mut b)? {
            let key: [u8; 32] = take(&mut b, 32)?.try_into().ok()?;
            let n = u16::from_be_bytes(take(&mut b, 2)?.try_into().ok()?);
            let mut list = Vec::new();
            for _ in 0..n {
                let mk: [u8; 32] = take(&mut b, 32)?.try_into().ok()?;
                let (joined, seen) = (num(&mut b)?, num(&mut b)?);
                if let (Some(j), Some(s)) = (back(joined), back(seen))
                    && Duration::from_secs(seen as u64) < TTL
                {
                    list.push(Member { key: mk, joined: j, seen: s });
                }
            }
            if !list.is_empty() {
                rooms.insert(key, list);
            }
        }
        Some(())
    };
    let _ = run();
}
