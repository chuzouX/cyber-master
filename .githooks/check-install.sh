#!/usr/bin/env sh
# ==============================================================================
# .githooks/check-install.sh
#
# install.sh 版本解析回归检查（离线、无网络、无 tty 依赖）。
#
# 背景缺陷：release 接口可能返回单行（minified）JSON，此时
#   grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/'
# 的贪婪匹配会吃到最后一段被引号包裹的文本 —— 即 `"body"`（发布说明）的尾部，
# 于是整段 CHANGELOG 被当作版本号打印、比较并拼进下载 URL。
#
# 本脚本用 curl/uname/getconf 替身喂入合成 release JSON 与合成安装包，
# 端到端跑一遍 install.sh 的「解析 → 比较 → 下载 → 校验 → 安装」路径：
#   1) minified JSON（且 body 里埋了诱饵 tag）→ 必须只认 v0.9.9；
#   2) pretty JSON + 已装同版本 → 必须提示「已是最新版本」且不发起下载；
#   3) 非法 --version → 必须快速失败（不得拿脏值拼 URL）。
#
# 用法: sh .githooks/check-install.sh [install.sh 路径]
# ==============================================================================
set -eu

if [ -t 1 ]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; NC=''
fi

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
INSTALLER="${1:-$ROOT/install.sh}"
[ -f "$INSTALLER" ] || { echo "${RED}[ERROR] 未找到 install.sh: $INSTALLER${NC}" >&2; exit 1; }

TAG='v0.9.9'
DECOY='v0.0.1'
BODY_MARKER='CHANGELOG_BODY_MARKER'
fail=0
note() { printf '%s\n' "$*"; }
ok()   { printf '%s\n' "  ${GREEN}✓${NC} $*"; }
bad()  { printf '%s\n' "  ${RED}✗${NC} $*"; fail=1; }
expect_contains() { # file needle desc
    if grep -qF -- "$2" "$1"; then ok "$3"; else bad "$3（缺少: $2）"; note "--- 实际输出 ---"; cat "$1"; note "--------------"; fi
}
expect_absent() { # file needle desc
    if grep -qF -- "$2" "$1"; then bad "$3（不应出现: $2）"; note "--- 实际输出 ---"; cat "$1"; note "--------------"; else ok "$3"; fi
}

work=$(mktemp -d 2>/dev/null || mktemp -d -t cyber-check) || exit 1
trap 'rm -rf "$work"' 0 1 2 3 15
mkdir -p "$work/bin" "$work/payload" "$work/home" "$work/install" "$work/arch"

# ─── 替身：uname / getconf（让任意宿主（含 Git-Bash）走 Linux x86_64 分支）────
cat >"$work/bin/uname" <<'SH'
#!/bin/sh
case "${1:-}" in
  -s) echo Linux ;;
  -m) echo x86_64 ;;
  *)  echo Linux ;;
esac
SH
cat >"$work/bin/getconf" <<'SH'
#!/bin/sh
echo "glibc 2.39"
SH
chmod +x "$work/bin/uname" "$work/bin/getconf"

# ─── 合成 release 响应（minified 单行 + body 内诱饵 tag）────────────────────
# body 里出现的 `"tag_name": "v0.0.1"` 在 JSON 中被转义为 \"tag_name\": ... ；
# 无论是否转义，都不能被当成真实 tag。
minified="$work/payload/min.json"
printf '%s' "{\"tag_name\": \"$TAG\", \"body\": \"$BODY_MARKER 变更与调整 (Changed)\\\\n\\\\n- 正文提及 \\\\\"tag_name\\\\\": \\\\\"$DECOY\\\\\" 与 \\\\\"其它引号\\\\\" 混杂，用于验证贪婪匹配不会吃到 body。\", \"assets\": [{\"name\": \"cyber-x86_64-unknown-linux-gnu.tar.gz\"}]}" >"$minified"
pretty="$work/payload/pretty.json"
cat >"$pretty" <<EOF
{
  "tag_name": "$TAG",
  "name": "$TAG",
  "body": "$BODY_MARKER 正文诱饵 \\"tag_name\\": \\"$DECOY\\"",
  "draft": false
}
EOF

# ─── 合成安装包（内含假 cyber，--version 输出目标版本）──────────────────────
target="x86_64-unknown-linux-gnu"
archive="cyber-$target.tar.gz"
printf '#!/bin/sh\n[ "${1:-}" = "--version" ] && echo "cyber %s"\nexit 0\n' "$TAG" >"$work/arch/cyber"
chmod +x "$work/arch/cyber"
tar -czf "$work/payload/$archive" -C "$work/arch" cyber

if command -v sha256sum >/dev/null 2>&1; then
    sha=$(sha256sum "$work/payload/$archive" | awk '{ print $1 }')
else
    sha=$(shasum -a 256 "$work/payload/$archive" | awk '{ print $1 }')
fi
printf '%s  %s\n' "$sha" "$archive" >"$work/payload/$archive.sha256"

# ─── curl 替身：按 URL 分类返回合成响应，并记录全部请求 ─────────────────────
cat >"$work/bin/curl" <<'SH'
#!/bin/sh
out=''; wr=''; url=''
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -w) wr="$2"; shift 2 ;;
    --connect-timeout|--max-time) shift 2 ;;
    -f|-s|-S|-L|-fsSL|-fsl) shift ;;
    *) url="$1"; shift ;;
  esac
done
[ -n "$url" ] && printf '%s\n' "$url" >>"$CURL_LOG"
case "$url" in
  *api.cnb.cool*) exit 22 ;;
  *releases/latest*) src="$CURL_PAYLOAD" ;;
  *.sha256) src="$CURL_DIR/$CURL_ARCHIVE.sha256" ;;
  *.tar.gz) src="$CURL_DIR/$CURL_ARCHIVE" ;;
  *) exit 22 ;;
esac
if [ -n "$out" ]; then cat "$src" >"$out"; else cat "$src"; fi
[ -n "$wr" ] && printf '0.01'
exit 0
SH
chmod +x "$work/bin/curl"

run_installer() { # payload_file installed_version out_file [extra args...]
    _payload="$1"; _installed="$2"; _out="$3"; shift 3
    printf '#!/bin/sh\n[ "${1:-}" = "--version" ] && echo "cyber %s"\nexit 0\n' "$_installed" >"$work/bin/cyber"
    chmod +x "$work/bin/cyber"
    : >"$work/curl.log"
    PATH="$work/bin:$PATH" \
    HOME="$work/home" \
    SHELL=/bin/sh \
    CURL_LOG="$work/curl.log" CURL_PAYLOAD="$_payload" CURL_DIR="$work/payload" CURL_ARCHIVE="$archive" \
    sh "$INSTALLER" --install-dir "$work/install" "$@" >"$_out" 2>&1 </dev/null || true
}

# ─── 用例 1：minified JSON（历史缺陷场景）→ 必须只认 v0.9.9 ─────────────────
note ""
note "[1/3] minified release JSON + 已装 cyber 0.4.2（--force 走完整安装链路）"
run_installer "$minified" '0.4.2' "$work/out1.txt" --force
expect_contains "$work/out1.txt" "云端最新版本: $TAG" "打印的云端版本是 $TAG"
expect_absent   "$work/out1.txt" "$BODY_MARKER" "输出不含发布说明正文"
expect_absent   "$work/out1.txt" "$DECOY" "输出不含正文诱饵 tag $DECOY"
expect_contains "$work/curl.log" "/download/$TAG/$archive" "下载 URL 使用 $TAG"
expect_absent   "$work/curl.log" "$DECOY" "下载 URL 不含诱饵 tag"
if [ -x "$work/install/cyber" ] && [ "$("$work/install/cyber" --version)" = "cyber $TAG" ]; then
    ok "已安装 $TAG 的二进制"
else
    bad "安装结果为 $TAG 的二进制"
fi

# ─── 用例 2：pretty JSON + 已装同版本 → 提示已是最新且不下载 ────────────────
note ""
note "[2/3] pretty release JSON + 已装 cyber $TAG"
run_installer "$pretty" '0.9.9' "$work/out2.txt"
expect_contains "$work/out2.txt" "云端最新版本: $TAG" "打印的云端版本是 $TAG"
expect_contains "$work/out2.txt" "已是最新版本" "识别为已是最新"
expect_absent   "$work/out2.txt" "$BODY_MARKER" "输出不含发布说明正文"
expect_absent   "$work/curl.log" ".tar.gz" "已是最新时不发起下载"

# ─── 用例 3：非法 --version → 快速失败，不得拼脏 URL ────────────────────────
note ""
note "[3/3] 非法 --version 必须快速失败"
PATH="$work/bin:$PATH" HOME="$work/home" SHELL=/bin/sh sh "$INSTALLER" --version 'not-a-version' --install-dir "$work/install" >"$work/out3.txt" 2>&1 </dev/null \
    && bad "非法版本号未被拒绝" || ok "非法版本号被拒绝（退出码非 0）"
expect_contains "$work/out3.txt" "版本号无效" "给出明确错误信息"

note ""
if [ "$fail" -eq 0 ]; then
    printf '%s\n' "${GREEN}✓ install.sh 版本解析检查通过${NC}"
    exit 0
fi
printf '%s\n' "${RED}[ERROR] install.sh 版本解析检查失败${NC}" >&2
exit 1
