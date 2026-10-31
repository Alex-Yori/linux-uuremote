#!/usr/bin/env bash
# 安装 UU 远程（自研控制端）的应用图标与桌面条目：
# GNOME 的 Dock/Alt-Tab 图标按 WM_CLASS 匹配 .desktop 文件，
# 所以除了 SDL 运行时 set_icon，还要装 hicolor 图标 + StartupWMClass 对齐的 .desktop。
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$REPO/assets/icons/app-256.png"
[ -f "$SRC" ] || { echo "缺 $SRC（先跑 peicon 提取）"; exit 1; }

for sz in 16 24 32 48 64 128 256; do
  dir="$HOME/.local/share/icons/hicolor/${sz}x${sz}/apps"
  mkdir -p "$dir"
  if [ "$sz" = 256 ]; then
    cp "$SRC" "$dir/uu-remote.png"
  else
    ffmpeg -hide_banner -loglevel error -i "$SRC" -vf "scale=${sz}:${sz}" -y "$dir/uu-remote.png"
  fi
done

mkdir -p "$HOME/.local/share/applications"
cat > "$HOME/.local/share/applications/uu-remote.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=UU 远程
Comment=网易UU远程（Linux 自研控制端）
Exec=$REPO/target/release/uu-viewer %u
Icon=uu-remote
Terminal=false
StartupWMClass=uu-viewer
Categories=Network;RemoteAccess;
EOF

update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
gtk-update-icon-cache "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "installed: hicolor sizes + ~/.local/share/applications/uu-remote.desktop (StartupWMClass=uu-viewer)"
