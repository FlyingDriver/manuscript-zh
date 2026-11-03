#!/usr/bin/env bash
# 编译并安装 Manuscript（简体中文版）到 ~/.local
#
# 用法:
#   ./scripts/build-install.sh            # 翻译 + 编译 + 安装
#   ./scripts/build-install.sh --no-zh    # 只编译安装英文原版
#   ./scripts/build-install.sh --check    # 只检查环境依赖
#
# 前置（唯一需要 root 的步骤，需手动执行一次）:
#   sudo apt install -y build-essential libgtk-4-dev libadwaita-1-dev \
#     libgtksourceview-5-dev libspelling-1-dev libonig-dev libpango1.0-dev
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT=$(pwd)

ZH=1
CHECK_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --no-zh) ZH=0 ;;
    --check) CHECK_ONLY=1 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done

echo "== 1/5 检查系统依赖 =="
MISSING=()
check_pc() {
  if pkg-config --exists "$1"; then
    printf '  ✓ %-20s %s\n' "$1" "$(pkg-config --modversion "$1")"
  else
    printf '  ✗ %-20s 缺失\n' "$1"
    MISSING+=("$2")
  fi
}
check_pc gtk4 libgtk-4-dev
check_pc libadwaita-1 libadwaita-1-dev
check_pc gtksourceview-5 libgtksourceview-5-dev
check_pc libspelling-1 libspelling-1-dev
check_pc oniguruma libonig-dev

if [ ${#MISSING[@]} -gt 0 ]; then
  echo
  echo "缺少开发库: ${MISSING[*]}"
  echo "请先执行:"
  echo "  sudo apt install -y build-essential libgtk-4-dev libadwaita-1-dev \\"
  echo "    libgtksourceview-5-dev libspelling-1-dev libonig-dev libpango1.0-dev"
  exit 1
fi
echo "  依赖齐全"

if [ "$CHECK_ONLY" = "1" ]; then
  exit 0
fi

echo
echo "== 2/5 应用简体中文翻译 =="
if [ "$ZH" = "1" ]; then
  python3 scripts/apply-zh-cn.py
else
  echo "  已跳过（--no-zh）"
fi

echo
echo "== 3/5 编译 release 版 =="
cargo build --release

echo
echo "== 4/5 安装到 ~/.local =="
make install PREFIX="$HOME/.local"

echo
echo "== 5/5 刷新桌面数据库 =="
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo
echo "✓ 安装完成"
echo "  可执行文件: $HOME/.local/bin/manuscript"
echo "  版本:       $("$HOME/.local/bin/manuscript" --version 2>/dev/null || echo '(需图形环境)')"
echo
echo "启动方式: 终端执行 manuscript  或  应用菜单搜索 \"Manuscript 手稿\""
