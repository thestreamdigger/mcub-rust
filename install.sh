#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TARGET_DIR="$SCRIPT_DIR/target/release"
SNDALOOP_CONF="/etc/alsa/conf.d/_sndaloop.conf"
SUDOERS_SRC="$SCRIPT_DIR/config/sudoers.d/mcub-rust"
SUDOERS_DST="/etc/sudoers.d/mcub-rust"

echo "=== MCUB-Rust Installer ==="

# Source cargo env if available (rustup default location, not in non-interactive PATH)
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

if ! command -v cargo > /dev/null 2>&1; then
    echo "ERR: cargo not found"
    echo "  install via: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
fi

if dpkg -l moode-player > /dev/null 2>&1 && [ -f "$SNDALOOP_CONF" ]; then
    if grep -q 'pcm "hw:Loopback,0"' "$SNDALOOP_CONF"; then
        echo "[moOde] Fixing loopback: hw -> plughw (bitperfect DAC)"
        sudo sed -i 's/pcm "hw:Loopback,0"/pcm "plughw:Loopback,0"/' "$SNDALOOP_CONF"
        echo "  OK: see docs/MOODE_LOOPBACK.md"
    else
        echo "[moOde] Loopback: already patched"
    fi
fi

echo "[1/4] Building (release)..."
cd "$SCRIPT_DIR"
cargo build --release --bins > /dev/null 2>&1
echo "  OK: mcub-bridge-rust + mcub-watcher-rust"

echo "[2/4] Installing binaries..."
sudo install -m 755 "$TARGET_DIR/mcub-bridge-rust" /usr/local/bin/mcub-bridge-rust
sudo install -m 755 "$TARGET_DIR/mcub-watcher-rust" /usr/local/bin/mcub-watcher-rust
echo "  OK: /usr/local/bin/mcub-{bridge,watcher}-rust"

echo "[3/4] Installing service..."
sudo sed "s|@MCUB_BASE_DIR@|$SCRIPT_DIR|g" "$SCRIPT_DIR/services/watcher.service" \
    | sudo tee /etc/systemd/system/mcub-watcher-rust.service > /dev/null
sudo systemctl daemon-reload
echo "  OK: mcub-watcher-rust.service (base=$SCRIPT_DIR)"

if [ -f "$SUDOERS_SRC" ]; then
    echo "[..] Sudoers..."
    SUDOERS_TMP=$(mktemp)
    cp "$SUDOERS_SRC" "$SUDOERS_TMP"
    if sudo visudo -cf "$SUDOERS_TMP" >/dev/null 2>&1; then
        sudo install -m 440 -o root -g root "$SUDOERS_TMP" "$SUDOERS_DST"
        echo "  OK: $SUDOERS_DST"
    else
        echo "  ERR: sudoers validation failed, skipping"
    fi
    rm -f "$SUDOERS_TMP"
fi

echo "[4/4] Enabling service..."
sudo systemctl enable mcub-watcher-rust.service
sudo systemctl restart mcub-watcher-rust.service
sleep 2
if systemctl is-active --quiet mcub-watcher-rust.service; then
    echo "  OK: service running"
else
    echo "  WARN: service not running, check: journalctl -u mcub-watcher-rust"
fi

echo ""
echo "=== Done ==="
echo "Commands:"
echo "  systemctl status mcub-watcher-rust    # check status"
echo "  journalctl -u mcub-watcher-rust -f    # follow logs"
echo "  mcub-watcher-rust --status            # device info"
echo "  mcub-bridge-rust <mpd|cava|hybrid>    # manual run"
