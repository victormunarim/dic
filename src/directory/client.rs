//! Lado cliente: publicar, consultar, salas, avisos.

use super::*;

/// Conexão de cliente com o diretório: lê o `hello` e já responde às provas de posse.
pub(super) struct Session {
    pub(super) s: TcpStream,
    pub(super) e_pub: [u8; 32],
    pub(super) seen_ip: String,
}

pub(super) async fn connect(server: &str) -> io::Result<Session> {
    let mut s = timeout(IO_TIMEOUT, TcpStream::connect(server)).await.map_err(|_| io::ErrorKind::TimedOut)??;
    let hello = read_frame(&mut s).await?;
    if hello.len() < 41 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "hello inválido"));
    }
    crate::clock::sync_to(u64::from_be_bytes(take(&hello[32..40])));
    Ok(Session { e_pub: take(&hello[..32]), seen_ip: String::from_utf8_lossy(&hello[40..]).into(), s })
}

pub(super) async fn ask(sess: &mut Session, req: &[u8]) -> io::Result<Vec<u8>> {
    write_frame(&mut sess.s, req).await?;
    read_frame(&mut sess.s).await
}

pub(super) fn bad(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

/// Publica (ou renova) a entrada de `secret` no diretório. `addr`: endereço público `ip:porta` do relay,
/// se o IP que o servidor enxerga não serve (NAT, VPN); `None` usa o IP visto pelo servidor e `relay_port`.
/// Devolve o endereço publicado.
pub async fn publish(server: &str, secret: &StaticSecret, relay_port: u16, onion: [u8; 32], link: [u8; 32], addr: Option<&str>) -> io::Result<String> {
    let mut sess = connect(server).await?;
    let me = PublicKey::from(secret);
    let addr = addr.unwrap_or("");
    if addr.len() > 60 {
        return Err(bad("endereço longo demais"));
    }
    let mut req = vec![REQ_PUBLISH];
    req.extend_from_slice(me.as_bytes());
    req.extend_from_slice(&onion);
    req.extend_from_slice(&link);
    req.extend_from_slice(&relay_port.to_be_bytes());
    req.push(addr.len() as u8);
    req.extend_from_slice(addr.as_bytes());
    let shared = secret.diffie_hellman(&PublicKey::from(sess.e_pub));
    req.extend_from_slice(&mac(shared.as_bytes(), &sess.e_pub, &req).finalize().into_bytes());
    let reply = ask(&mut sess, &req).await?;
    match reply.split_first() {
        Some((&RES_OK, a)) => Ok(String::from_utf8_lossy(a).into()),
        Some((&RES_ERR, m)) => Err(bad(&format!("diretório recusou: {}", String::from_utf8_lossy(m)))),
        _ => Err(bad("resposta inesperada")),
    }
}

/// Procura quem atende em `peer`.
pub async fn lookup(server: &str, peer: &[u8; 32]) -> io::Result<Option<RelayInfo>> {
    let mut sess = connect(server).await?;
    let reply = ask(&mut sess, &[&[REQ_LOOKUP][..], peer].concat()).await?;
    match reply.split_first() {
        Some((&RES_FOUND, b)) => Ok(Some(RelayInfo::decode(b).ok_or_else(|| bad("entrada inválida"))?.0)),
        Some((&RES_NONE, _)) => Ok(None),
        _ => Err(bad("resposta inesperada")),
    }
}

/// Até `n` relays sorteados, fora das chaves de `exclude` (no máximo 8 de cada vez).
pub async fn sample(server: &str, n: u8, exclude: &[[u8; 32]]) -> io::Result<Vec<RelayInfo>> {
    let mut sess = connect(server).await?;
    let mut req = vec![REQ_SAMPLE, n, exclude.len() as u8];
    for k in exclude {
        req.extend_from_slice(k);
    }
    let reply = ask(&mut sess, &req).await?;
    let Some((&RES_LIST, b)) = reply.split_first() else { return Err(bad("resposta inesperada")) };
    let (&count, mut rest) = b.split_first().ok_or_else(|| bad("curto"))?;
    let mut out = vec![];
    for _ in 0..count {
        let (info, r) = RelayInfo::decode(rest).ok_or_else(|| bad("entrada inválida"))?;
        out.push(info);
        rest = r;
    }
    Ok(out)
}

pub(super) async fn room_ask(server: &str, room: &ed25519_dalek::SigningKey, kind: u8, member: Option<&[u8; 32]>) -> io::Result<Vec<u8>> {
    let mut sess = connect(server).await?;
    let mut data = room.verifying_key().to_bytes().to_vec();
    data.extend_from_slice(member.map_or(&[][..], |m| &m[..]));
    let msg = [ROOM_CONTEXT, &sess.e_pub, &[kind], &data].concat();
    let sig = ed25519_dalek::Signer::sign(room, &msg);
    let mut req = vec![kind];
    req.extend_from_slice(&data);
    req.extend_from_slice(&sig.to_bytes());
    let reply = ask(&mut sess, &req).await?;
    match reply.first() {
        Some(&RES_ERR) => Err(bad(&format!("diretório recusou: {}", String::from_utf8_lossy(&reply[1..])))),
        Some(_) => Ok(reply),
        None => Err(bad("resposta vazia")),
    }
}

/// Registra (ou renova) `member` como presente na sala de chave `room`. Quem entra primeiro e continua
/// renovando (a cada [`REPUBLISH`]) é o anfitrião: veja [`room_members`].
pub async fn room_join(server: &str, room: &ed25519_dalek::SigningKey, member: &[u8; 32]) -> io::Result<()> {
    room_ask(server, room, REQ_ROOM_JOIN, Some(member)).await.map(|_| ())
}

pub async fn room_leave(server: &str, room: &ed25519_dalek::SigningKey, member: &[u8; 32]) -> io::Result<()> {
    room_ask(server, room, REQ_ROOM_LEAVE, Some(member)).await.map(|_| ())
}

/// Membros vivos da sala, do mais antigo (anfitrião) ao mais novo.
pub async fn room_members(server: &str, room: &ed25519_dalek::SigningKey) -> io::Result<Vec<[u8; 32]>> {
    let reply = room_ask(server, room, REQ_ROOM_LIST, None).await?;
    let (Some(&RES_MEMBERS), Some(&n)) = (reply.first(), reply.get(1)) else { return Err(bad("resposta inesperada")) };
    if reply.len() != 2 + 32 * n as usize {
        return Err(bad("lista inválida"));
    }
    Ok(reply[2..].chunks(32).map(take).collect())
}

/// O dono de uma sala (`owner`, a chave de identidade dele) deixa um aviso para `recipient` na sala `room`
/// (a chave pública da sala antiga). `notice` é o resultado de [`crate::room::seal_rekey`].
pub async fn notice_put(server: &str, owner: &StaticSecret, room: &[u8; 32], recipient: &[u8; 32], notice: &[u8]) -> io::Result<()> {
    if notice.len() != NOTICE_LEN {
        return Err(bad("aviso com tamanho errado"));
    }
    let mut sess = connect(server).await?;
    let mut req = vec![REQ_NOTICE_PUT];
    req.extend_from_slice(PublicKey::from(owner).as_bytes());
    req.extend_from_slice(room);
    req.extend_from_slice(recipient);
    req.extend_from_slice(notice);
    let shared = owner.diffie_hellman(&PublicKey::from(sess.e_pub));
    req.extend_from_slice(&mac(shared.as_bytes(), &sess.e_pub, &req).finalize().into_bytes());
    match ask(&mut sess, &req).await?.split_first() {
        Some((&RES_OK, _)) => Ok(()),
        Some((&RES_ERR, m)) => Err(bad(&format!("diretório recusou: {}", String::from_utf8_lossy(m)))),
        _ => Err(bad("resposta inesperada")),
    }
}

/// Aviso que o dono `owner` deixou para `recipient` na sala `room`, se houver.
pub async fn notice_get(server: &str, owner: &[u8; 32], room: &[u8; 32], recipient: &[u8; 32]) -> io::Result<Option<Vec<u8>>> {
    let mut sess = connect(server).await?;
    let req = [&[REQ_NOTICE_GET][..], owner, room, recipient].concat();
    let reply = ask(&mut sess, &req).await?;
    match reply.split_first() {
        Some((&RES_FOUND, b)) if b.len() == NOTICE_LEN => Ok(Some(b.to_vec())),
        Some((&RES_NONE, _)) => Ok(None),
        Some((&RES_ERR, m)) => Err(bad(&format!("diretório recusou: {}", String::from_utf8_lossy(m)))),
        _ => Err(bad("resposta inesperada")),
    }
}

/// Pede ao diretório que tente conectar ao relay que `secret` publicou. `true`: alcançável de fora.
/// Erra se nada foi publicado ainda ou se o teste foi pedido há pouco.
pub async fn reachable(server: &str, secret: &StaticSecret) -> io::Result<bool> {
    let mut sess = connect(server).await?;
    let mut req = vec![REQ_REACH];
    req.extend_from_slice(PublicKey::from(secret).as_bytes());
    let shared = secret.diffie_hellman(&PublicKey::from(sess.e_pub));
    req.extend_from_slice(&mac(shared.as_bytes(), &sess.e_pub, &req).finalize().into_bytes());
    let reply = ask(&mut sess, &req).await?;
    match reply.as_slice() {
        [RES_OK, v] => Ok(*v == 1),
        [RES_ERR, m @ ..] => Err(bad(&format!("servidor recusou: {}", String::from_utf8_lossy(m)))),
        _ => Err(bad("resposta inesperada")),
    }
}

/// IP que o servidor enxerga para esta máquina (útil para mostrar ao usuário).
pub async fn seen_ip(server: &str) -> io::Result<String> {
    Ok(connect(server).await?.seen_ip)
}
