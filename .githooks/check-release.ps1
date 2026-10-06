<#
.SYNOPSIS
  Cyber Master 本地发布预检脚本 (Release Pre-Flight Verification)
  对应 GitHub Actions: .github/workflows/release.yml

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts/check-release.ps1 -Tag "v0.5.0"
  powershell -ExecutionPolicy Bypass -File scripts/check-release.ps1
#>

param (
    [string]$Tag = ""
)

$ErrorActionPreference = "Stop"

Write-Host "=================================================================" -ForegroundColor Cyan
Write-Host "  Cyber Master - 发布前置检查 (Release Pre-Flight Verification)   " -ForegroundColor Cyan
Write-Host "=================================================================" -ForegroundColor Cyan

# 1. 解析 Cargo.toml
Write-Host "`n[1/9] 检查 Cargo.toml 版本号配置..." -ForegroundColor Blue
if (-not (Test-Path "Cargo.toml")) {
    Write-Host "[ERROR] 当前目录下未找到 Cargo.toml，请在项目根目录运行。" -ForegroundColor Red
    exit 1
}

$cargoLines = Get-Content "Cargo.toml"
$workspaceVersion = ""
$inWorkspacePackage = $false

foreach ($line in $cargoLines) {
    if ($line -match '^\s*\[workspace\.package\]') {
        $inWorkspacePackage = $true
        continue
    }
    if ($inWorkspacePackage -and ($line -match '^\s*\[')) {
        break
    }
    if ($inWorkspacePackage -and ($line -match '^\s*version\s*=\s*"([^"]+)"')) {
        $workspaceVersion = $matches[1]
        break
    }
}

if ([string]::IsNullOrWhiteSpace($workspaceVersion)) {
    Write-Host "[ERROR] 无法从 Cargo.toml 的 [workspace.package] 中解析到 version 字段！" -ForegroundColor Red
    exit 1
}
Write-Host "✓ 检出工作区版本号: $workspaceVersion" -ForegroundColor Green

# 2. 校验 Tag
Write-Host "`n[2/9] 校验 Tag 与版本号匹配..." -ForegroundColor Blue
if ($Tag -ne "") {
    $normTag = $Tag.TrimStart("v").TrimStart("V")
    if ($normTag -ne $workspaceVersion) {
        Write-Host "[ERROR] 发布 Tag ($Tag) 与 Cargo.toml 工作区版本号 ($workspaceVersion) 不匹配！" -ForegroundColor Red
        Write-Host "GitHub Actions release 工作流将以此 Tag 打包，版本不一致会导致二进制版本标识错误。" -ForegroundColor Yellow
        exit 1
    }
    Write-Host "✓ Tag $Tag 与 Cargo.toml 版本 $workspaceVersion 匹配" -ForegroundColor Green
} else {
    Write-Host "ℹ 未指定 Tag，将基于 Cargo.toml 当前版本 (v$workspaceVersion) 验证" -ForegroundColor Yellow
}

# 3. 校验 CHANGELOG.md
Write-Host "`n[3/9] 检查 CHANGELOG.md 更新记录..." -ForegroundColor Blue
if (-not (Test-Path "CHANGELOG.md")) {
    Write-Host "[ERROR] 未找到 CHANGELOG.md 文件！" -ForegroundColor Red
    exit 1
}
$changelogContent = Get-Content "CHANGELOG.md" -Raw
$escapedVer = [regex]::Escape($workspaceVersion)
if (-not ($changelogContent -match "(?m)^## \[[vV]?$escapedVer\]")) {
    Write-Host "[ERROR] CHANGELOG.md 中未找到版本 [$workspaceVersion] 的记录！" -ForegroundColor Red
    Write-Host "请在 CHANGELOG.md 顶部新增 '## [$workspaceVersion] - YYYY-MM-DD' 章节后再发布。" -ForegroundColor Yellow
    exit 1
}
Write-Host "✓ CHANGELOG.md 已包含版本 $workspaceVersion 的发版记录" -ForegroundColor Green

# 4. 检查未提交文件
Write-Host "`n[4/9] 检查 Cargo.toml 与 Cargo.lock 提交状态..." -ForegroundColor Blue
$diffCargo = git status --porcelain Cargo.toml Cargo.lock 2>$null
if ($diffCargo) {
    Write-Host "[ERROR] 检测到 Cargo.toml 或 Cargo.lock 存在未提交修改：" -ForegroundColor Red
    Write-Host $diffCargo
    Write-Host "GitHub Actions 使用锁定的 git ref 检出代码，未提交的配置/锁文件会导致远程构建失败！" -ForegroundColor Yellow
    exit 1
}
Write-Host "✓ Cargo.toml 与 Cargo.lock 已处于干净提交状态" -ForegroundColor Green

# 清理占用
Get-Process "cyber" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

# 5. install.sh 版本解析（离线回归检查）

Write-Host "`n[5/9] 检查 install.sh 版本解析（minified release JSON 回归）..." -ForegroundColor Blue

$shPath = Get-Command sh -ErrorAction SilentlyContinue

if ($shPath) {

    & sh .githooks/check-install.sh

    if ($LASTEXITCODE -ne 0) {

        Write-Host "[ERROR] install.sh 版本解析检查失败！用户 curl | sh 安装与 cyber update 均受影响。" -ForegroundColor Red

        exit 1

    }

    Write-Host "✓ install.sh 版本解析检查通过" -ForegroundColor Green

} else {

    Write-Host "ℹ 未找到 sh（Git for Windows 提供），跳过 install.sh 解析检查。" -ForegroundColor Yellow

}

# 6. 代码格式
Write-Host "`n[6/9] 执行代码格式校验 (cargo fmt --all -- --check)..." -ForegroundColor Blue
& cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) {
    Write-Host "[ERROR] 代码格式检查失败！请运行 'cargo fmt --all' 格式化代码后再提交。" -ForegroundColor Red
    exit 1
}
Write-Host "✓ 代码格式符合规范" -ForegroundColor Green

# 6. Clippy
Write-Host "`n[7/9] 执行 Clippy 静态分析 (cargo clippy --workspace --all-targets --locked -- -D warnings)..." -ForegroundColor Blue
& cargo clippy --workspace --all-targets --locked -- -D warnings
if ($LASTEXITCODE -ne 0) {
    Write-Host "[ERROR] Clippy 静态检查发现告警！" -ForegroundColor Red
    exit 1
}
Write-Host "✓ Clippy 检查通过，无警告" -ForegroundColor Green

# 7. 测试
Write-Host "`n[8/9] 执行全量测试套件 (cargo test --workspace --locked)..." -ForegroundColor Blue
& cargo test --workspace --locked
if ($LASTEXITCODE -ne 0) {
    Write-Host "[ERROR] 单元测试或集成测试未通过！" -ForegroundColor Red
    exit 1
}
Write-Host "✓ 全量工作区测试通过" -ForegroundColor Green

# 8. Release 构建
Write-Host "`n[9/9] 验证 Release 模式编译 (cargo build --release --locked -p cyber-app)..." -ForegroundColor Blue
& cargo build --release --locked -p cyber-app
if ($LASTEXITCODE -ne 0) {
    Write-Host "[ERROR] Release 编译失败！" -ForegroundColor Red
    exit 1
}
Write-Host "✓ Release 编译成功" -ForegroundColor Green

Write-Host "`n=================================================================" -ForegroundColor Green
Write-Host "  ✓ 所有发布预检项全部通过！可以安全推送到远程触发 GitHub Actions  " -ForegroundColor Green
Write-Host "=================================================================" -ForegroundColor Green
exit 0
