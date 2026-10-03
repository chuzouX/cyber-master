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
#   CYBER_DOWNLOAD_MIRROR 下载镜像前缀（默认自动尝试 ghproxy.net/gh-proxy.com/ghfast.top）
#
# Windows 用户请改用 install.ps1：
#   irm https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.ps1 | iex

set -eu

REPO="${CYBER_REPO:-chuzouX/cyber-master}"
VERSION="${CYBER_VERSION:-}"
INSTALL_DIR="${CYBER_INSTALL_DIR:-$HOME/.local/bin}"

# ─── 参数解析 ──────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
  case "$1" in
    --version|-v)
      [ $# -ge 2 ] && [ -n "$2" ] || { echo "--version 需要非空 tag" >&2; exit 1; }
      VERSION="$2"; shift 2 ;;
    --install-dir)
      [ $# -ge 2 ] && [ -n "$2" ] || { echo "--install-dir 需要非空路径" >&2; exit 1; }
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
  CYBER_DOWNLOAD_MIRROR  下载镜像前缀（默认自动尝试 ghproxy.net/gh-proxy.com/ghfast.top）
EOF
      exit 0 ;;
    *)
      echo "未知参数: $1（用 --help 查看用法）" >&2
      exit 1 ;;
  esac
done
case "$INSTALL_DIR" in
  *:*) echo "安装目录不能含 PATH 分隔符 ':'" >&2; exit 1 ;;
  *'
'*) echo "安装目录不能含换行" >&2; exit 1 ;;
  /*) ;;
  *) INSTALL_DIR="$PWD/$INSTALL_DIR" ;;
esac

# ─── 平台检测 ──────────────────────────────────────────────────────────────
case "$(uname -s)" in
  Linux)
    os=unknown-linux-gnu
    if ! getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
      echo "Linux 安装包需要 glibc；无法确认 glibc（需要 getconf），不支持 musl / Alpine。" >&2
      exit 1
    fi
    ;;
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
if command -v sha256sum >/dev/null 2>&1; then
  SHA256=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  SHA256="shasum -a 256"
else
  echo "缺少 SHA256 校验工具：需要 sha256sum 或 shasum，安装已中止。" >&2
  exit 1
fi

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

# ─── 下载源：GitHub 主源 + 常用镜像回退（可用 CYBER_DOWNLOAD_MIRROR 指定镜像前缀）──
github_base="https://github.com/$REPO/releases/download/$VERSION"
if [ -n "${CYBER_DOWNLOAD_MIRROR:-}" ]; then
  download_base_list="${CYBER_DOWNLOAD_MIRROR%/}/$github_base $github_base"
else
  download_base_list="$github_base https://ghproxy.net/$github_base https://gh-proxy.com/$github_base https://ghfast.top/$github_base"
fi

echo "→ 安装 cyber $VERSION ($target) 到 $INSTALL_DIR"

# ─── 创建临时目录 ─────────────────────────────────────────────────────────
tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t cyber-install)"
trap 'rm -rf "$tmpdir"' 0
trap 'exit 130' INT
trap 'exit 143' TERM

# ─── 下载（主源失败自动回退镜像）──────────────────────────────────────────
downloaded=0
for base in $download_base_list; do
  echo "→ 下载 $base/$archive"
  if curl -fsSL --connect-timeout 15 --max-time 30 -o "$tmpdir/$archive" "$base/$archive" \
     && curl -fsSL --connect-timeout 15 --max-time 30 -o "$tmpdir/$archive.sha256" "$base/$archive.sha256"; then
    downloaded=1
    break
  fi
  echo "  该源失败，尝试下一源…" >&2
done
[ "$downloaded" -eq 1 ] \
  || { echo "所有下载源均失败。请检查网络/代理（需放行 release-assets.githubusercontent.com），或用 CYBER_DOWNLOAD_MIRROR 指定镜像前缀。" >&2; exit 1; }
echo "→ 校验 SHA256…"
expected=$(awk 'NR == 1 { print $1 }' "$tmpdir/$archive.sha256")
case "$expected" in
  ''|*[!0-9a-fA-F]*) echo "SHA256 校验文件格式无效" >&2; exit 1 ;;
esac
[ "${#expected}" -eq 64 ] || { echo "SHA256 校验文件格式无效" >&2; exit 1; }
expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
actual=$($SHA256 "$tmpdir/$archive" | awk '{ print $1 }')
[ "$expected" = "$actual" ] \
  || { echo "SHA256 校验失败，文件可能损坏或被篡改" >&2; exit 1; }

# ─── 解压 + 安装 ──────────────────────────────────────────────────────────
tar -xzf "$tmpdir/$archive" -C "$tmpdir"
[ -f "$tmpdir/$binary" ] && [ ! -L "$tmpdir/$binary" ] \
  || { echo "压缩包内未找到有效的 cyber 二进制" >&2; exit 1; }
chmod +x "$tmpdir/$binary"
mkdir -p "$INSTALL_DIR"
mv -f "$tmpdir/$binary" "$INSTALL_DIR/$binary"

# ─── 幂等用户 PATH 配置（不覆盖现有启动文件）──────────────────────────────
# 单引号转义，避免自定义路径被 shell 当作命令展开。
quoted_dir=$(printf '%s' "$INSTALL_DIR" | sed "s/'/'\\\\''/g")
path_line="case \":\$PATH:\" in *:'$quoted_dir':*) ;; *) export PATH='$quoted_dir':\"\$PATH\" ;; esac"
append_path() {
  mkdir -p "$(dirname "$1")"
  if ! grep -Fqx "$path_line" "$1" 2>/dev/null; then
    printf '\n# Cyber installer: user PATH\n%s\n' "$path_line" >> "$1"
  fi
  echo "→ 用户 PATH 配置: $1"
}
shell_name="$(basename "${SHELL:-sh}")"
case "$shell_name" in
  fish)
    # fish 的单引号字符串只需要转义反斜杠和单引号。
    quoted_dir=$(printf '%s' "$INSTALL_DIR" | sed "s/\\\\/\\\\\\\\/g; s/'/\\\\'/g")
    path_line="contains -- '$quoted_dir' \$PATH; or set -gx PATH '$quoted_dir' \$PATH"
    append_path "${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/cyber-path.fish"
    ;;
  zsh) append_path "${ZDOTDIR:-$HOME}/.zshrc" ;;
  bash)
    append_path "$HOME/.bashrc"
    # Bash 登录 shell 只读取第一个存在的用户 profile。
    if [ -f "$HOME/.bash_profile" ]; then
      append_path "$HOME/.bash_profile"
    elif [ -f "$HOME/.bash_login" ]; then
      append_path "$HOME/.bash_login"
    else
      append_path "$HOME/.profile"
    fi
    ;;
  sh|dash|ksh) append_path "$HOME/.profile" ;;
  *)
    echo "未知 shell '$shell_name'，未修改启动文件。请手动将 $INSTALL_DIR 加入 PATH。" >&2
    ;;
esac
echo ""
echo "✓ 已安装: $INSTALL_DIR/$binary"
echo "请重新打开终端后运行 cyber（POSIX shell 需重新登录）；管道安装无法修改父 shell 的 PATH。"
echo "也可立即运行: \"$INSTALL_DIR/$binary\""

# ─── 首次启动提示 ─────────────────────────────────────────────────────────
echo ""
echo "首次运行 cyber 会自动在 ~/.cyber/ 创建配置目录。"
echo "文档: https://github.com/$REPO#readme"
