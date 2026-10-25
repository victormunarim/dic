//! Salas: estado de uma sala em andamento e o segredo que o dono troca.

use super::*;

/// Sala em andamento na tarefa do nó.
pub(crate) struct RoomRun {
    pub(crate) name: String,
    pub(crate) secret: [u8; 32],
    pub(crate) owner: Option<[u8; 32]>,
    pub(crate) key: clam::ed25519_dalek::SigningKey,
    pub(crate) mesh: Mesh,
    pub(crate) view: Option<RoomView>,
    pub(crate) sound: Option<Sound>,
    pub(crate) next_sync: tokio::time::Instant,
    pub(crate) next_dial: tokio::time::Instant,
    /// O que o último resumo mostrou (membros, conectados, anfitrião), para só repetir quando mudar.
    pub(crate) shown: (usize, usize, [u8; 32]),
    pub(crate) n: usize,
    /// Um decodificador por membro que transmite a tela.
    pub(crate) decoders: std::collections::HashMap<[u8; 32], Option<clam::video::H264Decoder>>,
}

/// Segue as trocas de segredo que o dono `owner` deixou no diretório, até não haver mais (no máximo 8).
/// Cada troca vai para `shared.rekeyed`, para a janela salvar o segredo novo.
pub(crate) async fn follow_chain(directory: &str, me: &StaticSecret, owner: &[u8; 32], mut secret: [u8; 32], shared: &Shared) -> [u8; 32] {
    for _ in 0..8 {
        match room::follow(directory, me, owner, &secret).await {
            Ok(Some(new)) if new != secret => {
                shared.rekeyed.lock().unwrap().push((secret, new));
                secret = new;
            }
            _ => break,
        }
    }
    secret
}

pub(crate) fn member_name(contacts: &[(String, [u8; 32])], key: &[u8; 32]) -> String {
    contacts.iter().find(|(_, k)| k == key).map(|(n, _)| n.clone()).unwrap_or_else(|| hex(key)[..8].to_string())
}

pub(crate) async fn leave_room(node: &Node, r: RoomRun, shared: &Shared, out: &Sink) {
    r.mesh.hangup();
    shared.screen.reset();
    let _ = room::leave(node, &r.key).await;
    out.line(format!("saiu da sala {}", r.name));
    shared.room.lock().unwrap().clear();
    shared.members.lock().unwrap().clear();
    *shared.mesh.lock().unwrap() = None;
    shared.in_room.store(false, Ordering::Relaxed);
    shared.in_call.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn follow_chain_walks_every_rotation_and_skips_removed_members() {
        use clam::{directory::DirectoryServer, rand_core::OsRng, x25519_dalek::PublicKey};
        let d = DirectoryServer::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = d.local_addr().to_string();
        let (owner, bob, eve) = (StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng), StaticSecret::random_from_rng(OsRng));
        let (opub, bpub, epub) = (PublicKey::from(&owner).to_bytes(), PublicKey::from(&bob).to_bytes(), PublicKey::from(&eve).to_bytes());
        let first = room::new_secret();
        let second = room::rotate(&addr, &owner, &first, &[bpub, epub]).await.unwrap();
        let third = room::rotate(&addr, &owner, &second, &[bpub]).await.unwrap();
        let shared = Shared::default();
        assert_eq!(follow_chain(&addr, &bob, &opub, first, &shared).await, third);
        assert_eq!(shared.rekeyed.lock().unwrap().len(), 2);
        // Eve foi removida na segunda troca: fica presa no segredo do meio.
        assert_eq!(follow_chain(&addr, &eve, &opub, first, &Shared::default()).await, second);
    }

    #[test]
    fn member_name_prefers_contact_names() {
        let contacts = vec![("ana".to_string(), [1u8; 32])];
        assert_eq!(member_name(&contacts, &[1; 32]), "ana");
        assert_eq!(member_name(&contacts, &[2; 32]), "02020202");
    }
}
