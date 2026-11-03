#!/usr/bin/env bash
# Manuscript 手稿 — 预编译包安装脚本
#
# 用法:
#   sudo ./install.sh              # 安装到 /usr/local
#   PREFIX=$HOME/.local ./install.sh   # 安装到用户目录（无需 root）
#   ./install.sh --uninstall       # 卸载
set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
APP_ID="io.gitlab.ilshat_apps.manuscript"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

BINDIR="$PREFIX/bin"
DATADIR="$PREFIX/share"

die() { echo "错误: $*" >&2; exit 1; }

check_runtime() {
    local missing=()
    for lib in libgtk-4.so.1 libadwaita-1.so.0 libgtksourceview-5.so.0 libspelling-1.so.1; do
        ldconfig -p 2>/dev/null | grep -q "$lib" || missing+=("$lib")
    done
    if [ ${#missing[@]} -gt 0 ]; then
        echo "警告: 缺少运行库: ${missing[*]}"
        echo
        echo "Debian / Ubuntu / Linux Mint:"
        echo "  sudo apt install -y libgtk-4-1 libadwaita-1-0 libgtksourceview-5-0 libspelling-1-1"
        echo "Fedora:"
        echo "  sudo dnf install -y gtk4 libadwaita gtksourceview5 libspelling"
        echo
        echo "缺少运行库时程序可能无法启动。"
        echo
    fi
}

if [ "${1:-}" = "--uninstall" ]; then
    echo "== 卸载 =="
    rm -f "$BINDIR/manuscript"
    rm -f "$DATADIR/applications/$APP_ID.desktop"
    rm -f "$DATADIR/metainfo/$APP_ID.metainfo.xml"
    rm -f "$DATADIR/icons/hicolor/scalable/apps/$APP_ID.svg"
    rm -rf "$DATADIR/licenses/manuscript"
    update-desktop-database "$DATADIR/applications" 2>/dev/null || true
    gtk-update-icon-cache -f -t "$DATADIR/icons/hicolor" 2>/dev/null || true
    echo "已卸载（PREFIX=$PREFIX）"
    exit 0
fi

[ -x "$HERE/manuscript" ] || die "找不到可执行文件 $HERE/manuscript"

# 目标前缀不存在时先尝试创建（$HOME/.local 常常还没建），
# 创建失败才判断为不可写。直接测试 [ -w ] 会把「不存在」误判成「不可写」。
if [ ! -d "$PREFIX" ]; then
    if ! mkdir -p "$PREFIX" 2>/dev/null; then
        if [ "$(id -u)" -ne 0 ]; then
            echo "错误: 无法创建 $PREFIX" >&2
            echo "可改用用户目录安装:" >&2
            echo "  PREFIX=\$HOME/.local ./install.sh" >&2
            exit 1
        fi
    fi
fi

if [ ! -w "$PREFIX" ] && [ "$(id -u)" -ne 0 ]; then
    echo "错误: $PREFIX 不可写且非 root。" >&2
    echo "可改用用户目录安装:" >&2
    echo "  PREFIX=\$HOME/.local ./install.sh" >&2
    exit 1
fi

check_runtime

echo "== 安装到 $PREFIX =="
install -Dm755 "$HERE/manuscript"                        "$BINDIR/manuscript"
install -Dm644 "$HERE/data/$APP_ID.desktop"              "$DATADIR/applications/$APP_ID.desktop"
install -Dm644 "$HERE/data/$APP_ID.metainfo.xml"         "$DATADIR/metainfo/$APP_ID.metainfo.xml"
install -Dm644 "$HERE/data/icons/hicolor/scalable/apps/$APP_ID.svg" "$DATADIR/icons/hicolor/scalable/apps/$APP_ID.svg"
[ -f "$HERE/LICENSE" ] && install -Dm644 "$HERE/LICENSE" "$DATADIR/licenses/manuscript/LICENSE" || true

update-desktop-database "$DATADIR/applications" 2>/dev/null || true
gtk-update-icon-cache -f -t "$DATADIR/icons/hicolor" 2>/dev/null || true

echo
echo "✓ 安装完成"
echo "  可执行文件: $BINDIR/manuscript"
echo "  启动方式:   终端执行 manuscript，或应用菜单搜索「Manuscript 手稿」"
if ! echo "$PATH" | tr ':' '\\n' | grep -qx "$BINDIR"; then
    echo
    echo "注意: $BINDIR 不在 PATH 中，请先加入，或用绝对路径启动。"
fi
