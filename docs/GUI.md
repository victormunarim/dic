# Interface gráfica: sala, membros e modo sem diretório

Uso da GUI (`examples/gui`): quem fala, volume, sensibilidade do microfone, apelidos, dispositivos, remover membro e o que funciona sem diretório.

[← Voltar ao README](../README.md)

## Interface da sala: quem fala, volume, apelidos e dispositivos

- **Quem fala.** Cada membro da sala tem uma bolinha: verde e maior enquanto fala (energia do áudio recebido, com 400 ms de retenção), cinza quando conectado e quieto, laranja enquanto a ligação não completou. Na API: `Mesh::is_speaking`.
- **Volume por pessoa.** Um controle de 0 (mudo) a 200 % por membro; vale só para o que você ouve (`Mesh::set_volume`, antes do limitador da mixagem).
- **Apelidos.** O campo ao lado de cada membro grava um apelido salvo no `~/.clam-gui.conf` (`nicks=`, só neste computador). Contatos continuam com o nome da lista; o apelido nunca torna alguém "contato" (atendimento automático segue só a lista de contatos).
- **Controles de áudio.** Com o áudio ligado e o nó online, o cartão "Áudio" tem mudo (envia silêncio), volume do alto-falante (0 a 200 %), ganho do microfone (0 a 300 %), um medidor do microfone (vale mesmo com mudo, para confirmar que ele capta) e "Testar o microfone", que devolve a sua voz ao alto-falante (use fones). Durante o teste a linha fica ocupada.
- **Sensibilidade e tratamento da voz.** O medidor traz uma linha laranja: o limiar de sensibilidade (-60 a -20 dBFS, padrão -45). Quando a voz passa da linha o medidor diz "Microfone: voz". Quanto mais à esquerda a linha, mais sensível. Em volta dela:
  - **Remover ruído (rede neural)** (ligado por padrão): RNNoise, pelo crate `nnnoiseless` (Rust puro). Tira teclado, ventilador, estática e vozes ao fundo, o que filtro por volume não separa. O áudio sobe de 16 para 48 kHz, passa pela rede em quadros de 10 ms e volta; custa ~10 ms de atraso e pouca CPU. A rede também diz, a cada 10 ms, a probabilidade de ser voz: com ela ligada, a porta e o indicador "voz" só contam trechos acima do limiar **e** com probabilidade ≥ 0,5. Nos primeiros ~100 ms de uma chamada a rede ainda se calibra e pode tomar ruído por voz.
  - **Silenciar o que não é voz** (porta, desligada por padrão): abaixo do limiar (ou fora do que a rede reconhece como voz) o som enviado cai pela "Redução do ruído" (6 a 60 dB, padrão 40). A porta abre em 2 ms, segura 250 ms depois da última palavra e fecha em ~60 ms, para não cortar o fim das frases.
  - **Cancelar eco do alto-falante** (ligado por padrão): o AEC3 do WebRTC (`sonora`) usa o que o alto-falante tocou como referência e tira do microfone o que voltou por ele. A janela mostra "Eco cancelado: N dB" na chamada (energia do microfone antes e depois do cancelador, enquanto o outro lado fala). Leva alguns segundos de conversa para aprender a sala. A referência é o que de fato saiu pelo alto-falante, amostra a amostra e inclusive o silêncio, entregue ao cancelador assim que chega e nunca completada com zeros: antes ela só tinha as partes com voz e blocos grandes da placa a entregavam em rajadas, e o cancelador perdia o atraso entre o que tocou e o que o microfone ouviu (no alto-falante do notebook, ~0–3 dB; agora ~14 dB, com o atraso achado em ~25 ms).
  - **Abaixar o microfone enquanto só há eco** (ligado por padrão): reforço para o que o cancelador deixa passar. Com o outro lado tocando acima de -50 dBFS (e até ~160 ms depois), se o microfone está mais de 20 dB abaixo do que sai pelo alto-falante o som cai 18 dB (desce em 3 ms, volta em 40 ms) e o indicador "voz" não acende. Quem fala bem baixinho por cima do outro pode ser cortado; com fones, desligue.
  - **Cortar graves** (ligado por padrão): filtro passa-altas de 12 dB por oitava em 100 Hz; tira ventilador, estrondo de mesa e sopro.
  - **Volume automático** (ligado por padrão): mira ~-22 dBFS (reforço até +24 dB, corte até -12 dB); desce rápido e sobe devagar, só aprende com voz e nunca reforça o ruído de fundo. Um limitador suave impede estouro. Ele substitui o ganho automático do WebRTC (`sonora`) que o `clam::audio` aplica por padrão: a GUI o desliga (`Audio::set_processing`), junto com o filtro de graves de lá, para nada ser tratado duas vezes; com "Remover ruído" ligado desliga também a supressão de ruído do WebRTC. O cancelamento de eco do WebRTC continua sempre ligado.
  - **Restaurar padrões do áudio** volta tudo ao padrão.

  O microfone chega já pelo WebRTC (só eco; o resto foi desligado de lá, ver `Audio::set_processing`). A ordem é filtro de graves → medidor (e volume por trecho de 5 ms) → RNNoise → porta → corte do eco residual → ganho do microfone × volume automático → limitador (`examples/gui/engine/voice.rs`). Os ajustes (e o volume e o ganho) ficam salvos no `~/.clam-gui.conf` (`volume`, `mic_gain`, `high_pass`, `denoise`, `echo_cancel`, `echo_suppress`, `gate`, `gate_db`, `gate_depth_db`, `agc`), gravados ao soltar o controle; valores fora da faixa ou ilegíveis voltam ao limite ou ao padrão.
- **Dispositivos de áudio.** Em Configurações avançadas, escolha microfone e alto-falante (`clam::audio::devices`, `Audio::open_with`); vazio usa o padrão do sistema. Se o dispositivo escolhido sumiu (fone desconectado), a abertura falha com aviso em vez de trocar de dispositivo em silêncio. No Linux o `cpal` usa o PulseAudio/PipeWire quando o servidor está rodando (feature `pulseaudio`), e a janela mostra "Sistema de áudio: PulseAudio" sob as listas (se disser ALSA, o servidor não foi achado e é a lista crua da placa); a lista traz os nomes que o sistema mostra ("… Speaker", "… Digital Microphone"), sem as fontes "Monitor of …" (que gravam o que toca, não são microfone); sem servidor, cai no ALSA, onde a lista tira repetidos e os falsos "Discard all samples" e "Default ALSA Output…". Um nome guardado do tempo do ALSA (por exemplo `sof-hda-dsp`) deixa de existir na lista: escolha o dispositivo de novo. Os fluxos pedem blocos de 10 ms à placa (se ela recusar, usam o tamanho dela).

## Remover membro da sala (GUI)

- Quem cria a sala é o **dono**; o código de convite agora é `segredo:dono[@host:porta]` e a sala fica salva com o dono (`nome segredo:dono`). Códigos antigos (só o segredo) continuam entrando, mas sem dono ninguém remove.
- O dono vê **Remover** ao lado de cada membro, com confirmação ("Sim, remover" / "Cancelar"). Remover troca o segredo (`room::rotate`) e deixa o aviso cifrado só para quem fica; a chamada com o removido cai na hora.
- Os outros membros conferem o aviso a cada sincronização (10 s) e ao entrar (`room::follow`, até 8 trocas seguidas), passam para o segredo novo e a janela o salva no lugar do antigo. O removido fica no segredo velho, sem ver nem ser alcançado pelos que ficaram.
- Limite: um membro desconectado por mais de 3 dias perde o aviso (expira no diretório) e precisa de um convite novo.

## Sem diretório

O diretório só é indispensável no primeiro contato. Depois disso:

- **Endereço guardado.** O `Node` lembra o último relay de cada contato para quem ligou (`Node::known()`, devolvido em `NodeConfig::known`). Ao discar, se esse relay ainda aceita conexão, a chamada vai direto, sem consultar o diretório. Se não responde, cai na busca normal e atualiza o endereço. A GUI guarda a lista em `~/.clam-gui.known`. Para esse endereço valer também depois de reabrir ou recompilar o programa de quem recebe, o relay do nó mantém as mesmas chaves (circuito e link) entre execuções: `NodeConfig::relay_keys`, que a GUI guarda em `~/.clam-gui.relay` (0600). Sem isso cada início gerava chaves novas e o endereço que os contatos tinham guardado deixava de servir, obrigando a consultar o diretório na primeira ligação. A GUI regrava `~/.clam-gui.known` a cada mudança (verificação a cada 5 s) e ao sair, não só depois de uma ligação, e mostra no log qualquer erro de gravação (uma vez por erro), inclusive o do arquivo das chaves do relay.
- **Ligar de volta sem diretório.** O convite leva o relay de quem liga (endereço e chaves, cifrados junto com a identidade; telefones antigos ignoram). Quem atende guarda isso como *dica* (`Node::hints()`, devolvida em `NodeConfig::hints`; na GUI são as linhas com `dica` no fim de `~/.clam-gui.known`). Ao discar a ordem é: endereço guardado, dica, diretório. A dica não é autenticada (qualquer um que conheça a chave de um contato pode escrever o convite), por isso nunca sobrescreve o endereço guardado e só vira endereço guardado depois que uma chamada completa por ela. Sem diretório na partida, o nó informa o IP dele na rede local em vez de 127.0.0.1.
- **Subir com o diretório fora do ar.** `Node::start` não falha mais: registra o problema em `network_note()`/`directory_error()` e tenta publicar de novo a cada 40 s. Enquanto isso, recebe chamadas de quem já tem o seu endereço e liga para quem está em `known`.

### Totalmente P2P: o código de contato direto

Dá para nem ter diretório na primeira conexão. Deixe o campo do servidor vazio e fique online (a GUI não o preenche sozinha; só *Adicionar contato* copia o `@host:porta` de um código para o campo, e só se ele estava vazio): *Meu código de contato* passa a mostrar o código direto,

```
<sua chave pública>#<ip:porta do seu relay>#<chave do circuito, 64 hex>#<chave do link, 64 hex>
```

(com `@host:porta` depois da chave se houver diretório; nesse caso o código continua o curto, `chave@host:porta`). Quem recebe cola o código em *Adicionar contato* ou *Ligar para um código*: a GUI guarda o relay como dica (`Node::add_hint`, e na partida as linhas de contatos com `#` entram em `NodeConfig::hints`) e disca por ele, sem diretório. O primeiro salto é o relay de quem liga, por loopback, e o segundo é o relay do código. Depois de uma chamada completa o endereço vira guardado (`~/.clam-gui.known`) como qualquer outro. Um código com `#` cortado ou torto é recusado inteiro. `split_code` ignora a parte do relay, então os contatos antigos continuam valendo.

Contatos e endereços andam juntos: *Adicionar contato* com um código direto também guarda o relay dele em `~/.clam-gui.known` (como dica) e passa a atendê-lo na hora, e *Remover* tira o contato, o que ele tinha de endereço guardado e a permissão de ligar para você (`Node::add_contact`/`Node::forget`; com o nó parado a GUI edita o arquivo direto).

Custos: o IP e a porta (4001) de quem recebe precisam ser alcançáveis de fora (UPnP, porta aberta ou endereço público informado; sem diretório não há teste de alcance nem furo de NAT, e CGNAT não funciona); o código muda quando o IP muda; e quem recebe vê o IP de quem liga, porque o primeiro salto é o relay dele. Sem endereço público o código leva o IP da rede local, que só serve dentro dela. Se o endereço do código não responde, o erro (`DialError::NoDirectory`) diz que não há diretório para achar outro.

Endereço guardado velho não trava a ligação: se o circuito não fica pronto em 5 s, a busca no diretório assume e corrige o cache. Quando o diretório volta, `published_addr()` e `network_note()` se atualizam sozinhos.

Limites: sem diretório não há furo de NAT nem sorteio de primeiro salto (usa o próprio relay por loopback), e o endereço publicado fica sendo o local até o diretório voltar.

#### Mudou de endereço sem diretório

Sem diretório, cada contato guarda o endereço do seu relay. Quando ele muda (IP novo, outra porta), o programa avisa os contatos sozinho, e o código antigo não precisa ser trocado:

- **Aviso.** Ao subir, e sempre que o endereço mudar, o nó monta um registro (versão, `seq`, endereço, chave do circuito e do link) cifrado para cada contato e o entrega pelo circuito, no endereço que tem dele: o guardado, a dica e os antigos, nessa ordem. O quadro tem 200 B, como os da sinalização (`media::update`). O telefone do contato confere a cifra, não toca, responde com uma confirmação cifrada e o nó dele passa o endereço velho para o histórico e guarda o novo (o log mostra "... mudou de endereço e avisou: agora ..."). Quem não é contato não consegue abrir o quadro e não recebe resposta.
- **Fila de saída.** Contato desligado ou sem rede não confirma: o nó tenta de novo com espera que dobra (30 s, 1 min, 2 min, ... até 2 h) e desiste depois de 3 dias ou quando o contato sai da lista. Um endereço novo troca o registro pendente de todos. A fila só existe enquanto o programa está aberto; ao reabrir, um registro novo é enviado a todos.
- **Histórico.** Os últimos 4 endereços de cada contato ficam em `~/.clam-gui.known` (linhas com `antigo` no fim; `Node::history()` / `NodeConfig::history`), do mais novo ao mais velho, e são tentados depois do guardado e da dica ao discar. Quem mudou de endereço sem conseguir avisar (os dois mudaram, por exemplo) ainda é achado se um dos endereços antigos voltar a servir.
- **Ligado por padrão só sem diretório** (`NodeConfig::updates`; o diretório já cobre a mudança de endereço).

Limites: se os dois lados mudam de endereço enquanto estão separados, o aviso de um não chega ao outro; isso é para a Fase 2 de [ENCONTRO.md](ENCONTRO.md). O `seq` é a hora em ms: relógio que volta mais que o intervalo entre dois inícios faz o contato ignorar o aviso até o `seq` passar do último que ele viu.

Detalhes do modo sem diretório: o nó consulta o endereço de cada contato (até 32) ao subir e a cada 5 renovações, então dá para ligar a quem você nunca discou. Um furo de NAT que falha não é repetido para o mesmo contato por 5 minutos. Na GUI, discar roda em segundo plano (o botão "Desligar" cancela) e erros de áudio (fone desconectado, driver caiu) aparecem no log.

## Queda de rede no meio da chamada

Se nada chega por 8 s (a rede caiu ou mudou de Wi-Fi), a GUI dá a chamada como perdida. Quem discou tenta religar sozinho, até 5 vezes, com 3 s entre as tentativas, e a janela mostra "religando (n/5)". "Desligar" cancela. Se o outro lado desligou de propósito (`ActiveCall::peer_hung_up()`), ninguém religa; quem recebeu a chamada espera a nova ligação de quem discou.
