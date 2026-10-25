//! Furo de NAT pelo diretório: UDP do servidor, ouvintes e pedidos.

use super::*;

/// Lê um quadro sem prazo (o controle fica parado até haver chamada).
pub(super) async fn read_frame_idle<R: AsyncReadExt + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// UDP do servidor: `[1] ‖ ficha (16)` → guarda o endereço de onde veio e responde `[2] ‖ ficha ‖ endereço (texto)`.
pub(super) async fn udp_loop(sock: Arc<UdpSocket>, punch: Arc<PunchState>) {
    let mut buf = [0u8; 64];
    loop {
        let Ok((n, src)) = sock.recv_from(&mut buf).await else { continue };
        if n != 17 || buf[0] != 1 {
            continue;
        }
        let tok: [u8; 16] = take(&buf[1..17]);
        let key = {
            let mut t = punch.tokens.lock().unwrap();
            let Some(e) = t.get_mut(&tok).filter(|e| e.at.elapsed() < TOKEN_TTL) else { continue };
            e.addr = Some(src);
            e.key
        };
        if let Some(k) = key
            && let Some(l) = punch.listeners.lock().unwrap().get_mut(&k)
            && l.token == tok
        {
            l.udp = Some(src);
        }
        let _ = sock.send_to(&[&[2u8][..], &tok, src.to_string().as_bytes()].concat(), src).await;
    }
}

pub(super) fn new_token(punch: &PunchState, key: Option<[u8; 32]>) -> Option<[u8; 16]> {
    let mut tok = [0u8; 16];
    OsRng.fill_bytes(&mut tok);
    let mut t = punch.tokens.lock().unwrap();
    t.retain(|_, e| e.at.elapsed() < TOKEN_TTL);
    if t.len() >= MAX_TOKENS {
        return None;
    }
    t.insert(tok, Token { addr: None, key, at: Instant::now() });
    Some(tok)
}

/// Preparar (`PREP`: devolve uma ficha para o cliente anunciar seu UDP) e pedir o furo (`GO`).
pub(super) fn punch_request(req: &[u8], punch: &PunchState) -> Vec<u8> {
    let err = |m: &str| [&[RES_ERR][..], m.as_bytes()].concat();
    match req[0] {
        REQ_PUNCH_PREP => match new_token(punch, None) {
            Some(tok) => [&[RES_OK][..], &tok].concat(),
            None => err("servidor ocupado"),
        },
        _ => {
            // ficha (16) ‖ destino (32)
            let (Ok(tok), Ok(target)) = (<[u8; 16]>::try_from(req.get(1..17).unwrap_or(&[])), <[u8; 32]>::try_from(req.get(17..).unwrap_or(&[]))) else {
                return err("tamanho");
            };
            let from = punch.tokens.lock().unwrap().get(&tok).filter(|e| e.at.elapsed() < TOKEN_TTL).and_then(|e| e.addr);
            let Some(from) = from else { return err("seu UDP não chegou ao servidor (firewall bloqueia UDP?)") };
            let ls = punch.listeners.lock().unwrap();
            let Some((udp, push)) = ls.get(&target).and_then(|l| l.udp.map(|u| (u, l.push.clone()))) else { return vec![RES_NONE] };
            if push.try_send(from).is_err() {
                return vec![RES_NONE];
            }
            [&[RES_OK][..], udp.to_string().as_bytes()].concat()
        }
    }
}

/// Teste de alcançabilidade: o servidor tenta abrir TCP no relay publicado por `pub` (exige a prova de posse
/// e respeita um intervalo mínimo por chave, para não servir de varredor de portas). Responde `OK ‖ 1|0`.
pub(super) async fn reach(req: &[u8], eph: &StaticSecret, e_pub: &[u8; 32], table: &Table, punch: &PunchState) -> Vec<u8> {
    let err = |m: &str| [&[RES_ERR][..], m.as_bytes()].concat();
    if req.len() != 1 + 32 + 32 {
        return err("tamanho");
    }
    let pubkey: [u8; 32] = take(&req[1..33]);
    let shared = eph.diffie_hellman(&PublicKey::from(pubkey));
    if !shared.was_contributory() || mac(shared.as_bytes(), e_pub, &req[..33]).verify_slice(&req[33..]).is_err() {
        return err("prova de posse inválida");
    }
    {
        let mut p = punch.probed.lock().unwrap();
        p.retain(|_, t| t.elapsed() < REACH_COOLDOWN);
        if p.len() >= MAX_ENTRIES || p.contains_key(&pubkey) {
            return err("tente de novo em instantes");
        }
        p.insert(pubkey, Instant::now());
    }
    let addr = table.lock().unwrap().get(&pubkey).filter(|e| e.seen.elapsed() < TTL).map(|e| e.info.addr.clone());
    let Some(addr) = addr else { return err("não publicado") };
    let ok = matches!(timeout(IO_TIMEOUT, TcpStream::connect(&addr)).await, Ok(Ok(_)));
    vec![RES_OK, ok as u8]
}

/// Conexão de um nó que aceita chamadas por furo de NAT: devolve a ficha UDP e depois avisa de cada chamador.
pub(super) async fn punch_listen(mut s: TcpStream, eph: &StaticSecret, e_pub: &[u8; 32], req: &[u8], punch: &PunchState) -> io::Result<()> {
    let refuse = |m: &str| [&[RES_ERR][..], m.as_bytes()].concat();
    if req.len() != 1 + 32 + 32 {
        return write_frame(&mut s, &refuse("tamanho")).await;
    }
    let pubkey: [u8; 32] = take(&req[1..33]);
    let shared = eph.diffie_hellman(&PublicKey::from(pubkey));
    if !shared.was_contributory() || mac(shared.as_bytes(), e_pub, &req[..33]).verify_slice(&req[33..]).is_err() {
        return write_frame(&mut s, &refuse("prova de posse inválida")).await;
    }
    let Some(token) = new_token(punch, Some(pubkey)) else { return write_frame(&mut s, &refuse("servidor ocupado")).await };
    let (tx, mut rx) = mpsc::channel(8);
    let id = OsRng.next_u64();
    let full = {
        let mut ls = punch.listeners.lock().unwrap();
        let full = ls.len() >= MAX_LISTENERS && !ls.contains_key(&pubkey);
        if !full {
            ls.insert(pubkey, Listener { id, token, udp: None, push: tx });
        }
        full
    };
    if full {
        return write_frame(&mut s, &refuse("servidor cheio")).await;
    }
    write_frame(&mut s, &[&[RES_OK][..], &token].concat()).await?;
    let mut tick = interval(REPUBLISH);
    tick.tick().await;
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            from = rx.recv() => {
                let Some(from) = from else { break };
                if write_frame(&mut s, &[&[CTL_PUNCH][..], from.to_string().as_bytes()].concat()).await.is_err() {
                    break;
                }
            }
            _ = tick.tick() => {
                if write_frame(&mut s, &[CTL_PING]).await.is_err() {
                    break;
                }
            }
            r = s.read(&mut probe) => {
                if !matches!(r, Ok(n) if n > 0) {
                    break;
                }
            }
        }
    }
    let mut ls = punch.listeners.lock().unwrap();
    if ls.get(&pubkey).is_some_and(|l| l.id == id) {
        ls.remove(&pubkey);
    }
    drop(ls);
    punch.tokens.lock().unwrap().remove(&token);
    Ok(())
}

/// Ouvinte de furos de NAT registrado no diretório (veja [`punch_register`]).
pub struct PunchListener {
    sess: Session,
    /// Ficha que o nó repete por UDP ao servidor para ele aprender (e manter aberto) o endereço externo.
    pub token: [u8; 16],
}

impl PunchListener {
    /// Espera o próximo chamador: o endereço UDP de onde ele vai sondar. Erra se a conexão cair.
    pub async fn next(&mut self) -> io::Result<SocketAddr> {
        loop {
            let f = timeout(CONTROL_IDLE, read_frame_idle(&mut self.sess.s)).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
            match f.first() {
                Some(&CTL_PING) => {}
                Some(&CTL_PUNCH) => return String::from_utf8_lossy(&f[1..]).parse().map_err(|_| bad("endereço inválido")),
                _ => return Err(bad("mensagem de controle desconhecida")),
            }
        }
    }
}

/// Passa a aceitar chamadas por furo de NAT: o servidor avisa a cada chamador novo.
pub async fn punch_register(server: &str, secret: &StaticSecret) -> io::Result<PunchListener> {
    let mut sess = connect(server).await?;
    let mut req = vec![REQ_PUNCH_LISTEN];
    req.extend_from_slice(PublicKey::from(secret).as_bytes());
    let shared = secret.diffie_hellman(&PublicKey::from(sess.e_pub));
    req.extend_from_slice(&mac(shared.as_bytes(), &sess.e_pub, &req).finalize().into_bytes());
    let reply = ask(&mut sess, &req).await?;
    match reply.split_first() {
        Some((&RES_OK, t)) => Ok(PunchListener { token: t.try_into().map_err(|_| bad("ficha inválida"))?, sess }),
        Some((&RES_ERR, m)) => Err(bad(&format!("servidor recusou: {}", String::from_utf8_lossy(m)))),
        _ => Err(bad("resposta inesperada")),
    }
}

/// Pede uma ficha para anunciar o UDP de quem vai chamar.
pub async fn punch_prep(server: &str) -> io::Result<[u8; 16]> {
    let mut sess = connect(server).await?;
    let reply = ask(&mut sess, &[REQ_PUNCH_PREP]).await?;
    match reply.split_first() {
        Some((&RES_OK, t)) => t.try_into().map_err(|_| bad("ficha inválida")),
        Some((&RES_ERR, m)) => Err(bad(&format!("servidor recusou: {}", String::from_utf8_lossy(m)))),
        _ => Err(bad("resposta inesperada")),
    }
}

/// Pede o furo até `target`: o servidor avisa `target` e devolve o endereço UDP dele (`None`: não aceita furos).
pub async fn punch_go(server: &str, token: &[u8; 16], target: &[u8; 32]) -> io::Result<Option<SocketAddr>> {
    let mut sess = connect(server).await?;
    let reply = ask(&mut sess, &[&[REQ_PUNCH_GO][..], token, target].concat()).await?;
    match reply.split_first() {
        Some((&RES_OK, a)) => Ok(Some(String::from_utf8_lossy(a).parse().map_err(|_| bad("endereço inválido"))?)),
        Some((&RES_NONE, _)) => Ok(None),
        Some((&RES_ERR, m)) => Err(bad(&format!("servidor recusou: {}", String::from_utf8_lossy(m)))),
        _ => Err(bad("resposta inesperada")),
    }
}
