﻿﻿﻿﻿﻿# Cyber Master 一键安装脚本（Windows / PowerShell）
#
# 用法（PowerShell 5.1+ / PowerShell 7+）：
#   irm https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.ps1 | iex
#
# 或先下载再执行（适用于 ExecutionPolicy 受限环境）：
#   powershell -ExecutionPolicy Bypass -File install.ps1
#
# 高级用法：
#   $CYBER_VERSION='v0.1.0'; irm https://raw.githubusercontent.com/.../install.ps1 | iex
#   irm https://raw.githubusercontent.com/.../install.ps1 | iex  # 默认装到 %USERPROFILE%\.local\bin
#
# 环境变量覆盖：
#   $env:CYBER_VERSION       指定版本 tag，如 'v0.1.0'
#   $env:CYBER_INSTALL_DIR   安装目录，默认 $env:USERPROFILE\.local\bin
#   $env:CYBER_REPO          GitHub owner/name，默认 chuzouX/cyber-master

#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = '',
    [string]$InstallDir = '',
    [string]$Repo = 'chuzouX/cyber-master'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'   # 关闭 Invoke-WebRequest 的进度条，否则慢且管道场景下报错

# ─── 合并环境变量 ──────────────────────────────────────────────────────────
if (-not $Version)    { $Version    = $env:CYBER_VERSION }
if (-not $InstallDir) { $InstallDir = $env:CYBER_INSTALL_DIR }
if (-not $InstallDir) { $InstallDir = Join-Path $env:USERPROFILE '.local\bin' }
if ($env:CYBER_REPO)  { $Repo       = $env:CYBER_REPO }

# ─── 平台检测（PowerShell 只支持 Windows 二进制；WSL 用户请用 install.sh）──
$Target = 'x86_64-pc-windows-msvc'
$Archive = "cyber-$Target.zip"

# ─── 解析版本（未指定时取 latest）──────────────────────────────────────────
if (-not $Version) {
    Write-Host "→ 查询最新版本…" -ForegroundColor Cyan
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
                                     -Headers @{ 'User-Agent' = 'cyber-installer' }
        $Version = $release.tag_name
    } catch {
        Write-Error "无法获取最新版本：$_`n请用 -Version <tag> 或 `$env:CYBER_VERSION 显式指定。"
        exit 1
    }
    if (-not $Version) {
        Write-Error "latest release 返回空 tag_name"
        exit 1
    }
}

$DownloadUrl = "https://github.com/$Repo/releases/download/$Version/$Archive"
$ChecksumUrl = "$DownloadUrl.sha256"

Write-Host "→ 安装 cyber $Version ($Target) 到 $InstallDir" -ForegroundColor Cyan

# ─── 下载 ────────────────────────────────────────────────────────────────
Write-Host "→ 下载 $DownloadUrl"
$tmp = New-TemporaryFile
try {
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $tmp.FullName -UseBasicParsing
} catch {
    Write-Error "下载失败：$_"
    exit 1
}

# ─── 校验 SHA256（可选：.sha256 不存在则跳过）──────────────────────────────
$shaFile = "$($tmp.FullName).sha256"
try {
    Invoke-WebRequest -Uri $ChecksumUrl -OutFile $shaFile -UseBasicParsing
    Write-Host "→ 校验 SHA256…"
    $expected = (Get-Content $shaFile -TotalCount 1).Split(' ')[0].Trim().ToLowerInvariant()
    $actual = (Get-FileHash $tmp.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        Remove-Item $tmp.FullName, $shaFile -Force -ErrorAction SilentlyContinue
        Write-Error "SHA256 校验失败：expected=$expected actual=$actual"
        exit 1
    }
} catch {
    Write-Host "  (未找到 .sha256 校验文件，跳过校验)" -ForegroundColor DarkGray
} finally {
    Remove-Item $shaFile -Force -ErrorAction SilentlyContinue
}

# ─── 创建安装目录 ─────────────────────────────────────────────────────────
if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# ─── 解压 + 安装 ──────────────────────────────────────────────────────────
$extractPath = Join-Path $env:TEMP "cyber-install-$(Get-Random -Maximum 2147483647)"
try {
    Expand-Archive -Path $tmp.FullName -DestinationPath $extractPath -Force
} catch {
    Write-Error "解压失败：$_"
    exit 1
} finally {
    Remove-Item $tmp.FullName -Force -ErrorAction SilentlyContinue
}

$srcBinary = Join-Path $extractPath 'cyber.exe'
$destBinary = Join-Path $InstallDir 'cyber.exe'

if (-not (Test-Path $srcBinary)) {
    Remove-Item -Recurse -Force $extractPath -ErrorAction SilentlyContinue
    Write-Error "压缩包内未找到 cyber.exe"
    exit 1
}

# Move-Item 在目标被占用时会失败；用 .NET Move 兼容覆盖
if (Test-Path $destBinary) {
    try {
        Remove-Item $destBinary -Force
    } catch {
        # 文件可能被正在运行的进程占用，重命名后下次启动自动失效
        $stale = "$destBinary.old.$(Get-Date -Format yyyyMMddHHmmss)"
        Move-Item $destBinary $stale -Force
    }
}

Move-Item -Path $srcBinary -Destination $destBinary -Force
Remove-Item -Recurse -Force $extractPath -ErrorAction SilentlyContinue

# ─── 添加到用户 PATH ──────────────────────────────────────────────────────
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
$pathItems = $userPath -split ';' | Where-Object { $_ -ne '' }
$alreadyInPath = $false
foreach ($item in $pathItems) {
    if ($item -and (Test-Path $item) `
        -and ((Resolve-Path $item).Path -eq (Resolve-Path $InstallDir).Path)) {
        $alreadyInPath = $true
        break
    }
}

if (-not $alreadyInPath) {
    $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    # 同步当前进程的 PATH，便于在同一会话内立即试用
    $env:Path = "$env:Path;$InstallDir"
    Write-Host "✓ 已将 $InstallDir 添加到用户 PATH" -ForegroundColor Green
    Write-Host "  (新开终端后生效；当前终端已临时加入 PATH)" -ForegroundColor DarkGray
} else {
    Write-Host "✓ $InstallDir 已在 PATH 中" -ForegroundColor DarkGray
}

# ─── 完成提示 ────────────────────────────────────────────────────────────
Write-Host ""
Write-Host "✓ 已安装: $destBinary" -ForegroundColor Green
Write-Host "  运行: cyber" -ForegroundColor Green
Write-Host ""
Write-Host "首次运行 cyber 会自动在 $env:USERPROFILE\.cyber\ 创建配置目录。" -ForegroundColor DarkGray
Write-Host "文档: https://github.com/$Repo#readme" -ForegroundColor DarkGray
