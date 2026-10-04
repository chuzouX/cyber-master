#!/usr/bin/env sh
# ==============================================================================
# .githooks/install.sh
# 
# 一键安装与配置本地 Git Hooks
# ==============================================================================

set -e

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT"

echo "正在配置 Git Hooks 路径为 .githooks ..."
git config core.hooksPath .githooks

# 同时在 .git/hooks/ 建立备份，确保各种 git 客户端环境兼容
if [ -d ".git/hooks" ]; then
    cp .githooks/pre-push .git/hooks/pre-push 2>/dev/null || true
    chmod +x .git/hooks/pre-push 2>/dev/null || true
fi
chmod +x .githooks/pre-push 2>/dev/null || true
chmod +x .githooks/check-release.sh 2>/dev/null || true

echo "✓ Git pre-push hook 安装成功！"
echo "  - 配置文件: .githooks/pre-push"
echo "  - 检查脚本: .githooks/check-release.sh"
echo "  - 生效机制: 当执行 git push 包含 v* 标签或设置 CYBER_RELEASE_CHECK=1 时自动执行预检"
