# clam

Núcleo de privacidade da rede de chamadas P2P, em Rust: **modo privado por padrão** e **presença cifrada**.
Implementa a Fase 1 (pareamento e chaves de presença) e o Fase 2 (publicação, busca e renovação automática de presença) e o núcleo da Fase 3 (circuito de 2 saltos).
O transporte libp2p é opcional (feature `net`); não há relays; veja [Limitações](#limitações).

## Instalação e testes

```sh
cargo test                       # testes unitários e doctest
cargo run --example quickstart   # fluxo completo, ponta a ponta
cargo doc --open                 # documentação da API
```

Requer Rust com edition 2024 (1.85+).
O crate reexporta `ed25519_dalek`, `x25519_dalek` e `rand_core` (`clam::ed25519_dalek` etc.), para você usar as mesmas versões das chaves que ele espera.

## Conceitos

- **Pareamento:** os dois dispositivos trocam chaves públicas X25519 uma única vez. Do segredo compartilhado, o HKDF deriva uma chave de cifra e uma semente de busca. Só os dois lados as têm.
- **Endereço que gira:** o identificador de busca na DHT é derivado da semente e do número do período (época, 1 hora por padrão). Quem observa a DHT não liga o endereço de hoje ao de ontem.
- **Registro de presença:** contém só pontos de introdução (endereços de relays), nunca o IP. É cifrado (ChaCha20-Poly1305), assinado (ed25519) e tem sempre 668 bytes, qualquer que seja o conteúdo. O endereço entra como dado autenticado, então o registro não vale em outro endereço.
- **Consultas falsas:** cada busca real vem misturada e embaralhada com consultas a endereços aleatórios.
- **Círculos:** os tópicos de presença e de anúncio de call vêm do segredo do grupo. Quem não é membro não os deriva.
- **Modos de caminho:** 2 saltos (padrão), 1 salto e direto.

## Passo a passo

Todos os exemplos abaixo estão reunidos em [`examples/quickstart.rs`](examples/quickstart.rs).

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

## Fase 3: circuito de 2 saltos (`clam::circuit`)

Núcleo do modo privado: você → relay A → relay B → destino. É *sans-IO*: as máquinas de estado recebem células e devolvem ações, e o transporte (libp2p, UDP) fica por sua conta. Ainda **não** há transporte real nem failover automático (veja o fim desta seção).

Garantias, cobertas por `tests/circuit.rs`:

- A só conhece o seu IP e o de B; B só conhece o de A e o destino. O destino só aparece dentro da camada de B.
- Handshake X25519 por salto, autenticado pela chave estática do relay (estilo ntor), com sigilo adiante. Um relay sem a chave certa é detectado pelo cliente.
- Células de tamanho fixo (`CELL_LEN = 256`), com preenchimento aleatório a cada salto: o tamanho no fio não revela a posição do salto.
- Contador explícito por camada e janela anti-replay de 64 células: repetição e adulteração são descartadas.
- Limites do relay: circuitos simultâneos, taxa de criação de circuitos, banda por circuito e total, expiração por ociosidade e derrubada após falhas de autenticação.
- Custo: cada célula carrega até **203 bytes** de dados (cabe um quadro Opus de 20 ms); voz bidirecional gasta cerca de 100 kbps por sentido, mais que os 40 a 60 kbps do Opus puro, por causa do tamanho fixo. Ajuste `CELL_LEN` em `src/circuit/mod.rs` para trocar banda por tamanho máximo de pacote.

### Cliente

```rust
use clam::circuit::client::{ClientCircuit, Event};

// Cada método devolve a célula a enviar ao PRIMEIRO relay (A).
let (mut c, create) = ClientCircuit::create(1, &chave_publica_de_a);
enviar_a_a(create);

// Para cada célula recebida de A:
match c.handle_cell(celula_recebida)? {
    Event::HopReady(0) => enviar_a_a(c.extend("relay-b.example:4001", &chave_publica_de_b)?),
    Event::HopReady(1) => enviar_a_a(c.begin("contato.example:9")?),
    Event::Data(bytes) => { /* áudio vindo do contato */ }
    Event::Pong { hop, nonce } => { /* latência até o salto: use um relógio seu */ }
    Event::Destroyed => { /* um relay caiu: monte outro circuito */ }
    _ => {}
}

enviar_a_a(c.send_data(&quadro_opus)?);   // até 203 bytes
enviar_a_a(c.ping(1, nonce)?);            // sinal de vida e medição de RTT
```

### Relay

```rust
use clam::circuit::relay::{Action, RelayConfig, RelayNode};

let mut relay = RelayNode::new(segredo_x25519, RelayConfig::default(), agora_ms);
// publique relay.public_key() no descritor do relay

let acoes = relay.handle_cell(link, celula, agora_ms);
for a in acoes {
    match a {
        Action::Send { link, cell } => { /* escreva a célula no link */ }
        Action::Connect { link, addr } => {
            /* conecte a addr; depois: relay.link_established(link), ou link_failed(link) */
        }
        Action::OpenStream { circ, dest } => { /* só no último salto: abra o fluxo até dest */ }
        Action::Deliver { circ, payload } => { /* entregue ao destino */ }
        Action::CloseStream { circ } => {}
    }
}
// Dados que voltam do destino: relay.stream_data(circ, &bytes)?
// A cada poucos segundos: relay.tick(agora_ms) derruba circuitos ociosos.
```

Contrato com o transporte: ids de link de entrada devem ser menores que `FIRST_OUTBOUND_LINK` (2^63); cada `Action::Connect` pede uma conexão **nova**. O `tests/circuit.rs` traz um simulador completo que você pode copiar.

### Gerente de circuitos (`clam::circuit::manager`)

O `CircuitManager` mantém um circuito **ativo** e outro **reserva**, já montado e verificado, por relays diferentes. Se o ativo falha, o reserva assume na mesma chamada de `tick`, sem novo handshake. Depois o gerente monta um novo reserva. É *sans-IO*: você alimenta com tempo (ms) e células, e ele devolve `Vec<Out>`.

```rust
use clam::circuit::manager::{Candidate, CircuitManager, ManagerConfig, Out, Event};

// `relays`: descritores dos relays (endereço, chave pública, pontuação).
let mut mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", relays);
let mut outs = mgr.start(agora_ms);

// Laço do app:
//   a cada ~10-20 ms:        outs = mgr.tick(agora_ms);
//   célula vinda de relay:   outs = mgr.handle_cell(cell, agora_ms);
//   quadro de áudio:         outs = mgr.send(&quadro)?;
for out in outs {
    match out {
        Out::Send { to, cell } => { /* envie `cell` à conexão com o relay `to` */ }
        Out::Event(Event::Data(bytes)) => { /* mídia do contato */ }
        Out::Event(Event::FailedOver { silence_ms }) => { /* só informativo */ }
        Out::Event(Event::CircuitFailed { first, second, reason }) => { /* alimente a reputação */ }
        Out::Event(_) => {}
    }
}
```

Como funciona:

- **Detecção**: o ativo recebe ping fim a fim a cada `ping_interval_ms` (100). Sem nenhum sinal por `dead_after_ms` (350), é dado como morto. Com o ping de 100 ms e o prazo de 350 ms, o silêncio do failover fica perto de 350 ms, abaixo do alvo de 500 ms.
- **Reserva**: pinga a cada 1 s para continuar vivo nos relays e abre o fluxo até o destino. Por isso o contato pode ver duas sessões; o app deve tolerar duplicatas (número de sequência no pacote de mídia).
- **Escolha de relays**: sorteia entre os 3 de maior pontuação. O reserva evita os relays do ativo; se não houver relays suficientes, cai para evitar só um deles.
- **Punição**: relay de um circuito que falhou fica `cooldown_ms` (30 s) fora das escolhas. Falha na montagem pune só o salto travado; falha em circuito pronto pune os dois, já que não dá para saber qual caiu.
- **Sem circuito**: se não há reserva, emite `Event::NoCircuit`, `send` devolve `SendError::Unavailable` e a montagem é refeita a cada `retry_ms`.
- **Ajuste de latência**: `dead_after_ms` precisa superar a ida e volta do circuito mais o intervalo de ping. Se a latência real passar de ~100 ms por travessia, aumente-o; o silêncio no failover cresce junto.

Os testes em `tests/manager.rs` derrubam o primeiro ou o segundo relay do ativo e verificam silêncio < 500 ms, reserva disjunto, remontagem sem os relays punidos e timeout de montagem. A rede simulada entrega sem atraso; a medição com latência real continua pendente.

### Transporte TCP (`clam::transport`, feature `net`)

Liga os motores *sans-IO* a sockets TCP reais.

**Rodar um relay:**

```rust
use clam::circuit::relay::{RelayConfig, RelayNode};
use clam::transport::{RelayServer, ServerConfig, allow_list};
use clam::{rand_core::OsRng, x25519_dalek::StaticSecret};

let node = RelayNode::new(StaticSecret::random_from_rng(OsRng), RelayConfig::default(), 0);
println!("chave do circuito: {:?}", node.public_key()); // vai no descritor (onion_key)

let mut cfg = ServerConfig::new("0.0.0.0:4001".parse()?);
// Só relays de saída precisam disto: quais destinos o segundo salto pode abrir.
cfg.exit = allow_list(vec![("contato:9".into(), "127.0.0.1:9000".into())]);
// Opcional: cfg.link_secret = Some(segredo_guardado) mantém a chave de link entre reinícios.
let relay = RelayServer::spawn(node, cfg).await?; // relay.stop() para encerrar
println!("chave do link: {:?}", relay.link_public_key()); // vai no descritor (link_key)
```

**Usar como cliente:**

```rust
use clam::circuit::manager::{Candidate, CircuitManager, Event, ManagerConfig};
use clam::transport::CircuitClient;

let mgr = CircuitManager::new(ManagerConfig::default(), "contato:9", candidatos);
let mut c = CircuitClient::spawn(mgr, std::time::Duration::from_secs(2));

while let Some(ev) = c.next_event().await {
    match ev {
        Event::Ready { .. } => c.send(b"quadro de audio").await?,
        Event::Data(bytes) => { /* mídia do contato */ }
        Event::CircuitFailed { first, second, reason } => { /* reputação */ }
        _ => {}
    }
}
```

Formato no fio: os links entre nós usam **Noise `NK`** (`clam::link`, crate `snow`) e carregam uma célula por mensagem cifrada; todo quadro no fio tem 279 bytes. O fluxo do relay de saída ao destino usa quadros `[u16 BE tamanho][dados]`, para preservar as fronteiras dos pacotes de mídia.

Cifra de link:

- **Autentica o relay**: quem disca conhece a **chave de link** do relay (`link_key` do descritor, `Candidate::link_public`). Com a chave errada o handshake falha e o link não abre. Relay para relay funciona igual: o cliente inclui a chave de link do próximo relay no pedido de extensão.
- **Oculta** ids de circuito, comandos e conteúdo de quem observa a rede, e impede injetar ou alterar células. O tamanho já é fixo; a temporização é coberta pelo cover traffic (seção abaixo).
- Nonces em ordem estrita por direção, handshake com prazo de 3 s, e `max_links` (512) conta também conexões ainda em handshake. Quem manda célula em claro, sem handshake, é desconectado.
- **Duas chaves distintas**: a do circuito (`onion_key`, handshake estilo ntor) e a do link (`link_key`, Noise NK), ambas no descritor assinado. Vazar ou girar uma não afeta a outra. Por padrão `RelayServer` gera a chave de link a cada início; para mantê-la, passe `ServerConfig::link_secret` e publique um descritor novo se ela mudar. Usar a chave do circuito como chave de link não abre o link (há teste).

Outros limites:

- **`ExitPolicy`**: o padrão recusa tudo. Sem lista de destinos, um relay de saída seria um proxy aberto. Relays que só fazem o primeiro salto não precisam configurar nada.
- O trecho do relay de saída ao destino é TCP puro. A mídia já vai cifrada ponta a ponta pela camada acima.
- Fechar um link não derruba na hora os circuitos que passavam por ele: o relay os expira por ociosidade e o cliente os detecta por silêncio.

`tests/transport.rs` roda tudo em 127.0.0.1: eco de voz por 2 saltos, política de saída recusando destino fora da lista, recusa de relay com chave errada, relay que descarta célula em claro, e failover derrubando o primeiro relay do circuito ativo com mídia a cada 20 ms (silêncio < 500 ms, repetido várias vezes sem falhar). Em loopback não há latência de rede; a medição entre máquinas distintas tem roteiro em "Medir entre máquinas reais" (resultados ainda não coletados).

### Descritores assinados (`clam::descriptor`)

Cada relay publica um descritor assinado pela sua **identidade** (ed25519): endereço, chave de circuito e chave de link (X25519), banda declarada e validade. O `RelayDirectory` valida os que chegam e entrega ao gerente a lista de `Candidate`.

```rust
use clam::descriptor::{DEFAULT_TTL_SECS, DirectoryConfig, RelayDescriptor, RelayDirectory};
use clam::{ed25519_dalek::SigningKey, rand_core::OsRng};

// No relay (guarde `identity` em disco; ela é a identidade do relay):
let identity = SigningKey::generate(&mut OsRng);
let d = RelayDescriptor::sign(&identity, node.public_key(), relay.link_public_key(), "203.0.113.5:4001", 2_000, agora_s, DEFAULT_TTL_SECS)?;
let bytes = d.to_bytes(); // publique/repasse estes bytes; reassine antes de vencer

// No cliente, para cada descritor recebido (GossipSub, arquivo de sementes...):
let mut dir = RelayDirectory::new(DirectoryConfig::default());
dir.insert_bytes(&bytes, agora_s)?;          // Err se inválido, vencido, endereço proibido...
let candidatos = dir.candidates(agora_s);     // alimenta CircuitManager::new / set_relays
```

Formato v2: `[versão][identidade 32][chave de circuito 32][chave de link 32][banda kbps u32][emitido u64][ttl u32][tam. addr u8][addr][assinatura 64]`, assinatura sobre `"clam-relay-desc-v2" || corpo`.

O diretório aplica:

- **Assinatura e validade**: qualquer byte alterado é recusado; vencido ou emitido no futuro (tolerância de 300 s) também. `ttl` máximo aceito: 24 h.
- **Versão mais nova vence**: um descritor antigo reenviado volta como `Stale` e não ressuscita nada.
- **Um endereço, uma identidade**: quem chegou primeiro (enquanto válido) o mantém; outra identidade com o mesmo endereço recebe `AddressTaken`.
- **Endereços seguros**: `host:porta` com porta ≠ 0; literais de IP loopback, privados, de link local ou multicast, e `localhost`, são recusados, para o descritor não apontar relays a redes internas. Em redes de teste use `allow_private_addrs: true`.
- **Teto de tamanho**: `max_entries` (4096).
- **Pontuação**: cresce com o log da banda declarada, limitada a 10 000 kbps, porque a banda é autodeclarada.

Limites: a identidade é barata de gerar (ataque Sybil), então a assinatura prova *quem assinou*, não que o relay é bom. Reputação medida e relays semeados pela equipe continuam necessários. O descritor ainda carrega uma chave só (link e circuito), e o diretório ainda não recebe descritores sozinho: falta o canal de descoberta (GossipSub).

### Descoberta de relays por GossipSub (`clam::discovery`, feature `net`)

Os relays anunciam o descritor no tópico público `clam/relays/v1`; todo nó que assina o tópico valida, guarda no `RelayDirectory` e alimenta o gerente de circuitos.

**Relay** (anuncia e ouve):

```rust
use std::sync::Arc;
use clam::discovery::{Announcer, Discovery, DiscoveryConfig, DiscoveryRunner};
use clam::net::{NetConfig, Node};
use clam::runner::system_clock;
use clam::descriptor::{DEFAULT_TTL_SECS, DirectoryConfig, RelayDescriptor};

let mut node = Node::start(NetConfig { /* listen, bootstrap */ ..Default::default() }).await?;
let discovery = Arc::new(Discovery::new(DirectoryConfig::default(), system_clock()));

// Reassina a cada chamada (issued_at = agora), para a versão nova substituir a antiga.
let announcer: Announcer = Arc::new(move |agora| {
    RelayDescriptor::sign(&identidade, chave_de_circuito, "203.0.113.5:4001", 2_000, agora, DEFAULT_TTL_SECS)
        .ok()
        .map(|d| d.to_bytes())
});
let _runner = DiscoveryRunner::spawn(
    discovery.clone(),
    node.gossip(),
    node.take_descriptors().unwrap(),
    Some(announcer),
    DiscoveryConfig::default(),
);
```

**Cliente** (só ouve): igual, com `None` no lugar do anunciante. Para começar sem esperar a rede, carregue sementes (um descritor em hexadecimal por linha, `#` para comentário; `to_seed_line` gera a linha):

```rust
discovery.load_seeds(&std::fs::read_to_string("relays.seeds")?);

// De tempos em tempos (ex.: a cada minuto) renove a lista do gerente:
mgr.set_relays(discovery.candidates());
```

Como funciona:

- **Validação antes de repassar.** O `Node` usa validação manual do GossipSub. Um descritor só é repassado, e só chega ao `Discovery`, se tiver assinatura válida, validade em dia e endereço permitido, conforme `NetConfig::relay_policy` (em redes de teste, `DirectoryConfig { allow_private_addrs: true, .. }`). Lixo, assinatura quebrada e descritor vencido morrem no primeiro nó que os recebe.
- **Anúncios.** O relay publica logo ao subir, de novo após 2 s, 10 s e 30 s (a malha do GossipSub leva um tempo para se formar) e depois a cada 10 min. A cada 5 min reenvia até 8 descritores que conhece, para quem entrou depois ficar sabendo.
- **Estado.** Fica no `RelayDirectory`: versão mais nova vence, um endereço por identidade, teto de 4096 entradas.
- As mensagens do tópico de relays vêm por `Node::take_descriptors()` e não passam por `next_message()`.

Limites: o tópico é público, então quem o assina vê quais relays existem (intencional; o que se protege é quem usa quais). Um cliente sem nenhum vizinho no GossipSub não descobre nada: mantenha `bootstrap` ou sementes. Sybil continua em aberto (veja "Descritores assinados").

### Reputação medida (`clam::reputation`)

A banda do descritor é declarada pelo próprio relay, então não prova nada. `Reputation` ajusta a pontuação com o que o seu cliente observa nos eventos do gerente:

```rust
let rep = Reputation::new(ReputationConfig::default());
let mut client = CircuitClient::spawn_with_reputation(manager, Duration::from_secs(3), rep);
// ao renovar a lista (descoberta): a reputação é aplicada pelo próprio cliente
client.set_relays(discovery.candidates()).await;
// diagnóstico
let stats = client.relay_stats("203.0.113.5:9000").await;
```

`spawn_with_reputation` liga tudo: o cliente alimenta a `Reputation` com cada `Rtt` e `CircuitFailed`, guarda a lista bruta de `set_relays` e reaplica as pontuações a cada falha e a cada 30 s (`REPUTATION_REFRESH`). Sem reputação (`spawn`), `set_relays` repassa a lista como veio. Quem preferir controle manual usa `Reputation::record` e `adjust` direto.

- **Acertos e erros** com decaimento exponencial (meia-vida de 1 h). Cada `Rtt` vale 0,02 acerto; cada falha, 1 erro. Falha de montagem em `create` culpa só o primeiro salto, em `extend` só o segundo; silêncio ou `Destroy` dividem 0,5 para cada um. Um relay ruim aparece em muitas falhas com parceiros diferentes, e a diferença se acumula.
- **Latência:** média móvel do RTT dos circuitos de que o relay participou. Até 100 ms não pesa; a cada 200 ms a mais, o fator cai pela metade.
- **Pontuação** = base × confiabilidade² × fator de latência. Relay sem histórico começa em 2/3 de confiabilidade: pode ser escolhido, mas perde para um relay que já provou ser estável.
- **Banimento local:** com pelo menos 4 de evidência e taxa de acerto abaixo de 20%, o relay sai da lista até as falhas decaírem.
- `CircuitClient::set_relays` troca a lista sem derrubar circuitos em uso. A lista só passa pela reputação depois do primeiro `set_relays`; antes disso vale a lista com que o gerente foi criado.

**Persistência.** A reputação sobrevive a reinícios por instantâneo em arquivo:

```rust
use clam::reputation::{load_file, save_file};

let path = Path::new("reputacao.bin");
let rep = load_file(ReputationConfig::default(), path, 0)?; // arquivo ausente = vazia
let client = CircuitClient::spawn_with_reputation(manager, timeout, rep);
// ... periodicamente (ex.: a cada minuto) e ao encerrar:
if let Some(bytes) = client.reputation_snapshot().await {
    save_file(path, &bytes)?;
}
```

- Formato `clamrep1`: endereço, acertos, erros e RTT por relay, mais a hora da gravação. Ao carregar, o decaimento cobre o tempo parado (desligado por uma meia-vida, as contagens caem pela metade).
- `save_file` é atômico (temporário + `rename`) e usa permissão `0600`: o arquivo revela quais relays você usou.
- O arquivo é tratado como não confiável: magia, tamanho, número de entradas (`max_entries`), UTF-8 do endereço e valores (finitos, entre 0 e 10 000) são validados; qualquer defeito vira `InvalidData`, nunca pânico. Quem tem acesso de escrita ao arquivo ainda pode forjar boa reputação para relays escolhidos; proteja o diretório como protegeria uma chave.
- Sem gravação automática: o chamador decide quando salvar.

Limites: só vale a experiência própria (nada de reputação trocada entre nós, que seria fácil de manipular), então a partida é fria. Um relay pode se comportar bem sob ping e mal sob carga. Sybil continua em aberto: quem cria muitas identidades começa todas com a prioridade de relay novo, o que só atrasa o ataque.

### Testes de abuso (`tests/fuzz.rs`)

Fuzzing determinístico, sem dependências: um PRNG fixo gera bytes aleatórios e mutações (bit trocado, byte zerado ou saturado, corte, extensão) de entradas válidas. Cobre o parser de células e de células de relay, o descritor, a importação de reputação, o relay, o cliente do circuito e o gerente. Os testes afirmam que nada entra em pânico, que as tabelas ficam limitadas e que descritor adulterado nunca passa na verificação.

```bash
cargo test --test fuzz                                    # rápido (padrão)
CLAM_FUZZ_SEED=7 CLAM_FUZZ_ITERS=400000 cargo test --release --test fuzz   # campanha longa
```

O que o fuzzing encontrou: **inundação de `Create`**. O limite de circuitos protegia a memória, mas quem abre e derruba circuitos em loop nunca enchia a tabela e fazia o relay gastar um Diffie-Hellman por célula. Agora o relay aceita no máximo `RelayConfig::creates_per_sec` criações por segundo (padrão 20, rajada `create_burst` = 40); o excesso recebe `Destroy` sem custo de criptografia e conta em `stats.refused_create_rate`. Também ficou provado que células adulteradas ou lixo no mesmo circuito não dessincronizam as camadas, e que 16 falhas de autenticação derrubam o circuito.

Limites: é fuzzing por amostragem, não guiado por cobertura; não substitui uma campanha longa com `cargo-fuzz` nem revisão por terceiros. O transporte (Noise e TCP) não está nestes testes: o `snow` cuida do parsing do link.

### Cover traffic (padding de link)

Sem cobertura, quem observa um link vê quando você fala e quando fica em silêncio (rajadas de 50 quadros/s contra quase nada). Com cover traffic ligado, cada link cifrado emite **exatamente um quadro por intervalo** (padrão 10 ms): a próxima célula da fila ou, se não houver, uma célula `Padding`. Depois do Noise, mídia, controle e preenchimento têm o mesmo tamanho (279 bytes) e a mesma cadência.

- **Cliente:** `CircuitClient::spawn` liga por padrão (`ClientOptions::cover = Some(DEFAULT_COVER)`). Para desligar ou mudar: `CircuitClient::spawn_with(mgr, timeout, ClientOptions { cover: None, ..Default::default() })`.
- **Relay:** abre padding nos links que ele mesmo disca (relay a relay) e **responde** com padding nos links de entrada cujo par mandou padding. Nunca inicia padding em link de entrada. `ServerConfig::cover` (`None` desliga) e `ServerConfig::max_cover_links` (padrão 64).
- **Descarte:** `Padding` é consumido na camada de link. Não chega ao relay lógico nem a circuitos, não gasta token bucket de circuito e não aparece como evento.
- **Custo:** 100 quadros/s × 279 bytes ≈ **223 kbps por sentido, por link, constantes** enquanto o link existe. Numa chamada de 2 saltos isso é bem mais que os ~100 kbps da voz sem cobertura. Aumente o intervalo (ex.: 20 ms) para gastar menos; abaixo da taxa de células reais (voz + ping ≈ 60/s) a fila de saída cresce e o atraso sobe, então não passe de ~15 ms com voz.
- **Latência:** o quadro real espera, em média, meio intervalo (5 ms no padrão). O teste de failover continua abaixo de 500 ms com cobertura ligada.

Limites, ditos com franqueza:

- É padding **por link**: protege contra quem observa um link (provedor do cliente, rede de um relay). O cover de ponta a ponta (seção seguinte) esconde o ritmo dos relays do circuito. Um adversário global que correlaciona entrada e saída continua fora do que o desenho promete.
- O padding acompanha o link, não a chamada: o início e o fim do link ainda revelam quando uma chamada começou e terminou.
- Um par pode forçar um relay a gastar banda mandando um padding. O teto `max_cover_links` limita o dano (64 × 223 kbps ≈ 14 Mbps por sentido no pior caso); relays com banda pequena devem baixar o teto ou desligar.
- Não há jitter aleatório: a cadência é fixa, o que é bom contra análise de temporização mas é uma assinatura reconhecível de "este é um link do clam".

### Medir entre máquinas reais (`examples/measure.rs`)

Em loopback não há latência de rede. O exemplo `measure` mede o circuito de 2 saltos em máquinas distintas e compara com TCP direto. Precisa de 3 máquinas (ou 3 processos em hosts diferentes) com portas TCP abertas: **C** (destino), **A** e **B** (relays). O cliente roda em qualquer outra, ou em C.

```sh
cargo build --release --features net --example measure   # em cada máquina; sempre release
M=target/release/examples/measure

# C: destino que ecoa os quadros
$M contact 0.0.0.0:9000

# A e B: relays. Argumentos: <escuta> <endereço público> contato:9=<IP-de-C>:9000
$M relay 0.0.0.0:4001 IP-DE-A:4001 contato:9=IP-DE-C:9000    # imprime: addr,onion,link
$M relay 0.0.0.0:4001 IP-DE-B:4001 contato:9=IP-DE-C:9000

# cliente: cole as duas linhas impressas por A e B
$M probe "LINHA-A" "LINHA-B" --seconds 120               # com cover (padrão)
$M probe "LINHA-A" "LINHA-B" --seconds 120 --no-cover    # sem cover
$M direct IP-DE-C:9000 --seconds 120                     # linha de base, sem relays
```

Cada relay imprime uma linha `addr,onion,link` (endereço e as duas chaves públicas em hex) na saída padrão. As chaves mudam a cada início do relay, então rode o `probe` com as linhas atuais. O `probe` manda um quadro de 128 B a cada 20 ms (`--interval-ms`) e relata perda e RTT (min, p50, p95, p99, máx, jitter), o tempo de montagem do circuito e failovers. Um quadro ainda em trânsito no fim conta como perda (≈ 1 em 250).

#### Medir entre duas máquinas (Linux e Windows) com os scripts

Se você só tem duas máquinas, os scripts de `scripts/` automatizam o roteiro acima. A divisão é: o **Windows** (ou qualquer outra) roda o destino e o relay B; esta máquina (**Linux**) roda o relay A e o cliente. Assim o `direct` mede a rede pura entre as duas.

1. Nas duas, instale o Rust e clone o projeto. Libere as portas TCP 4001 e 9000 no firewall da máquina remota.
2. No Windows, em um PowerShell (como administrador, se usar `-OpenFirewall`):

   ```powershell
   .\scripts\measure-windows.ps1 -PublicIp 192.168.0.20 -OpenFirewall
   ```

   `-PublicIp` é o IP do Windows alcançável pelo Linux. O script compila, sobe o destino e o relay B, e imprime o comando exato para rodar no Linux (com a linha `addr,onion,link` já preenchida). Ctrl+C encerra tudo e remove as regras de firewall.
3. No Linux, cole o comando impresso:

   ```sh
   scripts/measure.sh 192.168.0.20 '192.168.0.20:4001,<onion>,<link>' 60
   ```

   O script sobe o relay A, mede o `direct` e o circuito sem cover e com os perfis `economy`, `standard` e `strong` (60 s cada, mude o último argumento), e grava tudo em `measure-results-<data>.txt`.

Como ler: o `direct` é a rede pura Linux–Windows. Cada medição de circuito imprime `caminho: cliente -> X -> Y -> destino`. O gerente **sorteia a ordem dos relays** a cada execução, então o relay local pode ser o primeiro ou o de saída, e os dois caminhos não têm a mesma latência. Compare só medições com o mesmo caminho, ou repita o script até ter várias de cada. Os scripts foram testados só com os dois lados em `127.0.0.1`; o `measure-windows.ps1` não foi executado em Windows.

Como ler:

- **Referência**: o RTT do `direct` é a rede pura entre o cliente e C. O custo do circuito é `probe − direct`.
- **Orçamento de voz**: o gargalo é o trajeto de ida e volta. Para conversa fluida mire em p95 de ida (RTT/2) abaixo de ~150 ms; p99 do RTT muito acima do p50 indica fila ou perda.
- **Cover**: compare `--no-cover` com o padrão. O padding de 10 ms por link soma atraso (cada quadro espera o próximo instante de envio).
- **Failover**: derrube o relay A (Ctrl-C) durante um `probe` longo e veja a linha `failover (silêncio N ms)`. Meta: < 500 ms.
- Para um perfil de rede ruim, rode com `tc qdisc add dev eth0 root netem delay 40ms 10ms loss 1%` na máquina de um relay.

Referência em loopback (release, 1 máquina, 5 s), RTT p50 da voz:

| Configuração | p50 |
|---|---|
| TCP direto | 0,5 ms |
| Circuito sem nenhum cover (`relay ... --no-cover` e `probe --no-cover`) | 1,3 ms |
| Cover só entre os relays (padrão dos relays, `probe --no-cover`) | 18 ms |
| Cover em todos os links (padrão) | 25 ms |

O atraso de ~18 ms **não é bug**: é a quantização do cover. Com cover, cada link só envia no próximo instante do relógio de 10 ms, então um quadro espera em média 5 ms por link e por sentido (até 10). Com 3 links e 2 sentidos isso dá ~25 ms, e o relay liga cover por padrão nos links que ele abre, por isso `probe --no-cover` sozinho ainda mede 18 ms. Para medir só a rede, ligue `--no-cover` também nos relays. Reduzir o atraso custa banda: o intervalo de 5 ms corta a espera pela metade e dobra os ~223 kbps por sentido e por link.

### Cover traffic de ponta a ponta (circuito)

O padding de link esconde a temporização de quem observa um link, mas **os relays do circuito continuam vendo** quando há voz: o relay A enxerga as células do seu circuito e sabe se estão fluindo. O cover de circuito esconde isso: o cliente manda **exatamente uma célula de circuito a cada 20 ms** (padrão), seja mídia, ping ou um `RelayCmd::Drop`, e o relay de saída **responde uma por uma**, com o próximo quadro vindo do contato ou com outro `Drop`. Silêncio e conversa têm a mesma taxa nos dois sentidos, em todos os saltos (há testes que contam 50 células/s nos dois casos).

- **Ligar/desligar:** `ClientOptions::circuit_cover` (padrão `Some(DEFAULT_CIRCUIT_COVER)` = 20 ms; `None` desliga). No gerente sans-IO: `ManagerConfig::cover_ms` ou `CircuitManager::set_cover`. Os relays não precisam de configuração: o cover liga no relay de saída ao receber o primeiro `Drop`.
- **Relógio do cliente:** o cliente dita a cadência dos dois sentidos, então o relay de saída não precisa de timer. Mídia que chega do contato espera no relay (fila de 64, a mais antiga cai) até a próxima célula do cliente.
- **Ping:** só ocupa o lugar de um `Drop`. Falando sem parar, o RTT medido pode ficar desatualizado, mas o circuito segue vivo, porque cada `Drop` ou `Data` recebido conta como sinal de vida e o failover continua abaixo de 500 ms (há teste).
- **Custo:** 50 células de 256 B por sentido, por circuito ativo: ≈ 100 kbps por sentido. O reserva também mantém cover (`ManagerConfig::standby_cover`, padrão `true`): outras 50 células por sentido, então o gasto total do cliente é o dobro. Com `standby_cover: false` o reserva volta a mandar só um ping por segundo. Esse tráfego já está dentro dos 64 kB/s por circuito do relay.
- **Latência:** o quadro de mídia espera até 20 ms pela próxima célula do cliente, mais até 20 ms no sentido de volta pela mesma razão (o exit só responde quando o cliente manda).
- **Limites:** com `standby_cover` ligado o reserva tem o mesmo ritmo do ativo, então o failover não muda o padrão; com ele desligado, o reserva se distingue (um ping por segundo). Quem vê o primeiro relay ainda nota o tráfego de mídia passando de um circuito ao outro. O cover não protege contra um relay de saída malicioso que mede a fila ou contra adversário que vê o contato.

### Perfis de custo do cover (`clam::cover`)

Os dois covers custam banda constante. Um **perfil** agrupa os intervalos em quatro níveis:

| Perfil | Link | Circuito | Banda do cliente (por sentido) | Atraso aproximado por link |
|---|---|---|---|---|
| `Off` | desligado | desligado | 0 | 0 |
| `Economy` | 20 ms | 20 ms | ≈ 112 kbps | até 20 ms (média 10) |
| `Standard` (padrão) | 10 ms | 20 ms | ≈ 223 kbps | média 5 ms |
| `Strong` | 5 ms | 20 ms | ≈ 446 kbps | média 2,5 ms |

O cover de circuito vai dentro dos quadros do link e não soma; o circuito nunca passa de 20 ms (cadência da voz) e o link nunca fica mais lento que o circuito (há teste).

```rust
use clam::cover::CoverProfile;
use clam::transport::{CircuitClient, ClientOptions};

// Escolher na partida (substitui `cover` e `circuit_cover`):
let c = CircuitClient::spawn_with(mgr, timeout, ClientOptions { profile: Some(CoverProfile::Economy), ..Default::default() });

// Adaptar à rede em uso, mesmo com a chamada em curso:
c.set_profile(CoverProfile::for_budget(150.0)).await;   // dados móveis: cabe só o Economy
c.set_profile(CoverProfile::Strong).await;              // Wi-Fi
```

- `CoverProfile::for_budget(kbps)` devolve o perfil mais forte que cabe no orçamento por sentido. Abaixo de 112 kbps devolve `Off`, porque um cover pela metade não esconde nada. A banda da voz (≈ 100 kbps) fica fora desse orçamento.
- `set_profile` vale também nos links já abertos (o escritor de cada link observa o intervalo), sem reconectar nem refazer circuito.
- **Quem decide é o aplicativo.** O clam não adivinha se a rede é paga ou se a bateria está baixa: ele só aplica o perfil. Também não há troca automática por congestionamento, porque cover mais lento não alivia fila nenhuma.
- A troca é visível: quem observa o link vê a cadência mudar. Trocar de perfil no meio da chamada revela a troca, não o conteúdo; prefira trocar na mudança de rede, quando o link já muda.
- **Perfil por relay.** O relay tem o seu próprio perfil, independente do cliente: `ServerConfig { profile: Some(CoverProfile::Economy), .. }` na partida (substitui `ServerConfig::cover`, padrão 10 ms) e `RelayServer::set_profile(..)` para trocar com o relay no ar, inclusive nos links abertos. Vale para o que o relay escreve, nos links que ele abre e nos de entrada cujo par usa padding. Um relay em banda cara usa `Economy`; os outros ficam em `Standard`. O cliente continua ditando o ritmo do circuito (1 célula a cada 20 ms), então o perfil do relay só muda o padding de link e o atraso por salto. O teto `max_cover_links` segue valendo.
- No `measure`: `probe ... --profile economy`.

### Fuzzing guiado por cobertura (`fuzz/`, `cargo-fuzz`)

Além dos testes determinísticos de `tests/fuzz.rs`, o diretório `fuzz/` tem alvos libFuzzer que usam cobertura de código para chegar a caminhos que o gerador aleatório não alcança. É um crate à parte (`[workspace]` próprio), então não afeta `cargo build` nem `cargo test`.

| Alvo | O que exercita | Propriedade |
|---|---|---|
| `cell` | `Cell::from_bytes`, `RelayCell::decode` | sem pânico; célula aceita reproduz a entrada |
| `descriptor` | `RelayDescriptor::from_bytes`, `verify` | sem pânico; descritor aceito reproduz a entrada |
| `reputation` | `Reputation::import` | sem pânico; o que entra sai importável de novo |
| `presence` | `presence::open` | sem pânico com registro arbitrário |
| `relay` | `RelayNode` com um circuito real aberto e células arbitrárias em 4 links | sem pânico; circuitos ≤ `max_circuits` |
| `manager` | `CircuitManager` com cover ligado, células arbitrárias e relógio irregular | sem pânico |

```sh
cargo install cargo-fuzz
cargo fuzz run relay                       # nightly (rustup default nightly), com AddressSanitizer
cargo fuzz run relay -- -max_total_time=300

# Sem nightly, só com o Rust estável (sem sanitizer):
RUSTC_BOOTSTRAP=1 cargo fuzz run -s none relay -- -max_total_time=300
```

Um crash fica em `fuzz/artifacts/<alvo>/`; reproduza com `cargo fuzz run <alvo> fuzz/artifacts/<alvo>/crash-...`. O corpus (`fuzz/corpus`) e os artefatos não vão para o git.

Resultado (Rust 1.93 estável, `-s none`, sem AddressSanitizer): nenhum crash nem violação de propriedade em nenhuma rodada.

| Alvo | Rodada curta (40 s) | Campanha (20 min, 6 alvos em paralelo) | Cobertura (arestas / corpus) |
|---|---|---|---|
| `cell` | sem crash | 899 M execuções | 37 / 8 |
| `descriptor` | sem crash | 63 M | 163 / 35 |
| `reputation` | sem crash | 184 M | 141 / 155 |
| `presence` | sem crash | 2,2 M | 107 / 1 |
| `relay` | sem crash | 0,6 M | 346 / 78 |
| `manager` | sem crash | 3,1 M | 395 / 135 |

A cobertura de `cell`, `descriptor` e `presence` parou cedo, o que é esperado: o parser é pequeno, e `presence` rejeita quase tudo porque a entrada precisa ser um registro cifrado válido. Os alvos `relay` e `manager` são os que mais ganhariam com mais tempo (são lentos por execução e ainda achavam caminhos novos). Uma campanha de verdade ainda deve rodar horas por alvo e com AddressSanitizer. O link (Noise) não tem alvo: o `snow` é a biblioteca que decifra, e fuzzing do protocolo de link exigiria um par instrumentado.

### O que ainda falta na Fase 3

- Coletar as medições entre máquinas reais (roteiro e scripts prontos, nenhuma medição coletada).
- Campanha longa de fuzzing (horas por alvo, com AddressSanitizer em nightly) e alvos novos para o quadro do link e para o gerente de descobertas.

## Camada de mídia: chamada de voz (`clam::media`)

Só voz 1:1. Vídeo, grupo (MLS) e Opus ficam de fora (veja "Limites"). A camada é *sans-IO*: não abre socket nem tem relógio; você entrega bytes e o tempo em ms.

```text
microfone ─> Codec::encode ─> VoiceCall::send ─> [200 B] ─> CircuitClient::send
alto-falante <─ Codec::decode <─ VoiceCall::playout <─ jitter <─ VoiceCall::receive <─ Event::Data
```

### Uso

```rust
use clam::media::{VoiceCall, codec::Mulaw, keys::{CallKeys, Ephemeral}};

// Cada lado gera uma efêmera e a manda ao outro dentro do convite/resposta da chamada.
let eph = Ephemeral::generate();
// Chamador: my_static = sua chave do pareamento, peer_static = a do contato.
let keys = CallKeys::initiator(&my_static, &eph, &peer_static, &peer_eph)?;
// Chamado: CallKeys::responder(...) com os mesmos argumentos espelhados.
let mut call = VoiceCall::new(&keys, Mulaw::new());

// A cada 20 ms, com 160 amostras i16 (8 kHz) do microfone:
let pacote = call.send(&pcm)?;           // 200 bytes -> CircuitClient::send(&pacote)
// Ao chegar Event::Data(bytes):
call.receive(agora_ms, &bytes)?;         // erro = pacote descartado (forjado, repetido, tamanho errado)
// A cada 20 ms, também:
for quadro in call.playout(agora_ms) { /* despeje no alto-falante */ }
```

`cargo run --example voice -- --loss 5 --jitter-ms 60 --seconds 10` simula uma chamada com perda e jitter e imprime as estatísticas.

### O que cada parte faz

- **Chaves (`keys`)**: handshake de 3 DH no estilo Noise `KK` (efêmera×efêmera, estática×efêmera nos dois sentidos) + HKDF-SHA256 com a transcrição. Sigilo adiante por chamada, autenticação mútua pelas chaves do pareamento, uma chave por direção. Não há mensagem de confirmação: o primeiro pacote válido confirma. Pontos de ordem baixa são recusados (`MediaError::BadKey`).
- **Pacote (`packet`)**: `seq (4, autenticado) ‖ ChaCha20-Poly1305(len ‖ quadro ‖ zeros)`, **sempre 200 bytes**, cabe nos 203 de uma célula de 2 saltos. Quadro de fala, de silêncio ou de tamanho diferente: mesmo tamanho. Janela anti-replay de 128 pacotes; o `seq` só é marcado como visto depois de autenticar.
- **Jitter (`jitter`)**: reordena, marca perda e adapta o atraso. Começa em 2 quadros (40 ms); sobe 1 quadro a cada quadro que chega tarde demais, desce 1 depois de 250 quadros em dia; limite 200 ms. Salto de 50 quadros para frente reinicia o buffer.
- **Codec (`codec`)**: trait `Codec` (`encode`, `decode(None)` = perda com PLC). Vêm o G.722 (16 kHz, 320 amostras ↔ 160 B por quadro de 20 ms, 64 kbps, banda larga; usado pela GUI) e o G.711 µ-law (8 kHz, 160 B, banda estreita). Os dois lados precisam usar o mesmo codec. PLC: repete o último quadro com metade da amplitude, silêncio depois de 3 perdas seguidas.
- **Sinalização (`signal`)**: `Caller::invite` → `Incoming::parse` → `accept`/`reject` → `Caller::answer`, em quadros de 200 B. O convite leva a identidade de quem liga **cifrada** (chave de `DH(e_i, s_r)`) e a hora (válido por 60 s): o relay de saída vê que há um convite, mas não quem liga. Quem finge ser um contato não deriva as chaves da chamada. Replay de um convite gravado, dentro dos 60 s, só faz o telefone tocar.
- **Desligar**: pacote autenticado do mesmo tamanho dos de voz (campo `tipo` dentro da cifra); ninguém de fora derruba a chamada.
- **Sem DTX**: o cover do circuito já manda 1 célula a cada 20 ms, então calar não economiza banda e só revelaria quando você fala.

Banda total por sentido com o circuito: 1 célula de 256 B por 20 ms (~100 kbps), a mesma com ou sem fala.

### Limites

- **Opus não está incluído**: exige `libopus` (C) e `cmake` neste ambiente. Implemente `Codec` com a crate de sua escolha; o quadro codificado deve ter até 167 bytes (`MAX_PAYLOAD`), sobra para Opus a 32–64 kbps em 20 ms.
- Sem controle de congestionamento nem retransmissão (voz tolera perda), sem cancelamento de eco, sem relógio de áudio (o app amostra o dispositivo e lida com deriva).
- `tests/media.rs` cobre rede limpa, perda de 5% com jitter e duplicatas, pacotes forjados e áudio bit a bit idêntico ao do codec. Não há teste sobre o circuito real.

### Telefone: chamar e atender pelo circuito (`clam::phone`, feature `net`)

`media` é sans-IO; `phone` liga tudo a tokio, ao circuito e a um destino TCP.

**Quem atende** (papel de destino). `Phone` escuta num endereço TCP; é o endereço que a política de saída dos relays mapeia (`contato:9`, veja `allow_list`). Cada circuito chega como uma conexão com quadros `[u16 BE tamanho][dados]`.

```rust
use clam::phone::Phone;
use clam::media::codec::Mulaw;

// contatos: chaves públicas estáticas (32 bytes) dos pareamentos que podem ligar.
let mut phone = Phone::bind("0.0.0.0:7000".parse()?, minha_chave, contatos).await?;
while let Some(convite) = phone.next_incoming().await {
    println!("liga: {:x?}", convite.caller().as_bytes());
    let mut chamada = convite.accept(Mulaw::new()).await?;   // ou convite.reject().await
    // microfone -> chamada.send(pcm).await ; alto-falante <- chamada.next_pcm().await
}
```

**Quem liga**:

```rust
use clam::phone::call;

let cliente = CircuitClient::spawn(CircuitManager::new(cfg, "contato:9", relays), Duration::from_secs(3));
let mut chamada = call(cliente, &minha_chave, &chave_do_contato, Mulaw::new(), Duration::from_secs(10)).await?;
// Erros: CallError::{NoCircuit, NoAnswer, Rejected, Closed, Media}
```

- `ActiveCall::send(pcm)`: um quadro do microfone (160 amostras com `Mulaw`), a cada 20 ms. `next_pcm()`: o próximo quadro para o alto-falante, no ritmo do codec; vem quadro escondido (PLC) quando falta pacote, então o fluxo não para até a chamada acabar (`None`). `stats()`, `hangup()` (ou largar o valor) desliga.
- **Contatos desconhecidos** não tocam e **não recebem resposta**: o telefone não revela que existe. `contacts` vazio = ninguém liga.
- **Failover**: o circuito reserva abre outra conexão com o `Phone`; se o ativo cai, a voz passa a chegar por ela sem convite. O `Phone` experimenta o pacote nas chamadas em andamento e a que o autenticar passa a responder por aquela conexão. Pacote repetido nunca muda a rota.
- **Desligar** sai em 3 cópias (20 ms entre elas) porque a voz tolera perda, mas o desligar não deveria depender de uma célula só.
- `tests/phone.rs` cobre: chamada por relays reais com áudio nos dois sentidos e desligar; recusa; desconhecido sem resposta; troca de conexão (failover) sem convite; e queda do primeiro relay do circuito ativo no meio da chamada (6 relays, voz nos dois sentidos): a voz volta sozinha com lacuna de ~400 ms em 127.0.0.1 (limite do teste 800 ms), sem novo convite.
- Limites: o `Phone` aceita TCP de qualquer origem (não só dos relays), então protege-se só pela autenticação dos pacotes: quem não é contato é ignorado, mas pode gastar CPU com lixo. Uma chamada por contato e por vez não é imposta. Não há toque/ocupado, e a chamada em grupo não existe.

### Conexão simples: diretório só para o primeiro contato

Só o **primeiro contato** usa servidor. Um diretório (`src/directory.rs`) guarda, por 120 s, "chave pública → relay" de quem está online; cada `Node` (`src/node.rs`) republica a sua entrada a cada 40 s. Para ligar, você consulta a chave do contato uma vez, monta o circuito de 2 saltos até o relay dele e o resto (convite, voz) segue P2P pelos relays, sem passar pelo diretório.

- Quem publica prova a posse da chave sem assinatura (DH com a chave do servidor + HMAC do pedido); ninguém publica por outra chave.
- O diretório vê IPs e chaves de quem publica e consulta, nunca o conteúdo das chamadas.
- Cada pessoa é relay de si mesma. Com só duas pessoas, o primeiro salto é o relay do chamador e o chamado vê o IP dele; com mais gente online, o primeiro salto é sorteado do diretório.
- **Relógio:** o diretório manda a hora dele no primeiro contato e todos usam essa hora nos convites (`src/clock.rs`). Computadores com a hora errada deixam de recusar chamadas (o convite vale 60 s).
- **Atrás de roteador:** com o diretório na internet, o nó pede ao roteador para encaminhar a porta do relay por UPnP (`src/upnp.rs`, renovado a cada 40 s) e publica o IP externo. Sem UPnP, ou com CGNAT, a janela avisa o motivo; então abra a porta 4001 ou informe `public_addr`. Se o diretório está na LAN (IP privado), o UPnP não é usado.
- **Atrás de NAT sem abrir porta**: o servidor pode rodar também um relay (`--relay`). O nó marca "usar o relay do servidor" (`NodeConfig.via_server`): fica com uma conexão de controle de saída aberta e o diretório publica o relay do servidor como o dele. A cada circuito para você, o servidor avisa pelo controle, você abre uma conexão de saída com uma ficha de 16 bytes e o servidor emenda as duas (`REQ_HOST`/`REQ_ATTACH` em `src/directory.rs`). Custo: o servidor vê seu IP e o tráfego cifrado das suas chamadas; ele é o último salto. Cobertura: `tests/node.rs`.

Servidor (uma máquina alcançável pelos dois lados):

```text
cargo run --release --features net --example directory -- 0.0.0.0:7777
# com relay para quem está atrás de NAT (porta TCP 4000 também liberada):
cargo run --release --features net --example directory -- 0.0.0.0:7777 --relay 0.0.0.0:4000 IP-PUBLICO:4000
```

### Salas persistentes (`clam::room`, feature `net`)

Como uma sala do Discord: a sala é um **segredo de 32 bytes** (64 hex) que fica salvo na máquina de quem já entrou. Quem o tem entra de novo quando quiser; para convidar, copie o código da sala.

- O diretório identifica a sala por uma chave Ed25519 derivada do segredo (HKDF). Entrar, sair e listar são pedidos assinados por ela (com a `E` da conexão, sem repetição): sem o segredo não se entra nem se vê quem está lá.
- A lista é de quem está presente (renovada a cada sincronização, vence em `TTL`). O **primeiro membro vivo é o anfitrião**; se ele sai, o próximo assume. Até `MAX_ROOM_MEMBERS` (8) por sala.
- **Malha completa**: o menor `chave pública` de cada par liga para o outro; a voz segue de ponta a ponta entre cada par. `room::Mesh` envia o microfone a todos e mistura (soma saturada) o que chega.
- Os membros listados são liberados no telefone (`Node::allow`), então ninguém precisa estar na lista de Contatos.
- Na GUI: "Criar / entrar" (código vazio cria uma sala nova), "Copiar código", "Entrar" nas salas salvas, "Sair da sala". As salas ficam em `~/.clam-gui.conf` (campo `rooms`, contém segredos).
- Limite: o diretório vê quem entra em cada sala (chaves e IPs), não a voz.

### Interface gráfica mínima

Guias passo a passo por sistema: [`docs/LINUX.md`](docs/LINUX.md) e [`docs/WINDOWS.md`](docs/WINDOWS.md).

```text
cargo run --release --features gui --example gui
```

1. **Identidade**: cole a chave secreta (64 hex) ou *Gerar nova*; o código de contato (`chave@host:porta`, 64 hex + diretório) aparece com *Copiar*. Passe-o a quem vai te chamar.
2. **Diretório**: `host:porta` do servidor. **Contatos**: chaves públicas que podem ligar para você, separadas por `;` (quem não estiver na lista não recebe resposta; contatos são atendidos automaticamente).
3. **Ficar online** publica você no diretório. **Ligar** pede o código (ou só a chave) do outro; **Desligar** encerra.
4. Os campos são gravados em `~/.clam-gui.conf` (`%USERPROFILE%` no Windows), **inclusive a chave secreta**; no Unix o arquivo fica `0600`.
5. **Áudio real** (feature `audio`): `--features gui,audio` e marque *Microfone e alto-falante*. Usa `cpal`, 16 kHz mono (G.722), filas de 400 ms. O microfone passa pelo `sonora` (WebRTC em Rust puro): filtro passa-altas, cancelamento de eco, supressão de ruído e ganho automático. Fones ainda são o mais seguro. Sem a opção, cai para tom (440 Hz quem liga, 660 Hz quem atende). No Linux a compilação exige `pkg-config` e `libasound2-dev`.
6. `eframe` fica atrás da feature `gui`, fixado em 0.32 (a 0.37 conflita com o libp2p por `js-sys`).

O fluxo antigo, com relays manuais, continua em `examples/call.rs` e `examples/measure.rs`.

## Windows

**Status: não testado.** Todo o desenvolvimento e os testes rodaram em Linux. Nada no código depende de Linux, mas ninguém compilou no Windows ainda. Se algo falhar, trate como bug a corrigir.

O que deve funcionar, pelo que o código mostra:

- **Biblioteca, testes e `measure`**: as dependências (`tokio`, `snow`, `x25519-dalek`, `libp2p`) são Rust e têm suporte a Windows. O código usa só TCP e `std::fs`. O único trecho específico de sistema é `#[cfg(unix)]` em `src/reputation.rs`, que ajusta a permissão do arquivo de reputação para `0600`.
- **Permissão do arquivo de reputação**: no Windows esse ajuste não existe, e o arquivo herda as permissões da pasta. Guarde-o em uma pasta do seu usuário (por exemplo `%LOCALAPPDATA%`), não em uma pasta compartilhada.
- **Firewall**: nas máquinas dos relays, libere a porta TCP de escuta no Windows Defender Firewall, senão o `measure` e os clientes não conectam.

Para compilar e testar (PowerShell):

```powershell
rustup default stable-x86_64-pc-windows-msvc   # precisa das Build Tools do Visual Studio (C++)
cargo test --features net
cargo run --release --features net --example measure -- direct 127.0.0.1:9000
```

**Fuzzing no Windows**: use o WSL2. O `cargo-fuzz` (libFuzzer) tem suporte estável em Linux e macOS, não no Windows. Dentro do WSL2 os comandos da seção de fuzzing valem como estão.

## Mapa do código

| Arquivo | Conteúdo |
| --- | --- |
| `src/pairing.rs` | `PresenceKeys::derive`, `lookup_id(epoch)` |
| `src/presence.rs` | `seal`, `open`, `decoy_*`, `RECORD_LEN` |
| `src/service.rs` | `publish`, `lookup`, `circle_topics`, `PresenceConfig` |
| `src/store.rs` | `RecordStore`, `MemoryStore` |
| `src/runner.rs` | `PresenceRunner`, `RunnerConfig`, `Clock` (feature `net`) |
| `src/descriptor.rs` | `RelayDescriptor`, `RelayDirectory`, `DirectoryConfig` |
| `src/discovery.rs` | `Discovery`, `DiscoveryRunner`, `RELAY_TOPIC` (feature `net`) |
| `src/cover.rs` | `CoverProfile`: perfis de custo do cover (`Off`, `Economy`, `Standard`, `Strong`) |
| `src/reputation.rs` | `Reputation`, `ReputationConfig`, `RelayStats` |
| `src/intro.rs` | `IntroSelector`, `IntroConfig`, `Selection` |
| `src/circuit/` | `crypto`, `client::ClientCircuit`, `relay::RelayNode`, `manager::CircuitManager` (circuito de 2 saltos, reserva e failover) |
| `src/link.rs` | Noise NK do link (feature `net`) |
| `src/transport.rs` | `RelayServer`, `CircuitClient`, `ExitPolicy` (TCP, feature `net`) |
| `tests/fuzz.rs` | fuzzing determinístico dos parsers e do relay |
| `src/net.rs` | `Node`, `NetStore`, `NetConfig` (feature `net`) |
| `src/circle.rs` | `CircleSecret`, `TopicKind` |
| `examples/call.rs` | chamada de teste (`keygen`, `phone`, `dial`), usada por `scripts/call.sh` e `scripts/call-windows.ps1` |
| `src/audio.rs` | microfone/alto-falante via `cpal`, reamostragem para a taxa do codec (feature `audio`) |
| `examples/gui.rs` | janela mínima (feature `gui`) para ligar/atender sem terminal |
| `src/phone.rs` | `Phone`, `IncomingCall`, `call`, `ActiveCall` (feature `net`) |
| `src/media/` | `VoiceCall`, `keys` (handshake), `packet` (200 B + AEAD), `jitter`, `signal` (convite/aceite), `codec` (`Mulaw`, trait `Codec`) |
| `src/path.rs` | `PathMode`, `PathRequest`, `PathError` |

## Garantias e limites de segurança

- Contra contatos e observadores comuns: ninguém fora do par descobre que você está online, nem liga seus endereços entre períodos.
- **Não** promete anonimato diante de quem monitora a rede inteira (adversário global). Diga isso aos usuários.
- O modo direto expõe o seu IP ao contato; o código exige confiança e aviso confirmado, mas o aviso na interface é responsabilidade do app.
- O relógio é passado como argumento (`agora_unix`); use um relógio confiável. Defasagem maior que uma época faz a busca falhar.
- Não há proteção contra replay de um registro antigo dentro da janela de três épocas.

## Limitações

Ainda **não** existem: TLS/QUIC e travessia de NAT no adaptador (só TCP + Noise), validação de registros na DHT (um nó armazenador pode sobrescrever um registro com lixo; `open` rejeita lixo, mas ele esconde o registro bom até a próxima publicação), failover de circuitos, descoberta e reputação de relays, integração com MLS e vídeo.
`PathMode` só modela e valida o modo; ele não move mídia.
