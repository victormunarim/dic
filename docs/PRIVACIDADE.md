# Pareamento, presença e busca (Fases 1 e 2)

Passo a passo da biblioteca: parear, publicar presença, procurar contatos, círculos e publicação automática.

[← Voltar ao README](../README.md)

## Passo a passo

Todos os exemplos abaixo estão reunidos em [`examples/quickstart.rs`](../examples/quickstart.rs).

### 1. Parear e derivar as chaves

Cada lado guarda seu `StaticSecret` e recebe a chave pública do outro (por QR code, por exemplo).
O dono da presença também gera uma chave de assinatura ed25519 e entrega a chave de verificação ao contato no pareamento.

```rust
use clam::{pairing::PresenceKeys, ed25519_dalek::SigningKey, rand_core::OsRng,
           x25519_dalek::{PublicKey, StaticSecret}};

let meu_segredo = StaticSecret::random_from_rng(OsRng);
let assinador = SigningKey::generate(&mut OsRng);
// `chave_publica_do_contato` chegou no pareamento.
let chaves = PresenceKeys::derive(&meu_segredo, &chave_publica_do_contato);
```

`derive` é simétrico: os dois lados obtêm as mesmas chaves. As chaves se apagam da memória ao sair de escopo.

### 2. Publicar a sua presença

```rust
use clam::{service::{self, PresenceConfig}, store::RecordStore};

let cfg = PresenceConfig::default();           // época de 3600 s, 3 consultas falsas
let intros = vec![b"relay-a.example:4001".to_vec()]; // até 8 pontos, de 1 a 64 bytes cada
service::publish(&store, &cfg, &chaves, &assinador, agora_unix, &intros)?;
```

Chame `publish` de novo **a cada período** e sempre que os pontos de introdução mudarem. Um registro por contato: use as `PresenceKeys` de cada um.
Erros possíveis: `PresenceError::TooManyIntroPoints` e `PresenceError::BadIntroPoint`.

### 3. Procurar um contato

```rust
let achou = service::lookup(&store, &cfg, &chaves, &verificador_do_contato, agora_unix);
// Some(pontos_de_introdução) se está online; None se não achou ou não validou.
```

A busca tenta a época atual, a anterior e a seguinte, para tolerar relógios dessincronizados.
Registros com assinatura inválida, chave errada ou formato errado são tratados como ausentes.

### 4. Escolher o modo de caminho

```rust
use clam::path::{PathMode, PathRequest};

let modo = PathRequest::private().resolve()?;  // TwoHop, o padrão
let direto = PathRequest {
    mode: PathMode::Direct,
    contact_trusted: true,              // contato marcado como de confiança
    ip_exposure_warning_acked: true,    // usuário confirmou o aviso de exposição de IP
}.resolve()?;                           // sem os dois campos: PathError::DirectNotAllowed
```

| Modo | Relays | Alvo de voz |
| --- | --- | --- |
| `TwoHop` (padrão) | 2 | 200 ms |
| `OneHop` | 1 | 200 ms |
| `Direct` | 0 | 150 ms |

### 5. Assinar os tópicos do círculo

```rust
use clam::circle::CircleSecret;

let circulo = CircleSecret::from_bytes(segredo_do_grupo); // 32 bytes, ex.: exportado do MLS
let topicos = service::circle_topics(&circulo, &cfg, agora_unix);
// 4 nomes opacos: presença e anúncio de call, época atual e seguinte. Assine todos no GossipSub.
```

Reassine a cada período. Ao remover um contato, gere um segredo novo para o círculo e refaça o pareamento das chaves de presença dele: o segredo antigo continua válido para quem o tem.

### 6. Ligar à DHT e ao GossipSub (feature `net`)

O serviço só fala com o trait `RecordStore`. O adaptador libp2p vem atrás da feature `net`:

```sh
cargo test --features net    # inclui os testes com dois nós reais em 127.0.0.1
```

```toml
clam = { path = "...", features = ["net"] }
```

```rust
use clam::net::{NetConfig, Node};

// Primeiro nó (semente): só escuta.
let semente = Node::start(NetConfig {
    listen: vec!["/ip4/0.0.0.0/tcp/4001".parse()?],
    ..Default::default()
}).await?;
println!("PeerId: {}", semente.peer_id());

// Outro nó: escuta e disca para a semente (o endereço precisa do sufixo /p2p/<PeerId>).
let mut no = Node::start(NetConfig {
    listen: vec!["/ip4/0.0.0.0/tcp/0".parse()?],
    bootstrap: vec!["/ip4/203.0.113.7/tcp/4001/p2p/12D3Koo...".parse()?],
    ..Default::default()
}).await?;

// Presença: o NetStore implementa RecordStore.
let store = no.store();
tokio::task::spawn_blocking(move || {
    service::publish(&store, &cfg, &chaves, &assinador, agora_unix, &intros)
}).await??;

// Círculo: assine os tópicos derivados e receba anúncios.
for t in service::circle_topics(&circulo, &cfg, agora_unix) { no.subscribe(&t)?; }
no.publish(&topico, anuncio_cifrado)?;
while let Some(msg) = no.next_message().await { /* msg.topic, msg.data */ }
```

Regras de uso:

- `NetStore::get` **bloqueia** até a DHT responder (padrão 10 s, ajustável em `NetConfig::get_timeout`). Chame `publish`/`lookup` sempre dentro de `tokio::task::spawn_blocking`.
- `put` só enfileira o comando. Sem nenhum par conhecido, o registro fica apenas no nó local até a rede se formar; republique a cada período.
- O GossipSub roda em modo **anônimo**: as mensagens não levam PeerId nem assinatura. Cifre e autentique o conteúdo com as chaves do círculo; o transporte repassa bytes opacos.
- A malha do GossipSub leva alguns segundos para se formar. Publique de novo se ainda não houver pares no tópico.
- Descartar o `Node` encerra a tarefa de rede.
- Para outro transporte (outra DHT, por exemplo), implemente você mesmo o trait:

```rust
pub trait RecordStore {
    fn put(&self, key: [u8; 32], value: Vec<u8>);
    fn get(&self, key: &[u8; 32]) -> Option<Vec<u8>>;
}
```

### 7. Publicação automática (feature `net`)

`PresenceRunner` faz sozinho o que os passos 2 e 5 pedem para repetir a cada período:

```rust
use std::sync::Arc;
use clam::runner::{PresenceRunner, RunnerConfig, system_clock};

let node = Node::start(cfg_rede).await?;
let store = Arc::new(node.store());
let runner = PresenceRunner::spawn(store, assinador, RunnerConfig::default(), system_clock());

runner.set_contacts(vec![chaves_do_bob, chaves_da_carol]); // um registro por contato
runner.set_circles(vec![circulo]);                         // assina e renova os tópicos
runner.set_intro_points(vec![b"relay-a.example:4001".to_vec()])?; // valida e republica na hora
```

O que ele garante:

- Publica ao iniciar e a cada `refresh_secs` (900 s).
- Nos últimos `lead_secs` (300 s) da época, publica também o registro da **próxima** época, para o endereço novo existir antes da virada.
- Mantém os tópicos do círculo da época atual e da seguinte, e cancela os vencidos.
- Mudar contatos, círculos ou pontos de introdução dispara uma passada imediata.
- Descartar o `PresenceRunner` (ou `runner.stop()`) encerra a tarefa. O `Node` precisa continuar vivo enquanto o runner roda.
- Precisa de um runtime tokio. Para testes, passe um `Clock` próprio no lugar de `system_clock()`.

### 8. Escolher os pontos de introdução

`IntroSelector` decide quais relays vão no registro. No MVP, a fonte é a lista de relays semente da equipe.

```rust
use clam::intro::{IntroConfig, IntroSelector};
use clam::rand_core::OsRng;

let mut intro = IntroSelector::new(IntroConfig::default()); // 3 pontos
intro.load_seeds(include_str!("seeds.txt"))?;
```

Formato do `seeds.txt`: uma semente por linha, `endereço [região]`; `#` inicia comentário.

```text
# relays da equipe
relay1.example.com:4001 sa-east
relay2.example.com:4001 us-east
relay3.example.com:4001 eu-west
```

Ciclo de uso, de preferência a cada poucos minutos:

```rust
// 1. Meça cada relay (um ping/handshake) e informe o resultado.
intro.report_success("relay1.example.com:4001", 42); // latência em ms
intro.report_failure("relay2.example.com:4001");

// 2. Reescolha; só republique se o conjunto mudou.
let sel = intro.select(&mut OsRng);
if sel.changed {
    runner.set_intro_points(sel.points)?;
}
```

Regras da escolha:

- Relay com 3 falhas seguidas (`max_failures`) sai; um `report_success` o recoloca.
- A escolha é **estável**: pontos saudáveis permanecem, e só o que falhou é substituído. Isso evita republicar à toa.
- Prefere menor latência, mas sorteia entre os `2 × count` melhores, para os usuários não se concentrarem nos mesmos relays. Relay ainda não medido conta 500 ms.
- Evita dois pontos na mesma região (ou no mesmo host, se não houver região).
- Havendo menos relays saudáveis que `count`, devolve os que existem; sem nenhum, o registro sai vazio.
- Endereços têm no máximo 64 bytes, e `count` vai de 1 a 8 (limite do registro).

A medição de latência e saúde é do app (ou da Fase 3, que terá o handshake com os relays): `IntroSelector` não abre conexões.
