#!/usr/bin/env bash
# Mede o circuito de 2 saltos entre esta máquina (Linux) e outra (por exemplo, Windows).
#
#   Esta máquina:  relay A + cliente (probe e direct)
#   Outra máquina: destino (contact) + relay B   -> scripts/measure-windows.ps1
#
# Uso:  scripts/measure.sh <IP-DA-OUTRA> '<linha addr,onion,link do relay B>' [segundos] [porta-contact]
# Ex.:  scripts/measure.sh 192.168.0.20 '192.168.0.20:4001,ab12...,cd34...' 60
#
# O circuito é cliente -> A -> B -> destino ou cliente -> B -> A -> destino: o gerente sorteia
# a ordem a cada execução (o `probe` imprime a que valeu). O destino fica na outra máquina.
# O `direct` mede o TCP puro daqui até o destino, a referência para o custo do circuito.
set -euo pipefail

if [ $# -lt 2 ]; then
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi
OTHER_IP=$1
LINE_B=$2
SECS=${3:-60}
CONTACT_PORT=${4:-9000}
RELAY_A_PORT=${RELAY_A_PORT:-4001}

cd "$(dirname "$0")/.."
cargo build --release --features net --example measure
M=target/release/examples/measure
OUT=measure-results-$(date +%Y%m%d-%H%M%S).txt
TMP=$(mktemp -d)
trap 'kill "${RELAY_PID:-}" 2>/dev/null || true; rm -rf "$TMP"' EXIT

# Relay A escuta só em loopback: o cliente está nesta mesma máquina. O gerente sorteia a
# ordem dos relays, então A também pode ser o de saída: ele precisa da rota até o destino.
# O `probe` imprime o caminho que valeu em cada medição.
"$M" relay "127.0.0.1:$RELAY_A_PORT" "127.0.0.1:$RELAY_A_PORT" "contato:9=$OTHER_IP:$CONTACT_PORT" >"$TMP/relay_a.out" 2>"$TMP/relay_a.err" &
RELAY_PID=$!
for _ in $(seq 1 50); do
    [ -s "$TMP/relay_a.out" ] && break
    kill -0 "$RELAY_PID" 2>/dev/null || { echo "relay A caiu:"; cat "$TMP/relay_a.err"; exit 1; }
    sleep 0.1
done
LINE_A=$(head -n1 "$TMP/relay_a.out")
[ -n "$LINE_A" ] || { echo "relay A não imprimiu a linha"; exit 1; }

{
    echo "# $(date -Is)  segundos=$SECS  outra=$OTHER_IP  contact=$CONTACT_PORT"
    echo "## direct (TCP puro até o destino)"
    "$M" direct "$OTHER_IP:$CONTACT_PORT" --seconds "$SECS"
    echo "## circuito sem cover"
    "$M" probe "$LINE_A" "$LINE_B" --seconds "$SECS" --no-cover --no-circuit-cover
    for p in economy standard strong; do
        echo "## circuito com cover, perfil $p"
        "$M" probe "$LINE_A" "$LINE_B" --seconds "$SECS" --profile "$p"
    done
} 2>&1 | tee "$OUT"

echo
echo "Resultado salvo em $OUT"
