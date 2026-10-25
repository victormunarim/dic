# clam

Núcleo de privacidade da rede de chamadas P2P, em Rust: **modo privado por padrão** e **presença cifrada**.
Implementa a Fase 1 (pareamento e chaves de presença) e o Fase 2 (publicação, busca e renovação automática de presença) e o núcleo da Fase 3 (circuito de 2 saltos).
O transporte libp2p é opcional (feature `net`); não há relays; veja [Limitações](docs/SEGURANCA.md#limitações).

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

## Documentação

| Documento | Conteúdo |
|---|---|
| [docs/PRIVACIDADE.md](docs/PRIVACIDADE.md) | Pareamento, presença cifrada, busca, círculos (Fases 1 e 2) |
| [docs/CIRCUITO.md](docs/CIRCUITO.md) | Circuito de 2 saltos, transporte, reputação, cover traffic, fuzzing (Fase 3) |
| [docs/CHAMADAS.md](docs/CHAMADAS.md) | Mídia, telefone, diretório de primeiro contato, salas, GUI mínima |
| [docs/GUI.md](docs/GUI.md) | GUI: sala, volume, apelidos, remover membro, modo sem diretório |
| [docs/ENCONTRO.md](docs/ENCONTRO.md) | Reencontrar contatos sem diretório: aviso de endereço novo e ponto de encontro na DHT (fases 1 e 2 prontas) e a proposta das demais camadas |
| [docs/SEGURANCA.md](docs/SEGURANCA.md) | Garantias, limites, limite de taxa, troca de segredo, estado em disco |
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
| `examples/gui/` | janela mínima (feature `gui`) para ligar/atender sem terminal; `conf`, `engine` (sem janela, testável) e `ui` |
| `src/phone.rs` | `Phone`, `IncomingCall`, `call`, `ActiveCall` (feature `net`) |
| `src/media/` | `VoiceCall`, `keys` (handshake), `packet` (200 B + AEAD), `jitter`, `signal` (convite/aceite), `update` (aviso de endereço novo), `codec` (`Mulaw`, trait `Codec`) |
| `src/path.rs` | `PathMode`, `PathRequest`, `PathError` |
