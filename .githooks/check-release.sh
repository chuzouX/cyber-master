#!/usr/bin/env sh
# ==============================================================================
# scripts/check-release.sh
# 
# 本地发布预检脚本 (Release Pre-Flight Verification)
# 对应 GitHub Actions: .github/workflows/release.yml
#
# 用法:
#   sh scripts/check-release.sh [tag_name]
# 示例:
#   sh scripts/check-release.sh v0.5.0
#   sh scripts/check-release.sh
# ==============================================================================

set -e

# 颜色控制
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    CYAN=''
    BOLD=''
    NC=''
fi

echo "${BOLD}${CYAN}=================================================================${NC}"
echo "${BOLD}${CYAN}  Cyber Master - 发布前置检查 (Release Pre-Flight Verification)   ${NC}"
echo "${BOLD}${CYAN}=================================================================${NC}"

TARGET_TAG="${1:-}"

# 1. 解析 Cargo.toml 中的工作区版本号
echo ""
echo "${BLUE}[1/8] 检查 Cargo.toml 版本号配置...${NC}"
if [ ! -f "Cargo.toml" ]; then
    echo "${RED}[ERROR] 当前目录下未找到 Cargo.toml，请在项目根目录运行。${NC}"
    exit 1
fi

WORKSPACE_VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml | grep -E '^version[[:space:]]*=' | head -n 1 | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "$WORKSPACE_VERSION" ]; then
    echo "${RED}[ERROR] 无法从 Cargo.toml 的 [workspace.package] 中解析到 version 字段！${NC}"
    exit 1
fi
echo "${GREEN}✓ 检出工作区版本号: ${BOLD}${WORKSPACE_VERSION}${NC}"

# 2. 校验 Tag 与 Cargo.toml 版本一致性
echo ""
echo "${BLUE}[2/8] 校验 Tag 与版本号匹配...${NC}"
if [ -n "$TARGET_TAG" ]; then
    NORM_TAG=$(echo "$TARGET_TAG" | sed -E 's/^[vV]//')
    if [ "$NORM_TAG" != "$WORKSPACE_VERSION" ]; then
        echo "${RED}[ERROR] 发布 Tag (${TARGET_TAG}) 与 Cargo.toml 工作区版本号 (${WORKSPACE_VERSION}) 不匹配！${NC}"
        echo "${YELLOW}GitHub Actions release 工作流将以此 Tag 打包，版本不一致会导致二进制版本标识错误。${NC}"
        echo "${YELLOW}请统一版本号后再行发布：${NC}"
        echo "  - 修改 Cargo.toml 版本为与 Tag 一致，或"
        echo "  - 创建匹配当前版本的 Tag: v${WORKSPACE_VERSION}"
        exit 1
    fi
    echo "${GREEN}✓ Tag ${TARGET_TAG} 与 Cargo.toml 版本 ${WORKSPACE_VERSION} 匹配${NC}"
else
    echo "${YELLOW}ℹ 未指定 Tag，将基于 Cargo.toml 当前版本 (v${WORKSPACE_VERSION}) 验证${NC}"
fi

# 3. 校验 CHANGELOG.md 日志记录
echo ""
echo "${BLUE}[3/8] 检查 CHANGELOG.md 更新记录...${NC}"
if [ ! -f "CHANGELOG.md" ]; then
    echo "${RED}[ERROR] 未找到 CHANGELOG.md 文件！${NC}"
    exit 1
fi

if grep -E "^## \[[vV]?${WORKSPACE_VERSION}\]" CHANGELOG.md >/dev/null 2>&1; then
    echo "${GREEN}✓ CHANGELOG.md 已包含版本 ${WORKSPACE_VERSION} 的发版记录${NC}"
else
    echo "${RED}[ERROR] CHANGELOG.md 中未找到版本 [${WORKSPACE_VERSION}] 的记录！${NC}"
    echo "${YELLOW}请在 CHANGELOG.md 顶部新增 '## [${WORKSPACE_VERSION}] - YYYY-MM-DD' 章节后再发布。${NC}"
    exit 1
fi

# 4. 检查工作区代码与依赖锁定状态
echo ""
echo "${BLUE}[4/8] 检查 Cargo.toml 与 Cargo.lock 提交状态...${NC}"
MODIFIED_CARGO=$(git status --porcelain Cargo.toml Cargo.lock 2>/dev/null || true)
if [ -n "$MODIFIED_CARGO" ]; then
    echo "${RED}[ERROR] 检测到 Cargo.toml 或 Cargo.lock 存在未提交修改：${NC}"
    echo "$MODIFIED_CARGO"
    echo "${YELLOW}GitHub Actions 使用锁定的 git ref 检出代码，未提交的配置/锁文件会导致远程构建失败！${NC}"
    echo "${YELLOW}请执行 git add / commit 后再发布。${NC}"
    exit 1
fi
echo "${GREEN}✓ Cargo.toml 与 Cargo.lock 已处于干净提交状态${NC}"

# 5. 安装脚本版本解析（install.sh / cyber update 共用的 release JSON 解析）
echo ""
echo "${BLUE}[5/8] 检查 install.sh 版本解析（minified release JSON 回归）...${NC}"
if sh "$(dirname -- "$0")/check-install.sh"; then
    echo "${GREEN}✓ install.sh 版本解析检查通过${NC}"
else
    echo "${RED}[ERROR] install.sh 版本解析检查失败！${NC}"
    echo "${YELLOW}用户经 curl | sh 安装与 cyber update 都会受此影响，请先修复再发布。${NC}"
    exit 1
fi

# Windows 平台安全清理：杀死可能锁定 cyber.exe 的残留进程
if command -v taskkill >/dev/null 2>&1; then
    taskkill //F //IM cyber.exe >/dev/null 2>&1 || true
fi

# 5. 代码格式检查 (对应 GHA: Check formatting)
echo ""
echo "${BLUE}[6/8] 执行代码格式校验 (cargo fmt --all -- --check)...${NC}"
if cargo fmt --all -- --check; then
    echo "${GREEN}✓ 代码格式符合规范${NC}"
else
    echo "${RED}[ERROR] 代码格式检查失败！${NC}"
    echo "${YELLOW}请运行 'cargo fmt --all' 格式化代码后再提交。${NC}"
    exit 1
fi

# 6. Clippy 静态检查 (对应 GHA: Clippy)
echo ""
echo "${BLUE}[7/8] 执行 Clippy 静态分析 (cargo clippy --workspace --all-targets --locked -- -D warnings)...${NC}"
if cargo clippy --workspace --all-targets --locked -- -D warnings; then
    echo "${GREEN}✓ Clippy 检查通过，无警告${NC}"
else
    echo "${RED}[ERROR] Clippy 静态检查发现告警！GitHub Actions 将视告警为失败。${NC}"
    exit 1
fi

# 7. 全量单元/集成测试 (对应 GHA: Test workspace)
echo ""
echo "${BLUE}[8/8] 执行全量测试套件 (cargo test --workspace --locked)...${NC}"
if cargo test --workspace --locked; then
    echo "${GREEN}✓ 全量工作区测试通过${NC}"
else
    echo "${RED}[ERROR] 单元测试或集成测试未通过！${NC}"
    exit 1
fi
echo ""
echo "${BOLD}${GREEN}=================================================================${NC}"
echo "${BOLD}${GREEN}  ✓ 所有发布预检项全部通过！可以安全推送到远程触发 GitHub Actions  ${NC}"
echo "${BOLD}${GREEN}=================================================================${NC}"
exit 0
