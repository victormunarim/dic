# clam

Chamadas de voz e de tela P2P em Rust, **sem servidor nem diretório**: cada pessoa é o relay de si mesma, e toda chamada passa por um circuito de 2 saltos cifrado em camadas, com tráfego de cobertura.
O aplicativo (`examples/gui`) liga com a chave pública do contato e o endereço do relay dele; como a comunicação funciona está em [docs/COMUNICACAO.md](docs/COMUNICACAO.md).
Além do aplicativo, o crate tem uma camada de biblioteca (presença cifrada na DHT libp2p, círculos, reputação de relays) que o aplicativo ainda não usa; veja os [Conceitos](#conceitos-da-camada-de-biblioteca) e as [Limitações](docs/SEGURANCA.md#limitações).

## Instalação e testes

```sh
cargo test                       # testes unitários e doctest
cargo run --example quickstart   # fluxo completo, ponta a ponta
cargo doc --open                 # documentação da API
```

Requer Rust com edition 2024 (1.85+).
O crate reexporta `ed25519_dalek`, `x25519_dalek` e `rand_core` (`clam::ed25519_dalek` etc.), para você usar as mesmas versões das chaves que ele espera.

## Conceitos da camada de biblioteca

Valem para os módulos `pairing`, `presence`, `service`, `circle`, `intro` e `reputation`. O aplicativo reencontra contatos por outro caminho, descrito em [docs/ENCONTRO.md](docs/ENCONTRO.md).

- **Pareamento:** os dois dispositivos trocam chaves públicas X25519 uma única vez. Do segredo compartilhado, o HKDF deriva uma chave de cifra e uma semente de busca. Só os dois lados as têm.
- **Endereço que gira:** o identificador de busca na DHT é derivado da semente e do número do período (época, 1 hora por padrão). Quem observa a DHT não liga o endereço de hoje ao de ontem.
- **Registro de presença:** contém só pontos de introdução (endereços de relays), nunca o IP. É cifrado (ChaCha20-Poly1305), assinado (ed25519) e tem sempre 668 bytes, qualquer que seja o conteúdo. O endereço entra como dado autenticado, então o registro não vale em outro endereço.
- **Consultas falsas:** cada busca real vem misturada e embaralhada com consultas a endereços aleatórios.
- **Círculos:** os tópicos de presença e de anúncio de call vêm do segredo do grupo. Quem não é membro não os deriva.
- **Modos de caminho:** 2 saltos (padrão), 1 salto e direto.

## Documentação

| Documento | Conteúdo |
|---|---|
| [docs/COMUNICACAO.md](docs/COMUNICACAO.md) | **Como a comunicação funciona**, ponta a ponta: link, circuito, chamada, tela, reencontro e o que cada parte vê |
| [docs/PRIVACIDADE.md](docs/PRIVACIDADE.md) | Camada de biblioteca: pareamento, presença cifrada, busca, círculos |
| [docs/CIRCUITO.md](docs/CIRCUITO.md) | Circuito de 2 saltos, transporte, cover traffic, reputação, fuzzing |
| [docs/CHAMADAS.md](docs/CHAMADAS.md) | Voz, telefone, tela, conexão sem servidor |
| [docs/GUI.md](docs/GUI.md) | A janela: abas, contatos, áudio, tela, opções |
| [docs/ENCONTRO.md](docs/ENCONTRO.md) | Reencontrar contatos sem servidor: aviso de endereço novo e ponto de encontro na DHT, âncoras e contato de duas vias |
| [docs/SERVIDORES.md](docs/SERVIDORES.md) | Servidores e salas de voz com presença, tela na sala e reencontro por servidor, tudo P2P |
| [docs/SEGURANCA.md](docs/SEGURANCA.md) | Garantias, limites, chaveiro |
| [docs/LINUX.md](docs/LINUX.md) / [docs/WINDOWS.md](docs/WINDOWS.md) | Instalação por sistema |

Para ligar a alguém: rode o `examples/gui` (veja [docs/GUI.md](docs/GUI.md)) ou, em código, `Node::start` e `Node::dial` ([docs/CHAMADAS.md](docs/CHAMADAS.md)).

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
| `src/transport/` | `RelayServer`, `CircuitClient`, `ExitPolicy` (TCP, feature `net`) |
| `tests/fuzz.rs` | fuzzing determinístico dos parsers e do relay |
| `src/net.rs` | `Node`, `NetStore`, `NetConfig` (feature `net`) |
| `src/circle.rs` | `CircleSecret`, `TopicKind` |
| `examples/call.rs` | chamada de teste (`keygen`, `phone`, `dial`), usada por `scripts/call.sh` e `scripts/call-windows.ps1` |
| `src/audio.rs` | microfone/alto-falante via `cpal`, reamostragem para a taxa do codec (feature `audio`) |
| `examples/gui/` | o aplicativo (feature `gui`): `conf` (arquivos em disco), `engine` (sem janela, testável), `ui` (abas) e `theme` |
| `src/node.rs` | `Node`: relay + telefone + fila de endereços (feature `net`) |
| `src/rendezvous.rs` | ponto de encontro na DHT Mainline e âncoras (feature `net`) |
| `src/upnp.rs`, `src/timer.rs` | porta no roteador; temporizador de 1 ms no Windows (feature `net`) |
| `src/video.rs`, `src/hwenc.rs` | codificação da tela: controle de taxa e qualidade, NACK; codificador por hardware opcional (feature `video`) |
| `src/capture.rs`, `src/portal.rs` | captura da tela: X11 e GDI; portal do desktop (features `capture`, `portal`) |
| `src/phone.rs` | `Phone`, `IncomingCall`, `call`, `ActiveCall` (feature `net`) |
| `src/media/` | `VoiceCall`, `keys` (handshake), `packet` (200 B + AEAD), `jitter`, `signal` (convite/aceite), `update` (aviso de endereço novo), `screen` (pacotes, NACK e taxa da tela), `codec` (µ-law, G.722, Opus; trait `Codec`) |
| `src/path.rs` | `PathMode`, `PathRequest`, `PathError` |
