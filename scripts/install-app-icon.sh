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
Name[en]=UU Remote
GenericName=Remote Desktop Client
Comment=连接并控制你的其他设备（键鼠透传 / 画质帧率调节 / 声音）
Exec=$HOME/.local/bin/uu-viewer-launch
Icon=uu-remote
Terminal=false
Categories=Network;RemoteAccess;
StartupWMClass=uu-viewer
Keywords=UU;远程;远控;remote;desktop;
EOF

# 旧的重复条目与 fork 遗留的控制端条目（deb 装的），在应用网格里都算 UU，
# 本地覆盖 NoDisplay 隐藏（不动包文件）。
rm -f "$HOME/.local/share/applications/linux-uuremote-viewer.desktop"
cat > "$HOME/.local/share/applications/uu-remote-linux-viewer.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=UU 远程 Linux 控制端
NoDisplay=true
EOF

update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
gtk-update-icon-cache "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

# 启动器用的也是最新构建
mkdir -p "$HOME/.local/bin"
cp "$REPO/target/release/uu-viewer" "$HOME/.local/bin/uu-viewer"
echo "installed: hicolor sizes + uu-remote.desktop(唯一条目) + ~/.local/bin/uu-viewer 已更新"
