# Reencontrar contatos sem servidor

Desenho e estado das camadas que acham um contato cujo endereço mudou. A visão geral da comunicação está em [COMUNICACAO.md](COMUNICACAO.md).

Estado: **as camadas 1 (fila de saída), 2 (ponto de encontro na DHT) e 3 (âncoras), o histórico de endereços, a privacidade da camada 2 (consultas pelo circuito) e o contato de duas vias (apresentação) estão implementados** (fases 1 a 4, veja [GUI.md](GUI.md#mudou-de-endereço)). Réplicas em terceiros quaisquer, a outra metade da proposta da camada 3, continuam só como ideia. O código de contato direto e os endereços guardados (camada 0) vieram antes ([GUI.md](GUI.md#o-código-de-contato-direto)).

## Problema

Quem liga só chega ao contato pelo relay que está escrito no código dele ou guardado de uma chamada anterior. Se o IP de alguém muda, o endereço que os contatos têm morre e ninguém avisa ninguém. Sem nada que reencontre o contato, o código antigo vira lixo e é preciso trocar códigos de novo, por fora do programa. Isso não escala (cerca de N×(N−1) trocas para N pessoas) e quebra de vez quando os dois lados mudam de endereço enquanto estão separados.

## Objetivos e fora de escopo

Objetivos:

1. Depois da troca inicial de códigos, mudar de IP não exige trocar código de novo.
2. Nenhum servidor que o projeto precise operar nem em que seja preciso confiar.
3. Quem não é contato não descobre nem lê o endereço de ninguém.
4. Cada camada é opcional e pode falhar sem derrubar as outras.

Fora de escopo: salas, furo de NAT e quem está atrás de CGNAT sem porta alcançável (nenhuma camada daqui cria alcance que não existe).

## Camadas

Quem liga tenta nesta ordem e para na primeira que completar o circuito:

| # | Camada | Precisa de | Cobre |
|---|--------|------------|-------|
| 0 | Endereço guardado e dica (já existe) | nada novo | IP que não mudou |
| 1 | Fila de saída com registro assinado | contato alcançável no endereço antigo | só um lado mudou |
| 2 | Ponto de encontro na DHT | acesso à DHT, os dois online em algum momento | os dois mudaram |
| 3 | Âncoras (réplicas em terceiros quaisquer ficaram de fora) | nós dispostos a guardar | DHT inacessível ou bloqueada |

As três existem. A camada 0 e a 1 não dependem de ninguém; a 2 depende da Mainline; a 3, de uma âncora que a pessoa escolhe. Na GUI a DHT e o serviço de âncora vêm ligados de início, e cada camada pode falhar sem derrubar as outras.

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

Não feito: discar na hora ao receber o registro quando há um pedido de chamada pendente (a próxima ligação já usa o endereço novo). Fora do que a proposta previa: o `seq` e a fila vivem só na memória (a fila refaz tudo ao reabrir o programa, com `seq` novo = hora em ms), e o aviso é ligado por padrão (`NodeConfig::updates`).

## Camada 2: ponto de encontro na DHT

Os dois lados do par se encontram por um depósito numa DHT pública sem nenhum servidor. A DHT proposta é a Mainline do BitTorrent, que tem milhões de nós e itens mutáveis assinados (BEP 44).

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
- Limite: até 32 contatos por rodada, como no resto do nó.

**Como ficou.** O cliente é o crate `mainline` (BEP 44), em `src/rendezvous.rs` (`Rendezvous`, `DhtOptions`). A derivação do alvo e a cifra do valor estão em `src/media/update.rs` (`rendezvous`, `seal_record`, `open_record`): o valor é `nonce ‖ AEAD(registro)` com 162 B, o mesmo registro da camada 1 com outro dado associado, e a semente ed25519 e o sal saem do HKDF como na proposta. Em `src/node/`:

- `NodeConfig::dht` liga a camada (`None` por padrão na biblioteca; a GUI a liga de início). O registro do endereço atual (`seq` = hora em ms) é um só para a fila de saída e para a DHT, então o contato vê o mesmo `seq` por qualquer caminho.
- A tarefa de gravação escreve o item de cada contato (até 32) ao subir, quando o endereço muda, quando entra contato novo, quando o dia vira e a cada hora; uma gravação que falha é refeita em 1 min. Cada gravação leva um `seq` de DHT novo (hora em ms), porque a DHT recusa o mesmo `seq` com outro valor e cada cifra sai com nonce novo.
- A leitura é sob demanda, só ao discar (não em segundo plano para todos): depois dos endereços guardados, `Dialer` consulta o dia anterior, o atual e o seguinte ao mesmo tempo, fica com o registro de maior `seq`, aplica como um aviso da camada 1 (o endereço antigo vai para o histórico) e tenta o relay. Vale também para compartilhar a tela.
- Os testes usam `mainline::Testnet` em loopback (`tests/node.rs`: `both_contacts_changed_address_and_find_each_other_through_the_dht`: os dois começam com o endereço velho um do outro, sem fila de saída, e se acham pela DHT).
- Na GUI é a caixa "Não usar a DHT (BitTorrent) para reencontrar contatos" (aba Configurações), desmarcada de início, ou seja, a DHT vem ligada; marcá-la grava `no_dht=1`.

### Consultas pelo circuito (fase 3)

O relay de saída atual só abre fluxo para o telefone do destino e a DHT fala UDP, então a saída não podia só repassar bytes. Em vez de um proxy UDP aberto (que serviria de refletor e de varredor de portas), a saída faz **ela mesma** a consulta, como *gateway* (`src/rendezvous.rs`):

- **Quem serve.** Todo nó com `NodeConfig::dht` liga, além do cliente, um ouvinte em loopback que a política de saída do relay mapeia para o destino `dht:9` (`GATEWAY_DEST`). Só entende dois pedidos: ler o alvo `(chave, sal)` e gravar um item **já assinado** por quem pediu. A semente de assinatura não sai do nó de origem (ele assina antes), então o gateway não forja item; o valor é cifrado para o contato, então ele não o lê. Limites: pedido de até 400 B, 4 pedidos ao mesmo tempo e 60 por minuto somando todos os circuitos.
- **Formato.** Cada célula do circuito leva até 203 B, então pedido e resposta são `u16 BE tamanho ‖ corpo` em pedaços de 200 B (o gravar tem 299 B e cabe em dois). Leitura: `1 ‖ chave ‖ sal`; gravação: `2 ‖ chave ‖ assinatura ‖ seq ‖ sal ‖ valor`; respostas `1 ‖ valor`, `1` ou `0`.
- **Quem pede.** `NodeConfig::dht` com `circuit: true`: o `Dialer` monta um circuito de dois relays que não são o dele, sorteados entre os relays guardados dos contatos (`known`, `hints`, `history`, sem o do contato procurado); tenta até 3 saídas, cada uma com 8 s para ficar pronta, e faz os três dias de uma consulta, ou todas as gravações de uma rodada, na mesma sessão. O primeiro salto vê o IP e não o pedido; a saída vê o pedido (um alvo que muda por dia e não liga a ninguém sem as chaves do par) e não o IP; a DHT só vê o IP da saída.
- **Falha fechada.** Com menos de dois relays utilizáveis, ou sem saída que sirva de gateway, a DHT não é consultada nem gravada, e nunca cai para o acesso direto: o IP não vaza por causa da falha. A ligação segue para as outras fontes.
- **O que não muda.** Quem controla o primeiro salto e a saída ao mesmo tempo liga o IP ao alvo (o mesmo limite dos circuitos de chamada). Os relays disponíveis são os dos contatos, então o circuito só esconde o seu IP de um contato do outro (o contato da saída não vê o seu IP; o do primeiro salto vê, mas não o pedido). Um nó que serve de gateway aparece na DHT fazendo consultas que não são só dele, o que também mistura o tráfego dele.
- **Testes** (`tests/node.rs`): `the_dht_is_reached_through_two_contact_relays` (A e B sem acesso direto à DHT, só C e D têm; B acha A) e `circuit_mode_never_falls_back_to_the_direct_dht` (com um só relay de contato a DHT não é usada, e o controle direto acha o item).

## Camada 3: âncoras (fase 4)

Para quem a DHT está bloqueada ou não basta, um nó com endereço estável (IP fixo ou DNS) pode guardar os registros dos outros: a *âncora* (`NodeConfig::anchor`, na GUI a caixa "Não servir de âncora…", desmarcada de início). É um depósito voluntário e federado: cada pessoa escolhe em quais âncoras confia para estar no ar (`NodeConfig::anchors`, na GUI "Âncoras que você usa", uma por linha, `ip:porta#chave-do-circuito#chave-do-link`), e quem opera uma não precisa ser ninguém especial.

- **Mesmo item, outro depósito.** O dono grava em cada âncora que usa o mesmo item assinado que grava na DHT (mesmo alvo, mesma cifra: só o contato abre) e as renova a cada hora junto com a DHT (`rendezvous_publisher`). Quem quer ligar lê os mesmos três dias (anterior, atual e seguinte) em todas as âncoras e fica com o registro de maior `seq`. As várias âncoras são as réplicas: basta uma estar no ar. A ordem ao ligar passa a ser: endereço guardado, DHT e âncoras (ao mesmo tempo).
- **Protocolo.** O mesmo do gateway da DHT (`rendezvous::Remote`), no destino `anchor:9` da política de saída do relay, por um circuito até o relay da âncora. A âncora implementa `Store` com um depósito em memória (`Anchor`) em vez da DHT; o gateway e a âncora usam o mesmo código de atendimento.
- **O que a âncora aceita.** Só item de tamanho de registro cifrado, com assinatura que confere com a chave do alvo (`Signed::verify`, a mesma conta da DHT), e só substitui por `seq` maior. Guarda até 2048 itens por 3 dias sem gravação nova; passando disso sai o expirado ou, na falta, o mais antigo. Atende 600 pedidos por minuto, somados. Não lê o conteúdo (a cifra é do par) nem forja item (não tem a semente).
- **Limites.** A âncora é um depósito aberto e limitado: quem a enche de itens falsos assinados com chaves descartáveis pode empurrar os verdadeiros para fora, e o dono repõe o seu a cada hora, então o efeito dura enquanto durar o ataque. Quem opera a âncora vê o IP de quem pergunta e grava quando o primeiro salto é o relay do próprio usuário (sem relays de contatos guardados, o primeiro salto é ele por loopback, como numa chamada); com relays de contatos o circuito passa por eles e a âncora só vê o IP do último. Ela também vê quantos itens existem e a frequência de renovação, não a quem pertencem.
- **Testes:** unitários em `rendezvous` (assinatura, só `seq` maior, tamanho, expulsão do mais antigo) e `tests/node.rs::an_anchor_lets_two_moved_contacts_find_each_other` (sem DHT, B acha A só pela âncora R).

Réplicas em terceiros quaisquer (enviar o registro a k nós online sem combinar) ficam de fora: sem uma lista combinada de quem guarda, o leitor não sabe onde procurar. A âncora resolve isso pedindo que a pessoa a escolha.

## Contato de duas vias: a apresentação (fase 4)

Quem adiciona um contato pelo código só tem o código do outro; o outro lado não sabe de nada e precisaria colar o código de volta. Para os dois terem o contato na tela, quem adiciona manda uma *apresentação* (`Node::introduce`; a GUI chama ao "Adicionar contato"):

- **Quadro.** Um quadro novo de 200 bytes, só no telefone (etiqueta 7, `media::update::seal_intro`): a chave pública de quem manda em claro (o destinatário precisa dela para derivar a chave do par) e o resto cifrado com a chave do par, com o dono do quadro sendo o autor: o nome (até 32 bytes), o `seq` e como chegar nele, que é o relay dele (endereço, chave do circuito e do link). Quem não tem a chave do destinatário não abre; quem tem a chave de outra pessoa não consegue forjar um quadro em nome dela, pois a cifra do par só abre com as duas chaves. O destinatário confirma com a mesma resposta cifrada da atualização de endereço (etiqueta 5), com o `seq` repetido.
- **Quem pode.** Qualquer um que saiba a chave e o relay do destinatário pode se apresentar, não só contatos: foi justamente o que quem adiciona fez sem o outro saber. Por isso o destinatário tem o interruptor "Aceitar quem me adiciona como contato" (`NodeConfig::introductions`, ligado de início); desligado, o quadro é descartado sem resposta como o de qualquer estranho. Ligado, aceita no máximo 20 apresentações por minuto, somadas. O nó não adiciona sozinho: entrega a apresentação ao app (`Node::take_introductions`), que decide. A GUI adiciona direto, escreve o nome que a pessoa deu (limpo de caracteres de controle, `;`, `#` e `@`) e avisa no registro de atividade.
- **Entrega.** A fila é a da camada 1: tenta pelo endereço que se tem do outro (o do código, o guardado, a DHT e as âncoras, nessa ordem), com espera que dobra, e desiste em 3 dias ou quando o contato é removido (`Node::forget`). As entregas pendentes ficam em `~/.clam-gui.intro` e voltam no próximo início.
- **O que não acompanha.** Remover um contato de um lado não remove do outro, e trocar o nome não é reenviado; a apresentação é só a primeira vez.
- **Testes** (`tests/node.rs`): `adding_a_contact_introduces_us_to_it` (B, que não tinha A, recebe, adiciona e os dois se ligam), `the_introduction_waits_for_the_contact_to_come_online`, `introductions_are_ignored_when_switched_off`, `forgetting_a_contact_cancels_its_introduction`; unitários em `media::update` (autoria, sigilo do nome, nome cortado, ack).

## Rede e privacidade

- **Conteúdo:** protegido pela cifra (acima).
- **Quem fala com quem:** os nós da DHT que guardam um item veem o IP de quem escreve e o de quem lê. Quem controla vários nós próximos do alvo pode ligar os dois IPs e concluir que são contatos. A chave de busca muda por dia, o que limita a janela, mas não impede.
- **Âncoras (fase 4):** quem opera uma vê o IP de quem pergunta quando o primeiro salto é o relay do próprio usuário (veja a camada 3). A âncora que o usuário escolhe é uma confiança que ele dá.
- **Mitigação (fase 3, opcional):** as consultas e gravações da DHT saem por um circuito de 2 saltos (`DhtOptions::circuit`, na GUI "Falar com a DHT pelos relays dos contatos"). Sem essa opção, a camada 2 expõe o IP a quem opera nós da DHT. Veja "Consultas pelo circuito" abaixo.
- **Disponibilidade:** nós maliciosos perto do alvo podem esconder ou descartar o item (ataque de eclipse). Eles não leem nem forjam. Mitigação: replicação da própria DHT, período anterior e seguinte, e as outras camadas como alternativa.
- **Sem sigilo futuro:** vazar a chave secreta de uma identidade revela registros antigos que alguém tenha gravado. O dano é limitado a endereços antigos.
- **Relógio:** a hora vem do relógio local, que pode errar. A tolerância de um período para cada lado cobre desvio de até algumas horas; além disso a camada 2 falha e a camada 1 (que usa `seq`, não relógio) continua valendo.

## Onde está no código

- `src/node/`: `dialer.rs` (`Dialer::reachable_client`: a ordem de tentativa, guardado, dica e histórico; depois DHT e âncoras), `deliver.rs` (`deliver_update`, `deliver_intro`), `lookup.rs` (consulta à DHT e às âncoras), `publish.rs` (a fila de saída e as tarefas que gravam o registro na DHT e nas âncoras) e `reach.rs` (vigia o IP e o registro do endereço atual).
- `src/rendezvous.rs`: cliente da DHT (`Rendezvous`), o gateway (`dht:9`), a âncora (`anchor:9`) e o `Remote` que fala com os dois pelo circuito.
- `src/media/update.rs`: os quadros de 200 B (aviso de endereço, confirmação, apresentação) e a cifra do registro.
- `src/phone.rs`: o telefone confere e responde esses quadros sem tocar.
- `examples/gui`: o estado em arquivos (`.clam-gui.known`, `.clam-gui.intro`), as caixas da aba Configurações e as linhas da aba Atividade.

Nenhuma dependência criptográfica nova: `chacha20poly1305`, `hkdf`, `sha2` e `ed25519-dalek` já estão no projeto. A única dependência nova é o cliente da DHT (`mainline`).

## Plano em fases

1. **Camada 1** com memória de endereços antigos por contato (guardar os últimos N, não só um). Testes em loopback: A muda de endereço com B offline, B volta, recebe e liga. **Pronta** (`tests/node.rs`: `a_new_address_reaches_the_contact_that_was_offline`, `an_old_address_in_the_history_still_reaches_the_contact`, `address_updates_from_strangers_are_ignored`).
2. **Camada 2** sem privacidade extra, com a DHT local nos testes. Teste: os dois mudam de endereço e se encontram. **Pronta** (`tests/node.rs`: `both_contacts_changed_address_and_find_each_other_through_the_dht`).
3. **Privacidade da camada 2:** consultas pelo circuito. **Pronta** (`tests/node.rs`: `the_dht_is_reached_through_two_contact_relays`, `circuit_mode_never_falls_back_to_the_direct_dht`).
4. **Camada 3 (âncoras) e contato de duas vias.** **Pronta** (`tests/node.rs`: `an_anchor_lets_two_moved_contacts_find_each_other`, `adding_a_contact_introduces_us_to_it`, `the_introduction_waits_for_the_contact_to_come_online`, `introductions_are_ignored_when_switched_off`, `forgetting_a_contact_cancels_its_introduction`).

## Perguntas em aberto

- A Mainline é aceitável como dependência de rede (ela depende de nós iniciais fixos do ecossistema BitTorrent) ou o projeto prefere uma DHT própria?
- ~~O quadro de atualização silenciosa é um convite marcado ou um tipo novo no relay?~~ Tipo novo de quadro, só no telefone (veja a camada 1).
- Quanto tempo a fila de saída deve insistir (hoje 3 dias, até 2 h entre tentativas), e o usuário vê esse estado na GUI? Hoje só o log mostra o aviso recebido; o estado da fila não aparece.
- ~~O leitor deve consultar todos os contatos de uma vez ou só quando for ligar a um deles?~~ Só ao ligar (veja a camada 2). O custo é a demora da primeira chamada a quem mudou de endereço (até 10 s de consulta).
