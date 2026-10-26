# Servidores e salas (plano)

Plano de um conceito de **servidor** como o do Discord, sem servidor de verdade: um grupo de pessoas com salas de voz, onde se vê quem está online e quem está em cada sala. Tudo P2P, sobre o que já existe (relay de si mesmo, circuito de 2 saltos, telefone).

Estado: **plano, nada implementado**. Como a comunicação funciona hoje está em [COMUNICACAO.md](COMUNICACAO.md).

[← Voltar ao README](../README.md)

## O que se quer

1. Um **servidor** tem nome, membros e salas. Entra-se por convite.
2. Ao clicar no servidor, a janela mostra os **membros online**.
3. O servidor tem **salas de voz**; qualquer membro pode criar mais e entrar.
4. Quem está numa sala **aparece nela** para todos os membros, mesmo para quem não entrou.
5. Nada de servidor central, diretório ou coordenador.

## O que o plano reaproveita e o que muda

| Hoje | No plano |
|---|---|
| Só há chamada 1:1, entre contatos | Sala = várias chamadas 1:1 em malha, uma por par |
| O telefone só toca para quem é contato; o resto é ignorado em silêncio | Membro de servidor pode falar com você **só dentro de salas e de presença**, sem ser contato |
| Ninguém vê se um contato está online | Presença entre membros, sob demanda |
| Contatos se reencontram por par (aviso de endereço, DHT por par, âncoras) | Reencontro também por servidor (um registro por membro, não um por par) |
| Identidade X25519 (só troca de chaves) | Chave de assinatura ed25519 derivada da identidade, para assinar operações do servidor |

## Modelo

**Servidor.** `server_id` (16 B aleatórios), `ServerSecret` (32 B, só membros têm), nome, época do segredo e uma lista de membros (chaves X25519 + chave de verificação ed25519 de cada um).

**Papéis.** `dono` (quem criou), `admin` e `membro`. Dono e admins adicionam e removem membros e apagam salas. Qualquer membro cria salas e entra nelas (política do servidor, mudável pelo dono).

**Registro de operações.** O estado do servidor (membros, salas) não fica em lugar nenhum: é um **registro de operações assinadas** que cada membro guarda inteiro. Operações: `AddMember`, `RemoveMember`, `SetRole`, `CreateRoom`, `RenameRoom`, `DeleteRoom`, `Rekey`. Cada uma leva `server_id`, número de sequência por autor, autor e assinatura ed25519. Quem aplica confere a assinatura e a permissão do autor naquele ponto do registro. Ordem total por `(época, lamport, autor)`; conflito de nome de sala se resolve por essa ordem. Operações são pequenas (cabem num quadro de 200 B; nome de sala até 32 B).

**Sala.** `room_id` (16 B), nome, tipo (`voz`; texto fica para depois). A sala só existe como operação `CreateRoom`; quem está nela **não** é estado do registro, é presença.

**Presença.** Cada membro online mantém um estado: `visível`, sala atual (ou nenhuma), versão do registro que conhece, endereço do relay. Vale por um contrato de vida (lease) de ~20 s e é renovado enquanto o membro está online.

## Protocolo

Tudo vai por quadros novos de 200 B no mesmo telefone e circuito de hoje (tags livres: 8 em diante; 1 a 7 já estão em uso), cifrados com uma chave de servidor derivada de `ServerSecret` e da época (HKDF, rótulos próprios). Quem não é membro não os abre nem os distingue de lixo.

| Quadro | Para quê |
|---|---|
| `Hello` | "Estou online": estado de presença + versão do registro + até 3 endereços de relay de outros membros (fofoca de endereços) |
| `HelloAck` | Resposta com o estado de quem respondeu, mesmo formato |
| `OpsWant` / `Ops` | Pedido e entrega de operações que faltam (anti-entropia, um quadro por operação) |
| `RoomInvite` | Convite de chamada de sala (veja abaixo) |

**Ver quem está online.** Ao abrir o servidor na janela, o nó manda `Hello` a cada membro conhecido por um **circuito curto** (abre, manda, espera a resposta, fecha). Não se mantém circuito aberto com todos, porque cada circuito aberto custa cerca de 100 kbps por direção de cobertura. Depois disso:

- cada mudança minha (entrei numa sala, saí, fiquei invisível) vai como `Hello` aos membros que responderam como online;
- enquanto o servidor está aberto na janela, a presença se renova a cada ~10 s; fechado, só quem está em sala renova;
- cada `Hello` e `HelloAck` carrega endereços de outros membros e a versão do registro, então a rede se atualiza sozinha (fofoca) e quem está atrás pede `OpsWant`.

Quem está numa sala renova a presença de sala com todos os membros **online**, porque é isso que o faz "aparecer na sala" para quem só está olhando.

**Entrar numa sala.**

1. Anuncio `Hello` com a sala. Os membros online passam a me mostrar naquela sala.
2. Dos membros que já estão **na sala** (vejo pelos `Hello`), eu chamo cada um (o recém-chegado sempre chama; em empate de entrada, chama quem tem a chave menor). Cada par tem uma única chamada, sem o problema de dois lados ligarem ao mesmo tempo.
3. O convite é o convite de chamada de hoje com um sufixo versionado: `server_id` + `room_id`. O destino aceita sozinho, sem tocar, se ele estiver nessa sala e quem chama for membro. Fora disso, rejeita em silêncio.
4. Chamada de sala usa o codec, o jitter, as chaves e os pacotes de voz como hoje. Nada muda na mídia.

**Sair.** Hangup nos pares e `Hello` sem sala. Se cair sem avisar, o contrato de vida (~20 s) tira a pessoa da sala, e a chamada de cada par cai pelo vigia de silêncio que já existe.

**Tamanho da sala.** Malha: cada pessoa envia e recebe N-1 fluxos, cerca de 100 kbps por par em cada direção no modo privado (80 kbps de voz mais cobertura). Limite de **8 pessoas por sala** (≈700 kbps) no começo; a janela avisa ao passar do que o upload suporta. Salas maiores exigiriam um membro que recebe e retransmite; fica fora do plano inicial.

**Tela numa sala.** Quem compartilha serve só quem clicar em "assistir" (um circuito de tela por espectador, como hoje). Limite de 3 espectadores por compartilhamento. Fase própria.

## Entrada, saída e reencontro

**Convite.** Um texto (como o código de contato) com `server_id`, `ServerSecret`, nome, época, chave de verificação do dono e dicas de relay de 2 a 3 membros. Validade e uso único opcionais, conferidos por quem aceita o `Join`. Quem recebe o convite manda `Join` a um dos membros da dica; um admin responde com `AddMember` assinado e todos aprendem pelo registro.

**Remover membro.** `RemoveMember` seguido de `Rekey`: novo `ServerSecret` e época, enviado a cada membro restante cifrado com a chave X25519 dele. O removido continua com o segredo antigo, mas não abre mais nada novo, e os telefones dos membros param de aceitá-lo. **Limite honesto:** ele guarda o que já viu (nomes, endereços de então).

**Reencontro por servidor.** Cada membro publica na DHT (BEP 44, como hoje) **um** registro por servidor e período, cifrado com `ServerSecret`, no endereço `H(ServerSecret, chave do membro, dia)`. Qualquer membro o acha sabendo só a chave do outro. Troca de endereço também viaja no `Hello`. Âncoras valem como hoje.

## Privacidade (diferente de contato)

- **Membros veem o seu IP.** O relay de quem recebe vê o relay de quem liga, que é a sua máquina. Com contatos que você escolheu isso já é assim; num servidor grande são pessoas que você não conhece. **A janela deve avisar isso antes de entrar num servidor.**
- **Presença revela horário.** Membros sabem quando você está online e em que sala. Existe o estado **invisível**: não responde `Hello` e não entra em sala.
- **O registro é lista de membros inteira para todos.** Nada de papéis ocultos ou membros ocultos.
- **Cobertura:** quadros de servidor viajam em circuito com a cobertura de hoje e dentro do link com preenchimento; um observador não distingue presença de voz ou lixo.
- **Tamanho de servidor:** máximo de **50 membros** no começo, porque a presença é, no pior caso, N² mensagens de 200 B.
- **Saída sem `Rekey`:** quem sai por conta própria continua com o segredo. Só uma troca de segredo (`Rekey`) o deixa de fora; a janela oferece isso ao dono.

## Janela

- **Trilho de servidores** à esquerda da barra lateral atual (ícones redondos, o primeiro é o Início), como o Discord. Botão `+` para criar ou entrar por convite.
- Ao clicar num servidor: **coluna de salas** (cada sala com seus participantes por baixo, com o indicador de quem fala) e **coluna de membros** (online primeiro, offline esmaecidos). Clique numa sala entra nela; clique duplo num membro abre o contato dele.
- O painel de chamada atual passa a mostrar a lista de pessoas da sala, com mudo, volume e medidor por pessoa.
- Novo `examples/gui/servers.rs` para a tela; o estado e os quadros ficam em `examples/gui/engine/` e na biblioteca (veja abaixo).
- Estado em disco: `~/.clam-gui.servers` (0600), uma seção por servidor com segredo, registro de operações e membros. A identidade já fica no chaveiro; o `ServerSecret` vai no mesmo arquivo de `0600` na primeira fase e para o chaveiro depois.

## Onde vai no código

| Parte | Lugar |
|---|---|
| Registro de operações, papéis, assinatura, convite | `src/server/mod.rs`, `ops.rs`, `invite.rs` (biblioteca, testável sem rede) |
| Quadros `Hello`/`Ops`/`RoomInvite` | `src/media/server.rs` (junto de `update.rs`) |
| Permissão por servidor no telefone | `src/phone.rs`: `Contacts` vira uma permissão com origem (`Contato` ou `Membro(server_id)`), e `Membro` só abre quadros de servidor e convites de sala |
| Presença, anti-entropia, salas, malha | `src/node.rs` ou `src/server/presence.rs`, falando com o `Node` por canais, como as apresentações |
| Reencontro por servidor | `src/rendezvous.rs` (um registro por servidor) |
| GUI | `examples/gui/servers.rs`, `engine/server.rs` |
| Testes | `tests/server.rs` (registro, permissões, convergência, fuzz dos quadros); `tests/node.rs` ganha sala de 3 nós |

## Fases

1. **Registro e convite.** Criar servidor, convidar, entrar, remover, `Rekey`; só biblioteca e testes, com fuzz dos quadros. Sem rede.
2. **Presença.** `Hello`/`HelloAck`, fofoca de endereço, anti-entropia, estado invisível. A janela mostra o servidor e quem está online.
3. **Salas de voz.** Criar sala, `RoomInvite`, malha de até 8, contrato de vida, indicador de quem fala. É onde "aparece na sala para os outros" fica pronto.
4. **Reencontro por servidor** (DHT e âncoras) e testes de queda e troca de IP.
5. **Tela em sala** (assistir sob demanda).
6. Depois, se pedido: texto nas salas, salas com mais de 8 por retransmissor.

## Decisões em aberto

- Quem pode criar sala: qualquer membro (proposta) ou só admin?
- Limite de 50 membros e de 8 por sala: aceitável para o começo?
- Servidor com estranhos: basta o aviso de IP, ou o nó deve exigir que você já tenha pelo menos um contato dentro dele?
- `ServerSecret` no arquivo de 0600 na primeira fase: serve, ou já entra no chaveiro?
