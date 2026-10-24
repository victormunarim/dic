#!/usr/bin/env bash
# Chamada de voz de teste entre esta máquina (Linux, quem liga) e outra (Windows, quem atende).
#
#   Esta máquina:  relay A + chamador         (este script)
#   Outra máquina: telefone + relay B         (scripts/call-windows.ps1)
#
# Ordem:
#   1. aqui:     scripts/call.sh pub                       -> imprime a sua chave pública
#   2. Windows:  .\scripts\call-windows.ps1 -PublicIp <IP-WIN> -CallerPub <chave do passo 1> -OpenFirewall
#                (imprime o comando do passo 3)
#   3. aqui:     scripts/call.sh dial <IP-DESTA> <IP-WIN> '<linha addr,onion,link do relay B>' <chave pública do telefone> [segundos] [porta-do-telefone]
#
# <IP-DESTA> é o IP desta máquina alcançável pelo Windows (o relay B pode ser o primeiro salto e
# precisa chegar aqui). Libere a porta TCP 4001 (RELAY_A_PORT) no firewall desta máquina.
set -euo pipefail

cd "$(dirname "$0")/.."
KEY=${CLAM_KEY:-call-identity.key}
cargo build --release --features net --example call --example measure >&2
C=target/release/examples/call
M=target/release/examples/measure

case "${1:-}" in
pub)
    "$C" keygen "$KEY"
    ;;
dial)
    [ $# -ge 5 ] || { sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
    MY_IP=$2; OTHER_IP=$3; LINE_B=$4; PEER_PUB=$5; SECS=${6:-15}; PHONE_PORT=${7:-7000}
    RELAY_A_PORT=${RELAY_A_PORT:-4001}
    TMP=$(mktemp -d)
    trap 'kill "${RELAY_PID:-}" 2>/dev/null || true; rm -rf "$TMP"' EXIT
    # A também pode ser o relay de saída: precisa da rota até o telefone.
    "$M" relay "0.0.0.0:$RELAY_A_PORT" "$MY_IP:$RELAY_A_PORT" "contato:9=$OTHER_IP:$PHONE_PORT" >"$TMP/a.out" 2>"$TMP/a.err" &
    RELAY_PID=$!
    for _ in $(seq 1 50); do
        [ -s "$TMP/a.out" ] && break
        kill -0 "$RELAY_PID" 2>/dev/null || { echo "relay A caiu:"; cat "$TMP/a.err"; exit 1; }
        sleep 0.1
    done
    LINE_A=$(head -n1 "$TMP/a.out")
    [ -n "$LINE_A" ] || { echo "relay A não imprimiu a linha"; exit 1; }
    "$C" dial "$KEY" "$PEER_PUB" "$LINE_A" "$LINE_B" --seconds "$SECS"
    ;;
*)
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
