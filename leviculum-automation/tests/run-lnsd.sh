#!/usr/bin/env bash
# Zaženi lnsd z SerialInterface na XIAO LNode (/dev/ttyACM1)
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
CONFIG="$DIR/lnsd-config"

# Preveri serial port
if [[ ! -e /dev/ttyACM1 ]]; then
    echo "NAPAKA: /dev/ttyACM1 ni najden. Poveži XIAO nRF52840 in potrdi z:"
    echo "  ls /dev/ttyACM*"
    exit 1
fi

# Preveri ali sistemski lnsd daemon že drži /dev/ttyACM1 (prek /dev/leviculum-xiao-transport*)
if pgrep -u leviculum lnsd >/dev/null 2>&1; then
    echo "OPOZORILO: Sistemski servis 'lnsd' (uporabnik leviculum, /etc/reticulum/config)"
    echo "           že teče in drži ekskluzivni zaklep (TIOCEXCL) na /dev/ttyACM1"
    echo "           prek /dev/leviculum-xiao-transport*!"
    echo "           Ustavljam sistemski lnsd servis (sudo systemctl stop lnsd) ..."
    sudo systemctl stop lnsd
fi

echo "Zaganjam lnsd (config: $CONFIG) ..."
echo "Ctrl+C za zaustavitev."
echo ""
exec lnsd --config "$CONFIG" -v
