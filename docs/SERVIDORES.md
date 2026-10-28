# Servidores e salas

Um **servidor** como o do Discord, sem servidor de verdade: um grupo de pessoas com salas de voz, onde se vê quem está online e quem está em cada sala. Tudo P2P, sobre o que já existe (relay de si mesmo, circuito de 2 saltos, telefone).

Estado: **implementado** (biblioteca em `src/server/`, rede em `src/server/net.rs`, janela na aba *Servidores*). Como a comunicação funciona por baixo está em [COMUNICACAO.md](COMUNICACAO.md).

[← Voltar ao README](../README.md)

## O que se tem

1. Um servidor tem nome, membros e salas. Entra-se por **convite**.
2. Ao clicar no servidor, a janela mostra os **membros online e offline**.
3. O servidor tem **salas de voz**; qualquer membro cria salas e entra nelas.
4. Quem está numa sala **aparece nela** para todos os membros, mesmo para quem não entrou, com o aro verde de quem fala.
5. Quem está numa sala pode **transmitir a tela**; os outros da sala assistem sob pedido (até 3 por transmissão).
6. Nada de servidor central, diretório ou coordenador.

Limites: **50 membros** por servidor, **32 salas**, **8 pessoas por sala**, nomes de até 32 bytes.

## Modelo

**Servidor.** `server_id` (16 B aleatórios), `ServerSecret` (32 B, só membros têm), nome, época do segredo e um registro de operações.

**Papéis.** `dono` (quem criou), `admin` e `membro`. **Qualquer membro cria salas, entra nelas e convida pessoas dos próprios contatos.** Só dono e admins removem membros e apagam ou renomeiam salas dos outros; só o dono troca papéis.

**Registro de operações.** O estado do servidor (membros, salas, nomes) não fica em lugar nenhum: é um **registro de operações assinadas** (ed25519) que cada membro guarda inteiro. Operações: `AddMember`, `RemoveMember`, `SetRole`, `CreateRoom`, `RenameRoom`, `DeleteRoom`, `Rekey`, `SetName`. Cada uma leva autor (8 B da chave), relógio de Lamport, número de sequência por autor e assinatura que cobre também o `server_id`. A ordem é total por `(lamport, autor, seq)`; quem aplica confere a assinatura e a permissão do autor naquele ponto do registro, e convergem todos para o mesmo estado, em qualquer ordem de chegada. Uma operação cabe num quadro (corpo até 75 B). O registro guarda no máximo 4096 operações, com cota por autor (512), um poço pequeno para quem ainda não é membro (64) e uma reserva do dono (256); veja [SEGURANCA.md](SEGURANCA.md).

**Sala.** Só existe como operação `CreateRoom`; quem está nela **não** é estado do registro, é presença.

**Presença.** Cada membro online diz o que é: visível ou não, sala atual, se transmite a tela, resumo do registro e época do segredo. `Hello` a cada 15 s a quem está online (a quem está offline, uma sondagem por minuto); vale por 40 s (`LEASE`). Quem some deixa a sala quando o contrato vence, e a chamada dele cai pelo vigia de silêncio.

## Protocolo

Um quadro de servidor tem 200 B, tag **8**, e viaja pelo mesmo telefone e circuito de hoje:

```text
tag 8 ‖ servidor(4) ‖ remetente(8) ‖ tamanho(1) ‖ nonce(12) ‖ AEAD(tipo ‖ corpo) ‖ bytes aleatórios
```

A chave é **par a par**: `HKDF(DH(minha secreta, pública do outro), "clam/servidor/quadro/v1" ‖ server_id)`. Só os dois a calculam e o remetente prova a cada quadro que a chave bate. O telefone aceita quadros de servidor de quem é **membro de algum servidor em comum**, mesmo sem ser contato; o resto continua ignorado em silêncio.

| Mensagem | Para quê |
|---|---|
| `Hello` / `HelloAck` | "Estou online": sala, se transmite a tela, resumo do registro (`fp`, `count`) e época. Resposta no mesmo formato |
| `Vector` | Até onde cada lado tem as operações de cada autor (anti-entropia, em páginas) |
| `Op` | Uma operação que faltava |
| `MyAddr` | Endereço do relay do remetente (fofoca de endereço; o `seq` só cresce) |
| `Join` / `Joined` | O convidado pede para entrar, com a chave de verificação e o relay; o convidante responde e manda as operações |
| `Gift` | O segredo novo depois de um `Rekey` |
| `Ack` | "Recebi o quadro cujo nonce começa por estes 8 bytes" |
| `InviteChunk` | Um pedaço do convite, só entre contatos (veja *Entrega do convite pela rede*) |
| `Watch` | "Quero assistir à sua tela" / "parei" (só entre quem está na mesma sala) |
| `Chat` | Uma linha do bate-papo da sala (texto UTF-8, até 157 B, sem caractere de controle; só entre quem está na mesma sala) |

**Entrega.** O `ServerHub` (`src/server/net.rs`) junta os quadros por destinatário e manda em lote por um circuito (abre, manda, espera os `Ack`, fecha). Não se mantém circuito aberto com todos: cada um custaria cerca de 100 kbps de cobertura. Sem `Ack` a entrega é tentada de novo, e se falhar o nó procura o membro pelo registro do servidor (abaixo).

**Convite.** Texto `clam-servidor:` + hex, cifrado com `HKDF(DH(quem convida, convidado))`: leva segredo, época, chave do dono, nome, o relay de quem convida e até 2 dicas de relay. Como depende da chave do convidado, só ele o abre; repassar não funciona. O convidado manda `Join` a quem o convidou, que responde com `AddMember` assinado por ele.

**Entrega do convite pela rede.** *Enviar convite* parte o texto em até 8 pedaços (`InviteChunk`, 151 B cada) e os entrega ao contato pelo mesmo circuito dos outros quadros, sem outro canal. Como o convidado ainda não conhece o servidor, a chave desses quadros usa um servidor fictício (`INVITE_SERVER`, todo zero) no lugar do `server_id`. Só se aceitam pedaços de **contatos** (a lista do telefone); de quem não é contato some sem resposta. Cada pedaço é confirmado (`Ack`); quando chegam todos, o convite é aberto e vira um aviso na aba *Servidores* (*Convites recebidos*: Aceitar ou Recusar). O endereço do contato é procurado como numa chamada (guardado, DHT, âncoras). Se ele estiver offline, o convidante repete a entrega a cada 60 s, até 24 h; depois da confirmação, só lembra a cada 10 min (para quem reiniciou antes de aceitar). Quem recusa não recebe o mesmo convite de novo enquanto o programa estiver aberto. O texto para copiar (*Gerar só o texto*) continua existindo, para quem não é contato.

**Relógio.** O `lamport` de uma operação tem de ser, no máximo, um acima do maior que veio antes dela na ordem do registro; senão ela fica guardada mas não conta (e passa a contar quando as que faltam chegam). Assim ninguém trava o servidor assinando `u32::MAX`.

**Remover membro.** `RemoveMember` seguido de `Rekey`: o segredo novo vai a cada membro restante em `Gift`. O `Rekey` leva um compromisso do segredo novo (`secret_commit`, hash do servidor, da época e do segredo), e um `Gift` só troca o segredo se o reproduz: um membro comum não impõe um segredo falso. Num `Rekey` do formato antigo (sem compromisso) o `Gift` só vale se vier de quem administra. O removido guarda o que já viu (nomes, endereços de então), mas não abre mais nada novo.

## Salas de voz

1. Entro na sala: `Hello` com a sala. Os membros online passam a me mostrar nela.
2. O motor calcula a **malha** (quem está online na mesma sala) e avisa a janela (`Event::Mesh`). O nó passa a aceitar convite de chamada dessas pessoas mesmo que não sejam contatos (`Node::set_room_peers`).
3. Entre dois da malha, **quem tem a chave menor disca**; o outro atende sozinho, sem tocar, e só pares da malha (os demais são recusados). Cada par tem uma só chamada.
4. A chamada é a de sempre (codec, jitter, chaves e pacotes de voz iguais). A janela soma os quadros de todos num só alto-falante (a cada 20 ms, com limite de volume) e leva o microfone a cada par. Quem cai com a malha ainda valendo é religado a cada 4 s pelo lado que discou.
5. O aro verde vem da energia (RMS) do que cada um enviou.

Malha: cada pessoa envia e recebe N-1 fluxos, cerca de 100 kbps por par e direção no modo privado. Por isso o limite de 8 pessoas (≈700 kbps).

## Tela numa sala

- Quem transmite clica em *Compartilhar tela* na própria linha da sala. A presença passa a dizer que ele transmite, e os outros veem *Assistir* ao lado do nome.
- Quem clica em *Assistir* manda `Watch` e repete a cada 5 s até a imagem chegar. Quem transmite abre um **circuito de tela por espectador** (chaves novas da chamada com aquela pessoa) e manda o **mesmo quadro codificado** a todos; um codificador só, a taxa segue o pior espectador. Limite de **3 espectadores**; os outros ficam sem imagem. Um pedido repetido com o circuito aberto há mais de 8 s o reabre.
- As escolhas da aba *Vídeo* (privacidade, fluidez ou nitidez, placa de vídeo) valem aqui também.
- As imagens aparecem em *Telas da sala*, na aba *Servidores*, e se ampliam como na chamada 1:1.

## Bate-papo da sala

- Cada sala tem o seu bate-papo de texto, mostrado no centro da janela quando você está na sala, visível só para quem está nela. Escrever manda um `Chat` a cada pessoa da malha (um quadro de 200 B por pessoa, pelo mesmo caminho dos outros quadros de servidor); não há mensagem para quem está fora da sala.
- Quem recebe confere que o remetente é membro e está online **na mesma sala que ele**; senão ignora. O texto é limpo na entrada (sem controle, no máximo 157 bytes por linha; o envio corta no limite).
- **Nada vai a disco.** O histórico fica em memória, até 100 linhas por sala, e é esquecido quando a sala fica **sem ninguém** (ou é apagada), segundo a presença que cada lado enxerga. Reiniciar o programa também apaga.
- **Limites conhecidos:** quem entra depois não vê o que foi dito antes (não há sincronização de histórico); quem sai e volta com a sala ainda ocupada mantém só o que recebeu enquanto estava dentro; não há confirmação de entrega nem ordenação entre remetentes (cada lado mostra na ordem em que as linhas chegam); a linha leva só texto. Uma versão antiga ignora o quadro `Chat` (tipo desconhecido), então quem a usa não vê a conversa, mas o resto da sala segue normal.
- Quem observa a rede vê mais um quadro de 200 B a cada linha, dentro do circuito com cobertura; o tamanho é o mesmo dos demais quadros e o conteúdo é cifrado par a par.

## Reencontro por servidor

O endereço de um membro pode mudar enquanto ele está offline. Cada membro publica, na DHT e nas âncoras, **um registro por servidor e dia**, cifrado com o `ServerSecret` e achado por `HKDF(ServerSecret, chave do membro, dia)`. Qualquer outro membro o acha sabendo só a chave do membro (`src/media/update.rs`: `server_rendezvous`, `seal_server_record`). Troca de endereço também viaja em `MyAddr`.

- A busca só roda quando uma entrega falha, e no máximo a cada 120 s por membro.
- **Limite:** como todos os membros têm o segredo, um membro pode gravar no lugar de outro. O registro é só uma **dica de relay**: a chamada confere a identidade de quem atende, então o pior efeito é a dica errada fazer a tentativa falhar.

## Privacidade

- **Membros veem o seu IP.** O relay de quem recebe vê o relay de quem liga, que é a sua máquina. A janela avisa antes de convidar e de aceitar, e exige o aceite do aviso.
- **Presença revela horário e sala.** Existe o estado **invisível** (clique na bolinha de estado do perfil, no pé da coluna de salas: cheia = aparecendo, só o aro = invisível): não responde `Hello` e não entra em presença.
- **O registro é a lista de membros inteira para todos.** Não há papéis nem membros ocultos.
- **Cobertura:** os quadros de servidor viajam em circuito com a cobertura de hoje, dentro do link com preenchimento.
- **Saída sem `Rekey`:** quem sai por conta própria continua com o segredo. Só uma troca de segredo o deixa de fora.

## Janela

- **Convites recebidos**: cartão no alto da aba, com *Aceitar* (exige o aviso de IP) e *Recusar*.
- **Trilho de servidores** na aba *Servidores*: avatares, *+* para criar ou aceitar convite.
- No servidor: o cabeçalho da coluna de salas tem o ícone de porta com seta (*Sair do servidor*; abre um item vermelho *Confirmar saída*). As **salas** ficam na coluna à esquerda, ao lado do trilho de servidores (um clique entra; na sala em que você está, uma subseção "Sala conectada" aparece acima do perfil, com o ✖ vermelho que sai dela; quem está em cada sala aparece embaixo, com aro verde em quem fala; 🗑 apaga; o cartão apagado com + no fim da lista, que ao clicar vira o campo do nome (Enter cria, Esc cancela),). Trocar de sala exige sair da atual antes. Na sala em que você está, 🖥 compartilha a sua tela e 👁 assiste à de quem transmite, na própria linha da pessoa. O **centro** da janela é o **bate-papo** da sala em que você está (com as telas da sala acima dele, quando há); sem sala, uma dica. Os **membros** (online primeiro, papel, sala em que estão; *Promover*/*Rebaixar* e *Remover* ficam no menu ⋮ de cada um) ocupam uma coluna à direita, de ponta a ponta da janela, com o **convite** no fim da lista, dentro do cartão de membros (um cartão apagado com +, que ao clicar vira o formulário no lugar) (escolha de um contato, aviso de IP, *Enviar convite* e, à parte, o texto para copiar).
- Estado em disco: `~/.clam-gui.servers` (modo `0600`): segredos, registros e membros de cada servidor, regravado a cada 5 s se mudou. Sem chaveiro por servidor, para funcionar onde não há chaveiro.

## Onde está no código

| Parte | Lugar |
|---|---|
| Operações, registro, papéis, convite | `src/server/op.rs`, `registry.rs`, `invite.rs` |
| Quadros | `src/server/wire.rs` |
| Motor (presença, anti-entropia, salas, malha, visão para a janela) | `src/server/engine.rs` |
| Rede (lotes, `Ack`, busca) | `src/server/net.rs` |
| Reencontro por servidor | `src/media/update.rs`, `src/node/publish.rs` (`server_publisher`), `src/node/lookup.rs` (`Dialer::find_member`) |
| Chamadas de não-contatos da sala | `src/phone.rs`, `src/node/mod.rs` (`set_room_peers`) |
| Sala de voz e tela da sala na janela | `examples/gui/engine/room.rs`, `engine/screen.rs` |
| Aba | `examples/gui/servers.rs` |
| Testes | `tests/server.rs` (motor), `tests/node_servers.rs` (rede: 3 nós, voz entre não-contatos, reencontro pela âncora) |

## Decisões tomadas

- Qualquer membro cria salas e convida pessoas dos seus contatos; basta o aviso de IP.
- O `ServerSecret` fica em `~/.clam-gui.servers`, modo `0600`.
- Limites: 50 membros por servidor e 8 pessoas por sala.
- Fica para depois, se pedido: texto nas salas e salas maiores com um membro que retransmite.
