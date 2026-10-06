#!/bin/sh
# Cyber Master 一键安装脚本（Unix：Linux / macOS）
#
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.sh | sh
#   或:
#   curl -fsSL https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.sh | sh -s -- --version v0.1.0
#   或本仓库内直接运行:
#   sh install.sh [--version v0.1.0] [--install-dir /path/to/bin] [--force]
#
# 若本机已安装 cyber，脚本会先检测已安装版本并与云端最新版本对比：
#   - 已是最新：直接提示并退出（除非 --force）；
#   - 不是最新：询问是否更新到最新版本（回车 / y 确认，n 取消）；
#   - --force 或 CYBER_FORCE=1：跳过版本检查与询问，直接下载并覆盖安装。
#
# 环境变量覆盖：
#   CYBER_VERSION     指定版本 tag（如 v0.1.0），默认取 latest release
#   CYBER_INSTALL_DIR 安装目录，默认 ~/.local/bin
#   CYBER_REPO        GitHub 仓库（owner/name），默认 chuzouX/cyber-master
#   CYBER_DOWNLOAD_MIRROR 下载镜像前缀（默认自动尝试 ghproxy.net/gh-proxy.com/ghfast.top）
#   CYBER_FORCE       设为 1 时等价于 --force（跳过版本检查与询问，直接覆盖安装）
#
# Windows 用户请改用 install.ps1：
#   irm https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.ps1 | iex

set -eu

REPO="${CYBER_REPO:-chuzouX/cyber-master}"
CNB_REPO="${CYBER_CNB_REPO:-funxlink/cyber-master}"
VERSION="${CYBER_VERSION:-}"
INSTALL_DIR="${CYBER_INSTALL_DIR:-$HOME/.local/bin}"
USE_CNB="${CYBER_USE_CNB:-0}"
FORCE="${CYBER_FORCE:-0}"
# ─── 参数解析 ──────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
  case "$1" in
    --version|-v)
      [ $# -ge 2 ] && [ -n "$2" ] || { echo "--version 需要非空 tag" >&2; exit 1; }
      VERSION="$2"; shift 2 ;;
    --install-dir)
      [ $# -ge 2 ] && [ -n "$2" ] || { echo "--install-dir 需要非空路径" >&2; exit 1; }
      INSTALL_DIR="$2"; shift 2 ;;
    --cnb)
      USE_CNB=1; shift ;;
    --force|-f)
      FORCE=1; shift ;;
    --help|-h)
      cat <<EOF
Cyber Master installer

Usage: sh install.sh [OPTIONS]

Options:
  --version <tag>        指定版本（如 v0.1.0），默认 latest
  --install-dir <path>   安装目录，默认 ~/.local/bin
  --cnb                  优先使用 CNB 国内极速源
  --force, -f            跳过版本检查与询问，直接下载并覆盖安装
  -h, --help             显示此帮助

Environment:
  CYBER_VERSION          等价于 --version
  CYBER_INSTALL_DIR      等价于 --install-dir
  CYBER_REPO             GitHub owner/name，默认 chuzouX/cyber-master
  CYBER_DOWNLOAD_MIRROR  下载镜像前缀（默认自动尝试 ghproxy.net/gh-proxy.com/ghfast.top）
  CYBER_CNB_REPO         CNB 镜像仓库（默认同 CYBER_REPO）
  CYBER_USE_CNB          设为 1 时优先使用 CNB 源
  CYBER_FORCE            设为 1 时等价于 --force
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
# release 接口可能返回单行（minified）JSON；此时 `sed -E 's/.*"([^"]+)".*/\1/'`
# 的贪婪匹配会一路吃到 `"body"`（发布说明）的尾部，把整段 CHANGELOG 当成版本号。
# 这里锚定 `"tag_name"` 键抽取，并用 `is_version_tag` 过滤，任何脏值都不会流入
# 版本比较与下载 URL。
extract_tag_name() {
  sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1
}

# 版本号形态校验：vX.Y.Z（Y/Z 可省略，允许 -pre / +build 后缀）。
is_version_tag() {
  printf '%s' "$1" | grep -Eq '^[vV]?[0-9]+(\.[0-9]+){1,2}([-+][0-9A-Za-z.]+)?$'
}

# 依次尝试各源的 release 接口，返回第一个解析成功且形态合法的 tag；全部失败返回非 0。
resolve_latest_version() {
  _cands=""
  if [ "$USE_CNB" = "1" ]; then
    _cands="https://api.cnb.cool/$CNB_REPO/-/releases"
  fi
  _cands="$_cands https://api.github.com/repos/$REPO/releases/latest https://api.cnb.cool/$CNB_REPO/-/releases"
  for _url in $_cands; do
    _body=$(curl -fsSL --connect-timeout 4 --max-time 6 "$_url" 2>/dev/null || true)
    _tag=$(printf '%s' "$_body" | extract_tag_name || true)
    if is_version_tag "$_tag"; then
      printf '%s' "$_tag"
      return 0
    fi
  done
  return 1
}

if [ -z "$VERSION" ]; then
  echo "→ 查询最新版本…"
  if ! VERSION=$(resolve_latest_version); then
    echo "无法获取最新版本。请用 --version <tag> 显式指定，或检查网络。" >&2
    exit 1
  fi
fi
if ! is_version_tag "$VERSION"; then
  echo "版本号无效: $VERSION（期望形如 v1.2.3；请用 --version <tag> 指定正确 tag）" >&2
  exit 1
fi

# ─── 已安装版本检测、版本对比与更新确认 ────────────────────────────────────
# 版本号比较：ver_lt A B，当 A 严格小于 B 时返回 0（忽略 v/V 前缀，按数字段比较）。
ver_lt() {
  awk -v a="$(printf '%s' "$1" | sed 's/^[vV]//; s/[-+].*$//')" \
      -v b="$(printf '%s' "$2" | sed 's/^[vV]//; s/[-+].*$//')" 'BEGIN {
    na = split(a, x, "."); nb = split(b, y, ".");
    n = (na > nb) ? na : nb;
    for (i = 1; i <= n; i++) {
      va = x[i] + 0; vb = y[i] + 0;
      if (va < vb) exit 0;
      if (va > vb) exit 1;
    }
    exit 1;
  }'
}

# 交互式确认：优先从控制终端读取，兼容 `curl | sh` 管道安装时 stdin 被脚本占用的情况；
# 无控制终端的非交互环境默认不更新（返回失败），需要强制覆盖时请使用 --force。
ask_confirm() {
  _ans=""
  if [ -r /dev/tty ]; then
    # /dev/tty 可能因无控制终端而 open 失败：视为非交互，默认不更新。
    # 2>/dev/null 必须置于 /dev/tty 重定向之前，才能压掉 shell 的重定向失败报错。
    if printf '%s' "$1" 2>/dev/null > /dev/tty && read -r _ans 2>/dev/null </dev/tty; then
      :
    else
      echo "$1非交互环境，已跳过更新（如需强制覆盖安装请使用 --force）" >&2
      return 1
    fi
  elif [ -t 0 ]; then
    printf '%s' "$1"
    read -r _ans || _ans=""
  else
    echo "$1非交互环境，已跳过更新（如需强制覆盖安装请使用 --force）" >&2
    return 1
  fi
  case "$_ans" in
    ""|y|Y|yes|YES) return 0 ;;
    *) return 1 ;;
  esac
}

installed_bin=""
if command -v cyber >/dev/null 2>&1; then
  installed_bin="$(command -v cyber)"
elif [ -x "$INSTALL_DIR/cyber" ]; then
  installed_bin="$INSTALL_DIR/cyber"
fi

if [ -n "$installed_bin" ]; then
  installed_version="$("$installed_bin" --version 2>/dev/null | head -n 1 | awk '{ print $NF }' || true)"
  installed_norm="$(printf '%s' "$installed_version" | sed 's/^[vV]//; s/[-+].*$//')"
  target_norm="$(printf '%s' "$VERSION" | sed 's/^[vV]//; s/[-+].*$//')"
  echo "→ 检测到已安装的 cyber: $installed_bin"
  echo "  已安装版本: ${installed_version:-未知}"
  echo "  云端最新版本: $VERSION"
  if [ "$FORCE" = "1" ]; then
    echo "→ --force 已启用：跳过版本检查与询问，直接下载并覆盖安装。"
  elif [ -z "$installed_version" ]; then
    ask_confirm "无法确定已安装版本，是否覆盖安装 $VERSION？[Y/n]: " \
      || { echo "已取消更新。"; exit 0; }
  elif [ "$installed_norm" = "$target_norm" ]; then
    echo "✓ 已是最新版本（$installed_version），无需更新。"
    echo "  如需强制覆盖安装，请运行: cyber update --force"
    exit 0
  elif ver_lt "$installed_version" "$VERSION"; then
    ask_confirm "是否更新到最新版本 $VERSION？[Y/n]: " \
      || { echo "已取消更新。"; exit 0; }
  else
    ask_confirm "已安装版本 $installed_version 高于目标版本 $VERSION，是否覆盖安装？[Y/n]: " \
      || { echo "已取消更新。"; exit 0; }
  fi
else
  echo "→ 未检测到已安装的 cyber，将进行全新安装。"
fi

# ─── 下载源候选列表与多源测速 ──────────────────────────────────────────────
github_base="https://github.com/$REPO/releases/download/$VERSION"
cnb_base="https://cnb.cool/$CNB_REPO/-/releases/download/$VERSION"
candidates=""
if [ -n "${CYBER_DOWNLOAD_MIRROR:-}" ]; then
  candidates="自定义镜像|${CYBER_DOWNLOAD_MIRROR%/}/$github_base"
fi
if [ "$USE_CNB" = "1" ]; then
  candidates="$candidates CNB源(国内极速)|$cnb_base GitHub官方源|$github_base gh-proxy.com镜像|https://gh-proxy.com/$github_base ghfast.top镜像|https://ghfast.top/$github_base ghproxy.net镜像|https://ghproxy.net/$github_base"
else
  candidates="$candidates CNB源(国内极速)|$cnb_base GitHub官方源|$github_base gh-proxy.com镜像|https://gh-proxy.com/$github_base ghfast.top镜像|https://ghfast.top/$github_base ghproxy.net镜像|https://ghproxy.net/$github_base"
fi
echo "→ 安装 cyber $VERSION ($target) 到 $INSTALL_DIR"

# ─── 创建临时目录 ─────────────────────────────────────────────────────────
tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t cyber-install)"
trap 'rm -rf "$tmpdir"' 0
trap 'exit 130' INT
trap 'exit 143' TERM

# ─── 测速各下载源并选择最优节点 ────────────────────────────────────────────
echo "→ 测速各下载源并选择最优节点…"
speedtest_results=""
for item in $candidates; do
  [ -z "$item" ] && continue
  name="${item%%|*}"
  base="${item#*|}"
  sha_url="$base/$archive.sha256"
  time_sec=$(curl -fsSL --connect-timeout 4 --max-time 6 -w "%{time_total}" -o "$tmpdir/speedtest.sha256" "$sha_url" 2>/dev/null || true)
  if [ -n "$time_sec" ] && [ -s "$tmpdir/speedtest.sha256" ]; then
    hash_val=$(awk 'NR == 1 { print $1 }' "$tmpdir/speedtest.sha256" 2>/dev/null || true)
    case "$hash_val" in
      [0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]*)
        if [ "${#hash_val}" -ge 64 ]; then
          printf "   [✓] %-18s : %.2fs\n" "$name" "$time_sec"
          speedtest_results="${speedtest_results}${time_sec} ${base} ${name}\n"
          continue
        fi
        ;;
    esac
  fi
  printf "   [✗] %-18s : 超时/不可达\n" "$name"
done

download_base_list=""
if [ -n "$speedtest_results" ]; then
  download_base_list=$(printf "$speedtest_results" | sort -n | awk '{ print $2 }')
  fastest_sec=$(printf "$speedtest_results" | sort -n | head -n1 | awk '{ print $1 }')
  fastest_name=$(printf "$speedtest_results" | sort -n | head -n1 | awk '{ print $3 }')
  printf "→ 选用最优源: %s (%.2fs)\n" "$fastest_name" "$fastest_sec"
else
  echo "   所有源测速未响应，回退至默认源重试…"
  for item in $candidates; do
    [ -z "$item" ] && continue
    base="${item#*|}"
    download_base_list="$download_base_list $base"
  done
fi

# ─── 下载（最优源失败自动回退备用镜像）────────────────────────────────────
downloaded=0
for base in $download_base_list; do
  echo "→ 下载 $base/$archive"
  if curl -fsSL --connect-timeout 15 --max-time 300 -o "$tmpdir/$archive" "$base/$archive" \
     && curl -fsSL --connect-timeout 15 --max-time 60 -o "$tmpdir/$archive.sha256" "$base/$archive.sha256"; then
    downloaded=1
    break
  fi
  echo "   该源下载失败，尝试备用源…" >&2
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
