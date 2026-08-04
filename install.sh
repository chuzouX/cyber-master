#!/bin/sh
# Cyber Master 一键安装脚本（Unix：Linux / macOS）
#
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.sh | sh
#   或:
#   curl -fsSL https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.sh | sh -s -- --version v0.1.0
#   或本仓库内直接运行:
#   sh install.sh [--version v0.1.0] [--install-dir /path/to/bin]
#
# 环境变量覆盖：
#   CYBER_VERSION     指定版本 tag（如 v0.1.0），默认取 latest release
#   CYBER_INSTALL_DIR 安装目录，默认 ~/.local/bin
#   CYBER_REPO        GitHub 仓库（owner/name），默认 chuzouX/cyber-master
#
# Windows 用户请改用 install.ps1：
#   irm https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.ps1 | iex

set -eu

REPO="${CYBER_REPO:-chuzouX/cyber-master}"
VERSION=""
INSTALL_DIR="${CYBER_INSTALL_DIR:-$HOME/.local/bin}"

# ─── 参数解析 ──────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
  case "$1" in
    --version|-v)
      VERSION="$2"; shift 2 ;;
    --install-dir)
      INSTALL_DIR="$2"; shift 2 ;;
    --help|-h)
      cat <<EOF
Cyber Master installer

Usage: sh install.sh [OPTIONS]

Options:
  --version <tag>        指定版本（如 v0.1.0），默认 latest
  --install-dir <path>   安装目录，默认 ~/.local/bin
  -h, --help             显示此帮助

Environment:
  CYBER_VERSION          等价于 --version
  CYBER_INSTALL_DIR      等价于 --install-dir
  CYBER_REPO             GitHub owner/name，默认 chuzouX/cyber-master
EOF
      exit 0 ;;
    *)
      echo "未知参数: $1（用 --help 查看用法）" >&2
      exit 1 ;;
  esac
done
[ -n "${CYBER_VERSION:-}" ] && VERSION="$CYBER_VERSION"

# ─── 平台检测 ──────────────────────────────────────────────────────────────
case "$(uname -s)" in
  Linux)  os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "检测到 Windows / Git Bash。请改用 install.ps1：" >&2
    echo "  powershell -c \"irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex\"" >&2
    exit 1 ;;
  *) echo "不支持的 OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64)    arch=x86_64 ;;
  arm64|aarch64)   arch=aarch64 ;;
  *) echo "不支持的架构: $(uname -m)" >&2; exit 1 ;;
esac

target="$arch-$os"
archive="cyber-$target.tar.gz"
binary="cyber"

# ─── 依赖检查 ──────────────────────────────────────────────────────────────
need() { command -v "$1" >/dev/null 2>&1 || { echo "缺少依赖: $1" >&2; exit 1; }; }
need curl
need tar

# ─── 解析版本（未指定时取 latest）──────────────────────────────────────────
if [ -z "$VERSION" ]; then
  echo "→ 查询最新版本…"
  VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
            | grep -E '"tag_name"' | head -n1 | sed -E 's/.*"([^"]+)".*/\1/')
  if [ -z "$VERSION" ]; then
    echo "无法获取最新版本。请用 --version <tag> 显式指定，或检查网络。" >&2
    exit 1
  fi
fi

download_url="https://github.com/$REPO/releases/download/$VERSION/$archive"
checksum_url="$download_url.sha256"

echo "→ 安装 cyber $VERSION ($target) 到 $INSTALL_DIR"

# ─── 创建临时目录 ─────────────────────────────────────────────────────────
tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t cyber-install)"
trap 'rm -rf "$tmpdir"' EXIT INT TERM

# ─── 下载 ────────────────────────────────────────────────────────────────
echo "→ 下载 $download_url"
curl -fsSL -o "$tmpdir/$archive" "$download_url"

# ─── 校验 SHA256（可选：若 .sha256 不存在则跳过，不阻断安装）────────────
if sha256sum --version >/dev/null 2>&1; then
  if curl -fsSL -o "$tmpdir/$archive.sha256" "$checksum_url"; then
    echo "→ 校验 SHA256…"
    # sha256sum 校验时需在文件同目录下，且 .sha256 内是相对文件名
    (cd "$tmpdir" && sha256sum -c "$archive.sha256" 2>/dev/null) \
      || { echo "SHA256 校验失败，文件可能损坏或被篡改" >&2; exit 1; }
  else
    echo "  (未找到 .sha256 校验文件，跳过校验)"
  fi
fi

# ─── 解压 + 安装 ──────────────────────────────────────────────────────────
mkdir -p "$INSTALL_DIR"
tar -xzf "$tmpdir/$archive" -C "$tmpdir"
mv -f "$tmpdir/$binary" "$INSTALL_DIR/$binary"
chmod +x "$INSTALL_DIR/$binary"

# ─── PATH 提示 ────────────────────────────────────────────────────────────
case ":$PATH:" in
  *":$INSTALL_DIR:"*)
    echo ""
    echo "✓ 已安装: $INSTALL_DIR/$binary"
    echo "  直接运行: cyber"
    ;;
  *)
    echo ""
    echo "✓ 已安装: $INSTALL_DIR/$binary"
    echo ""
    echo "⚠ $INSTALL_DIR 不在 PATH 中。请添加："
    # shell 检测，给出最贴合的命令
    shell_name="$(basename "${SHELL:-}")"
    case "$shell_name" in
      fish)
        echo "    set -Ux fish_user_paths $INSTALL_DIR \$fish_user_paths"
        ;;
      zsh)
        echo "    echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc && source ~/.zshrc"
        ;;
      bash)
        echo "    echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc && source ~/.bashrc"
        ;;
      *)
        echo "    echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.profile"
        ;;
    esac
    echo "  然后运行: cyber"
    ;;
esac

# ─── 首次启动提示 ─────────────────────────────────────────────────────────
echo ""
echo "首次运行 cyber 会自动在 ~/.cyber/ 创建配置目录。"
echo "文档: https://github.com/$REPO#readme"
