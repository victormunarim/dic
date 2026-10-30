---
name: Clam
description: Chamadas P2P em soft UI silencioso, com relevo de duas sombras e um único acento.
colors:
  primary: "#5360ea"
  base-dark: "#2b2e35"
  well-dark: "#24272d"
  selected-dark: "#333740"
  hover-dark: "#353942"
  highlight-dark: "#383c45"
  shade-dark: "#1d1f24"
  divider-dark: "#4a4f5a"
  text-dark: "#dde0e6"
  muted-dark: "#9aa1ad"
  strong-dark: "#ffffff"
  live-dark: "#1a8650"
  destructive-dark: "#cf3d42"
  warning-dark: "#f0b232"
  error-dark: "#f27b7e"
  base-light: "#e4e8ee"
  well-light: "#d6dbe4"
  selected-light: "#d9dee8"
  hover-light: "#d3d9e3"
  highlight-light: "#ffffff"
  shade-light: "#a0a8b6"
  divider-light: "#9aa2b1"
  text-light: "#2a2e37"
  muted-light: "#555d6b"
  strong-light: "#121419"
  live-light: "#15723e"
  destructive-light: "#c2343a"
  warning-light: "#8f5f00"
  error-light: "#b0242a"
typography:
  heading:
    fontFamily: "egui default proportional"
    fontSize: "26px"
    fontWeight: 700
  card-title:
    fontFamily: "egui default proportional"
    fontSize: "18px"
    fontWeight: 700
  body:
    fontFamily: "egui default proportional"
    fontSize: "16px"
    fontWeight: 400
  note:
    fontFamily: "egui default proportional"
    fontSize: "14px"
    fontWeight: 400
  small:
    fontFamily: "egui default proportional"
    fontSize: "13px"
    fontWeight: 400
  mono:
    fontFamily: "egui default monospace"
    fontSize: "14.5px"
    fontWeight: 400
rounded:
  card: "18px"
  button: "12px"
  rail-icon: "18px"
  avatar: "999px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "16px"
  xl: "22px"
components:
  button:
    backgroundColor: "{colors.base-dark}"
    textColor: "{colors.strong-dark}"
    rounded: "{rounded.button}"
    height: "40px"
    width: "110px"
  button-primary:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.strong-dark}"
    rounded: "{rounded.button}"
    height: "40px"
  icon-button:
    backgroundColor: "{colors.base-dark}"
    textColor: "{colors.strong-dark}"
    rounded: "{rounded.button}"
    size: "44px"
  card:
    backgroundColor: "{colors.base-dark}"
    textColor: "{colors.text-dark}"
    rounded: "{rounded.card}"
    padding: "22px"
  field:
    backgroundColor: "{colors.well-dark}"
    textColor: "{colors.text-dark}"
    rounded: "{rounded.button}"
    padding: "10px 12px"
  rail-icon:
    backgroundColor: "{colors.base-dark}"
    textColor: "{colors.strong-dark}"
    rounded: "{rounded.rail-icon}"
    size: "52px"
---

# Design System: Clam

## Overview

**Creative North Star: "A Sala Silenciosa"**

O Clam se comporta como uma sala onde ninguém levanta a voz. Tudo é moldado do mesmo material macio: uma única cor base, relevo dado por duas sombras opostas e quase nenhuma cor. A privacidade é o produto, então a interface não grita: ícones no lugar de parágrafos, explicações guardadas no hover, e o único som visual é o aro do acento em volta de quem está falando.

A densidade é média e folgada (margens internas de 22 px nos cartões, espaçamento em múltiplos de 4 px). O desenho segue o padrão de trilho, coluna e centro conhecido de apps de chamada, porque o usuário já o conhece, e esconde a complexidade da rede atrás de poucos controles. Os dois temas (escuro padrão, claro) usam a mesma gramática de relevo; só a paleta muda.

**Key Characteristics:**
- Uma cor base por tema; superfície e fundo são o mesmo material.
- Três estados e só três: elevado, afundado, plano.
- Um único acento; vermelho só destrutivo, verde só "ao vivo/ok".
- Ícones desenhados com o `painter`, sem depender de glifo da fonte.
- Contraste AA nos dois temas, conferido por teste.

## Colors

Paleta sóbria e azulada: um cinza-ardósia (ou cinza-névoa no claro), um acento índigo e dois semânticos. Os tokens `*-dark` e `*-light` são os valores de `Palette` em `examples/gui/theme.rs`; `pal()` escolhe o conjunto.

### Primary
- **Índigo do Aro** (#5360ea): ação principal, item ativo/selecionado, aro de quem fala, seleção de texto. É o único acento e fica igual nos dois temas.

### Secondary
- **Verde Ao Vivo** (#1a8650 escuro, #15723e claro): só "ao vivo/ok" (online, falando, confirmação).
- **Vermelho Destrutivo** (#cf3d42 escuro, #c2343a claro): só remover, desligar, encerrar.
- **Âmbar de Aviso** (#f0b232 escuro, #8f5f00 claro): atenção e o ícone do sol do tema.

### Neutral
- **Ardósia Base** (#2b2e35) / **Névoa Base** (#e4e8ee): fundo, trilho, cartões e botões em repouso — o mesmo material.
- **Poço** (#24272d / #d6dbe4): campos de texto e caixas afundadas.
- **Selecionado** (#333740 / #d9dee8) e **Hover** (#353942 / #d3d9e3): itens de lista e controles sob o ponteiro.
- **Luz** (#383c45 / #ffffff) e **Sombra** (#1d1f24 / #a0a8b6): o par de sombras do relevo.
- **Divisor** (#4a4f5a / #9aa2b1): linha fina entre trilho, salas, centro e membros.
- **Texto** (#dde0e6 / #2a2e37), **Texto Apagado** (#9aa1ad / #555d6b), **Texto Forte** (#ffffff / #121419) e **Erro** (#f27b7e / #b0242a).

### Named Rules
**The One Voice Rule.** O acento índigo é a única cor viva da tela; se dois elementos competem por ela, um deles é o errado.
**The Reserved Semantics Rule.** Verde significa "ao vivo/ok" e vermelho significa destrutivo, nunca decoração.
**The Not-Shadow-Alone Rule.** Estado importante (ativo, desativado, erro, quem fala) soma cor do acento, ícone ou rótulo ao relevo; sombra sozinha não basta.

## Typography

**Display / Body Font:** a proporcional padrão do egui (`default_fonts`).
**Mono Font:** a monoespaçada padrão do egui, para chaves e códigos.

**Character:** neutra e legível, sem personalidade tipográfica própria; a personalidade vem do relevo e do espaço.

### Hierarchy
- **Heading** (700, 26 px): título da página.
- **Card title** (700, 18 px): título de cartão e de painel; cor `strong`.
- **Body** (400, 16 px): texto corrente e botões.
- **Note** (400, 14 px, cor apagada): subtítulos e ajuda curta.
- **Small** (400, 13 px): rótulos pequenos e estados.
- **Mono** (400, 14.5 px): chaves e códigos de contato.

### Named Rules
**The Icon-Before-Text Rule.** Se um ícone resolve, não escreva um parágrafo; o que não é óbvio vira `on_hover_text`.

## Layout

Quatro colunas separadas por um divisor fino: trilho de servidores (76 px fixos), coluna de salas ou contatos, centro e, na tela do servidor, membros. Com janela larga (≥ 780 px) cada página se divide em duas colunas; a janela tem mínimo de 820×560. Espaçamento em múltiplos de 4 px, item de lista a 10 px, cartões com 22 px de margem interna e 4 px de respiro externo. O botão do tema fica preso no pé do trilho; a rolagem dos servidores usa o que sobra.

## Elevation & Depth

Relevo estrutural, não enfeite: duas sombras opostas dizem o que é elevado (luz no canto superior esquerdo, sombra no inferior direito), afundado (as mesmas sombras para dentro, num traço de 2 px) ou plano (texto e ícone sem relevo). Não há sombra de estado nem de foco além disso; o desativado perde o relevo e fica opaco.

### Shadow Vocabulary
- **Elevado do cartão e da janela** (profundidade 6 px, blur 12): cartões, janelas, menus.
- **Elevado do controle** (profundidade 4 px, blur 8): botões, ícones do trilho, popups.
- **Afundado** (traço de 2 px escuro em cima/esquerda, claro embaixo/direita): campos, item ativo, botão apertado.

### Named Rules
**The Two Shadows Rule.** Todo relevo é o par luz + sombra, sempre opostos; uma sombra sozinha é bug.
**The No-Hot-Path Rule.** Nada de desenhar relevo por quadro de vídeo ou por pacote.

## Shapes

Cantos generosos e iguais por categoria: 18 px nos cartões e janelas, 12 px nos botões, campos e menus, 18 px nos ícones do trilho e círculo nos avatares. Sem bordas coloridas nem contornos duros; a única linha é o divisor fino entre seções e o contorno de 1 px nos menus e janelas, para não se misturarem ao fundo.

## Components

### Buttons
- **Shape:** cantos de 12 px, 40 px de altura, 110 px de largura mínima (180×48 o grande).
- **Neutral:** a própria base, elevada; ao apertar afunda.
- **Primary:** preenchido com o acento, texto branco, relevo igual.
- **Destructive:** preenchido com o vermelho; **ao vivo:** com o verde.
- **Disabled:** opaco e sem relevo.

### Icon buttons
Quadrados de 44×40 px, só ícone; o nome da ação aparece ao passar o mouse. Ações secundárias e destrutivas vão num menu de três pontos.

### Cards / Containers
- **Corner Style:** 18 px.
- **Background:** a base.
- **Shadow Strategy:** elevado (6 px).
- **Internal Padding:** 22 px.

### Inputs / Fields
- **Style:** afundado no poço, 12 px de raio, margem 12×10.
- **Focus:** o traço do acento do `egui` sobre o poço; o texto de dica fica apagado.

### Navigation
O trilho de 76 px tem ícones de 52×52 px: Contatos (👤 com selo de não lidas), um por servidor (inicial), "+" (verde) e, no pé, o tema (sol no escuro, lua no claro). Item ativo afunda e vira o acento.

### Avatar
Círculo com a inicial sobre uma de seis cores estáveis por nome, com bolinha de estado opcional.

## Do's and Don'ts

### Do:
- **Do** usar `card`, `button`, `icon_button`, `raised_frame`, `sunken_frame`, `raised` e `sunken` de `theme.rs`.
- **Do** ler cores só de `pal()`; texto sobre a base precisa de 4,5:1, ícone e cor de estado de 3:1.
- **Do** desenhar o ícone importante com o `painter` quando a fonte pode não ter o glifo.
- **Do** explicar o que não é óbvio no hover, e manter visível, com aceite, o aviso que protege a pessoa (IP, privacidade).

### Don't:
- **Don't** pintar `egui::Frame` com cor chapada nem sombra à mão fora de `theme.rs`.
- **Don't** criar uma segunda cor de acento nem usar vermelho/verde fora do significado.
- **Don't** deixar estado só na sombra: some cor do acento, ícone ou rótulo.
- **Don't** despejar parágrafo fixo na tela quando um ícone com hover resolve.
