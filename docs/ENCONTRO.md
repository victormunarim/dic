# Proposta: reencontrar contatos sem diretório

Estado: **as camadas 1 (fila de saída) e 2 (ponto de encontro na DHT, sem privacidade extra) e o histórico de endereços estão implementados** (fases 1 e 2, veja [GUI.md](GUI.md#mudou-de-endereço-sem-diretório)). A camada 3 continua só como proposta. O código de contato direto e os endereços guardados (camada 0) vieram antes ([GUI.md](GUI.md#totalmente-p2p-o-código-de-contato-direto)).

## Problema

Sem diretório, quem liga só chega ao contato pelo relay que está escrito no código dele ou guardado de uma chamada anterior. Se o IP de alguém muda, o endereço que os contatos têm morre e ninguém avisa ninguém. Com o diretório isso se resolve sozinho; sem ele, o código antigo vira lixo e é preciso trocar códigos de novo, por fora do programa. Isso não escala (cerca de N×(N−1) trocas para N pessoas) e quebra de vez quando os dois lados mudam de endereço enquanto estão separados.

## Objetivos e fora de escopo

Objetivos:

1. Depois da troca inicial de códigos, mudar de IP não exige trocar código de novo.
2. Nenhum servidor que o projeto precise operar nem em que seja preciso confiar.
3. Quem não é contato não descobre nem lê o endereço de ninguém.
4. Cada camada é opcional e pode falhar sem derrubar as outras.

Fora de escopo: salas sem diretório, furo de NAT sem diretório e quem está atrás de CGNAT sem porta alcançável (nenhuma camada daqui cria alcance que não existe).

## Camadas

Quem liga tenta nesta ordem e para na primeira que completar o circuito:

| # | Camada | Precisa de | Cobre |
|---|--------|------------|-------|
| 0 | Endereço guardado e dica (já existe) | nada novo | IP que não mudou |
| 1 | Fila de saída com registro assinado | contato alcançável no endereço antigo | só um lado mudou |
| 2 | Ponto de encontro na DHT | acesso à DHT, os dois online em algum momento | os dois mudaram |
| 3 | Réplicas em terceiros e âncoras | nós dispostos a guardar | DHT inacessível ou bloqueada |
| 4 | Diretório (já existe) | servidor | tudo, se houver um |

As camadas 1 e 2 são o núcleo da proposta. A 3 só entra se as duas primeiras não bastarem na prática.

## Registro de endereço

Todas as camadas carregam o mesmo conteúdo, o **registro de endereço**:

```
versão (1 B) ‖ seq (8 B) ‖ tamanho do addr (1 B) ‖ addr ‖ onion (32 B) ‖ link (32 B)
```

- `seq` só cresce (hora em ms na criação, nunca menor que o último usado, guardado em disco). Quem recebe descarta registro com `seq` menor ou igual ao que já tem; isso barra replay de registro velho.
- `addr` é o endereço publicado como hoje (IP ou nome, com porta).
- Tamanho total abaixo de 150 B, mais nonce e etiqueta da cifra.

Cifra: ChaCha20-Poly1305 (já em `Cargo.toml`) com chave de HKDF-SHA256 (também já em `Cargo.toml`):

```
shared = X25519(minha_secreta, pública_do_contato)
chave  = HKDF(shared, info = "clam/encontro/cifra/v1" ‖ pública_do_dono)
```

A chave é **por sentido** (inclui a chave pública de quem é o dono do endereço), então o registro de A para B e o de B para A usam chaves diferentes. Só A e B calculam `shared`; quem tem apenas as duas chaves públicas não calcula nada. A cifra é autenticada: um terceiro não forja nem adultera registro.

Limite assumido: o outro lado do par também consegue gerar um registro "de A" com a chave de A para B. Isso só prejudica quem usaria o registro (B), então não é tratado como ataque.

## Camada 1: fila de saída

Quando o endereço publicado muda (porta aberta por UPnP, rede nova, nome que passa a apontar para outro IP) e na partida, o nó cria um registro novo e o põe numa fila por contato.

- A fila tenta entregar o registro a cada contato no último endereço conhecido dele (e nos antigos), com espera que dobra a cada falha (30 s, 1 min, 2 min, ... até 2 h) e descarta a entrada após 3 dias, quando o contato confirma ou quando ele sai da lista.
- A entrega é uma mensagem curta e silenciosa pelo circuito até o relay do contato, que não toca o telefone. O contato confere a cifra e o `seq` e troca o endereço guardado. Responde com confirmação.
- Se o lado que quer ligar tem um pedido pendente, ao receber o registro disca na hora.
- Cobre o caso comum, em que só um lado mudou de endereço.

**Como ficou.** A atualização é um tipo de quadro novo (tag 4, e o ack é a tag 5), tratado só pelo telefone: o relay de saída não muda, porque o quadro vai pelo mesmo circuito do convite. Formato em `src/media/update.rs`: `tag ‖ nonce (12 B) ‖ AEAD(registro) ‖ enchimento aleatório`, sempre 200 B. O quadro não diz de quem é; o telefone tenta a chave de cada contato (uma troca X25519 por contato) e descarta em silêncio o que nenhuma abre. O registro tem endereço de até 60 B, preenchido com zeros até tamanho fixo. A confirmação cifra o `seq` com a chave do sentido contrário. Quem confirma prova que tem a chave do par, então o endereço que respondeu vira o guardado (`Dialer::remember`). O aviso recebido põe o endereço antigo no histórico (até 4 por contato) e retira a dica de convite do contato, que não era autenticada.

Não feito: discar na hora ao receber o registro quando há um pedido de chamada pendente (a próxima ligação já usa o endereço novo). Fora do que a proposta previa: o `seq` e a fila vivem só na memória (a fila refaz tudo ao reabrir o programa, com `seq` novo = hora em ms), e o aviso é ligado por padrão só quando não há diretório (`NodeConfig::updates`).

## Camada 2: ponto de encontro na DHT

Os dois lados do par se encontram por um depósito numa DHT pública sem que nenhum servidor do projeto exista. A DHT proposta é a Mainline do BitTorrent, que tem milhões de nós e itens mutáveis assinados (BEP 44).

Cada nó publica, para cada contato, **um item por sentido**:

```
sk, pk = ed25519 derivado de HKDF(shared, "clam/encontro/assina/v1" ‖ pública_do_dono ‖ período)
sal    = HKDF(shared, "clam/encontro/sal/v1" ‖ pública_do_dono ‖ período)
item   = cifrado(registro de endereço), assinado por sk, no alvo (pk, sal)
```

- `período` é o dia (`unix / 86400`). A chave de busca muda sozinha todo dia e o par não fica ligável de um dia para o outro.
- Os dois lados derivam o mesmo `sk` para o mesmo sentido, então o leitor sabe onde procurar sem combinar nada além do que a troca de códigos já deu.
- Um sentido por item evita que A e B sobrescrevam o item um do outro.
- Quem escreve republica a cada hora enquanto estiver online. Itens da Mainline somem em cerca de 2 h sem renovação; **o depósito só dura enquanto o dono está online.** Isso basta para o objetivo (substituir "estar no mesmo lugar" por "estar online ao mesmo tempo") mas não vale como correio para quem vai ficar dias fora.
- O leitor consulta os contatos em ordem, com o período atual, o anterior e o seguinte, porque o relógio pode estar errado (veja abaixo).
- Limite: até 32 contatos por rodada, como na consulta ao diretório de hoje.

**Como ficou.** O cliente é o crate `mainline` (BEP 44), em `src/rendezvous.rs` (`Rendezvous`, `DhtOptions`). A derivação do alvo e a cifra do valor estão em `src/media/update.rs` (`rendezvous`, `seal_record`, `open_record`): o valor é `nonce ‖ AEAD(registro)` com 162 B, o mesmo registro da camada 1 com outro dado associado, e a semente ed25519 e o sal saem do HKDF como na proposta. Em `src/node.rs`:

- `NodeConfig::dht` (`None` por padrão) liga a camada. O registro do endereço atual (`seq` = hora em ms) é um só para a fila de saída e para a DHT, então o contato vê o mesmo `seq` por qualquer caminho.
- A tarefa de gravação escreve o item de cada contato (até 32) ao subir, quando o endereço muda, quando entra contato novo, quando o dia vira e a cada hora; uma gravação que falha é refeita em 1 min. Cada gravação leva um `seq` de DHT novo (hora em ms), porque a DHT recusa o mesmo `seq` com outro valor e cada cifra sai com nonce novo.
- A leitura é sob demanda, só ao discar (não em segundo plano para todos): depois dos endereços guardados e antes do diretório, `Dialer` consulta o dia anterior, o atual e o seguinte ao mesmo tempo, fica com o registro de maior `seq`, aplica como um aviso da camada 1 (o endereço antigo vai para o histórico) e tenta o relay. Vale também para compartilhar a tela.
- Os testes usam `mainline::Testnet` em loopback (`tests/node.rs`: `both_contacts_changed_address_and_find_each_other_through_the_dht`: os dois começam com o endereço velho um do outro, sem fila de saída, e se acham pela DHT).
- Na GUI é a caixa "Usar a DHT (BitTorrent) para reencontrar contatos", desligada por padrão.

## Camada 3: réplicas e âncoras (se for preciso)

- **Âncora:** um contato com endereço estável (IP fixo ou DNS) que concorda em guardar registros dos outros. O dono envia o registro a ela com a mesma mensagem da camada 1; quem quer ligar a consulta. É o mesmo papel do diretório, mas voluntário e federado.
- **Réplicas em terceiros:** o registro cifrado é enviado a k nós online quaisquer. Eles não leem o conteúdo; guardam por poucos dias.

Só entra se a DHT estiver bloqueada ou for pouco confiável para o público do projeto.

## Rede e privacidade

- **Conteúdo:** protegido pela cifra (acima).
- **Quem fala com quem:** os nós da DHT que guardam um item veem o IP de quem escreve e o de quem lê. Quem controla vários nós próximos do alvo pode ligar os dois IPs e concluir que são contatos. A chave de busca muda por dia, o que limita a janela, mas não impede.
- **Mitigação desejada:** fazer as consultas e publicações da DHT saírem por um circuito de 2 saltos. Isso exige relays que aceitem encaminhar tráfego da DHT para a saída, o que o relay atual (que só encaminha para o telefone do destino) não faz. Fica como fase própria e opção na GUI ("consultar a DHT por circuito"). Até lá, a camada 2 expõe o IP a quem opera nós da DHT.
- **Disponibilidade:** nós maliciosos perto do alvo podem esconder ou descartar o item (ataque de eclipse). Eles não leem nem forjam. Mitigação: replicação da própria DHT, período anterior e seguinte, e as outras camadas como alternativa.
- **Sem sigilo futuro:** vazar a chave secreta de uma identidade revela registros antigos que alguém tenha gravado. O dano é limitado a endereços antigos.
- **Relógio:** hoje a hora certa vem do diretório (`src/clock.rs`). Sem ele, o relógio local pode errar. A tolerância de um período para cada lado cobre desvio de até algumas horas; além disso a camada 2 falha e a camada 1 (que usa `seq`, não relógio) continua valendo.

## Onde isso muda o código

- `src/node.rs`: a ordem de tentativa em `Dialer::dial` ganha as camadas 1 e 2 entre as dicas e o diretório; `Node` ganha uma tarefa de fundo (como a de `refresh_known`) que publica e lê os itens e roda a fila de saída. `NodeConfig` ganha chaves para ligar cada camada.
- Módulo novo, por exemplo `src/encontro/`: formato do registro, derivação de chaves, fila de saída e cliente da DHT.
- `src/phone.rs` ou o relay: o quadro de atualização silenciosa da camada 1.
- `examples/gui`: arquivo de estado da fila (junto de `.clam-gui.known`), opção para ligar/desligar a DHT e linha de log por camada que atendeu a ligação.
- `docs/GUI.md` e `docs/CHAMADAS.md`: passam a descrever as camadas quando existirem.

Nenhuma dependência criptográfica nova: `chacha20poly1305`, `hkdf`, `sha2` e `ed25519-dalek` já estão no projeto. A única dependência nova é o cliente da DHT.

## Plano em fases

1. **Camada 1** com memória de endereços antigos por contato (guardar os últimos N, não só um). Testes em loopback: A muda de endereço com B offline, B volta, recebe e liga. **Pronta** (`tests/node.rs`: `a_new_address_reaches_the_contact_that_was_offline`, `an_old_address_in_the_history_still_reaches_the_contact`, `address_updates_from_strangers_are_ignored`).
2. **Camada 2** sem privacidade extra, com a DHT local nos testes. Teste: os dois mudam de endereço e se encontram. **Pronta** (`tests/node.rs`: `both_contacts_changed_address_and_find_each_other_through_the_dht`).
3. **Privacidade da camada 2:** consultas pelo circuito.
4. **Camada 3**, só se a prática mostrar que precisa.

## Perguntas em aberto

- A Mainline é aceitável como dependência de rede (ela depende de nós iniciais fixos do ecossistema BitTorrent) ou o projeto prefere uma DHT própria?
- ~~O quadro de atualização silenciosa é um convite marcado ou um tipo novo no relay?~~ Tipo novo de quadro, só no telefone (veja a camada 1).
- Quanto tempo a fila de saída deve insistir (hoje 3 dias, até 2 h entre tentativas), e o usuário vê esse estado na GUI? Hoje só o log mostra o aviso recebido; o estado da fila não aparece.
- ~~O leitor deve consultar todos os contatos de uma vez ou só quando for ligar a um deles?~~ Só ao ligar (veja a camada 2). O custo é a demora da primeira chamada a quem mudou de endereço (até 10 s de consulta).
