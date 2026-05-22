#!/usr/bin/env bash
# regen-screenshot.sh — regenerate docs/itsy-tui.png from itsy-tui.ansi.
#
# itsy-tui.ansi is a hand-curated snippet of what a typical session
# looks like: welcome banner, a few tool calls, a closing summary.
# It uses real ESC bytes (0x1b) so freeze can interpret the ANSI
# colour codes directly — `cat`ing it does the right thing.
#
# Dependencies:
#   * freeze (Charm)  — https://github.com/charmbracelet/freeze/releases
#   * a Nerd-Font-capable system font; the default freeze font handles
#     ›, ✓, ✗, ⚡, │ fine, but if you want the gear glyph (⚙) install
#     a Nerd Font and pass --font.family="JetBrainsMono Nerd Font Mono".
#
# Editing the screenshot:
#   1. Tweak docs/itsy-tui.ansi (keep real ESC bytes; if you edit it
#      in an editor that strips them, re-run the python re-encode at
#      the bottom of this script with EDIT_MODE=1).
#   2. Re-run this script.
#   3. Commit both the .ansi source and the rendered .png.

set -euo pipefail

cd "$(dirname "$0")"

# Optional: re-encode the file in case the ESC bytes were lost in an
# editor round-trip. Triggered by EDIT_MODE=1.
if [ "${EDIT_MODE:-0}" = "1" ]; then
  python3 - <<'PY'
import re
src = open('itsy-tui.ansi', 'rb').read().decode()
out = re.sub(r'\[([0-9;]+)m', lambda m: '\x1b[' + m.group(1) + 'm', src)
open('itsy-tui.ansi', 'wb').write(out.encode())
print('re-encoded itsy-tui.ansi with', len(out), 'bytes')
PY
fi

command -v freeze >/dev/null || {
  cat >&2 <<'EOF'
freeze not on $PATH. Install:

  # Linux x86_64
  curl -fsSL -o /tmp/freeze.tgz \
    https://github.com/charmbracelet/freeze/releases/download/v0.2.1/freeze_0.2.1_Linux_x86_64.tar.gz
  tar -xzf /tmp/freeze.tgz -C /tmp
  sudo install -m 0755 /tmp/freeze_0.2.1_Linux_x86_64/freeze /usr/local/bin/

  # macOS
  brew install charmbracelet/tap/freeze
EOF
  exit 1
}

freeze --execute "cat itsy-tui.ansi" \
  --output itsy-tui.png \
  --background "#0f1117" \
  --font.size 14 \
  --line-height 1.4 \
  --padding "30,40,30,40" \
  --border.radius 10 \
  --shadow.blur 30 --shadow.y 12 \
  --window

echo "wrote $(pwd)/itsy-tui.png"
