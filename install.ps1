# Cyber Master 一键安装脚本（Windows / PowerShell）
#
# 用法（PowerShell 5.1+ / PowerShell 7+）：
#   irm https://raw.githubusercontent.com/chuzouX/cyber-master/main/install.ps1 | iex
#
# 或先下载再执行（适用于 ExecutionPolicy 受限环境）：
#   powershell -ExecutionPolicy Bypass -File install.ps1
#
# 高级用法：
#   $env:CYBER_VERSION='v0.1.0'; irm https://raw.githubusercontent.com/.../install.ps1 | iex
#   irm https://raw.githubusercontent.com/.../install.ps1 | iex  # 默认装到 %USERPROFILE%\.local\bin
#
# 环境变量覆盖：
#   $env:CYBER_VERSION       指定版本 tag，如 'v0.1.0'
#   $env:CYBER_INSTALL_DIR   安装目录，默认 $env:USERPROFILE\.local\bin
#   $env:CYBER_REPO          GitHub owner/name，默认 chuzouX/cyber-master
#   $env:CYBER_DOWNLOAD_MIRROR 下载镜像前缀（默认自动尝试 ghproxy.net/gh-proxy.com/ghfast.top）

#Requires -Version 5.1

# 注意：`irm <url> | iex` 以表达式模式解析脚本，不能使用 param() 块；
# 安装选项统一通过环境变量传入（见下方合并逻辑）。
$Version    = ''
$InstallDir = ''
$Repo       = 'chuzouX/cyber-master'

$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'   # 关闭 Invoke-WebRequest 的进度条，否则慢且管道场景下报错

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'install.ps1 仅支持 Windows；Linux / macOS / WSL 请使用 install.sh。'
}

# ─── 合并环境变量 ──────────────────────────────────────────────────────────
if (-not $Version)    { $Version    = $env:CYBER_VERSION }
if (-not $InstallDir) { $InstallDir = $env:CYBER_INSTALL_DIR }
if (-not $InstallDir) { $InstallDir = Join-Path $env:USERPROFILE '.local\bin' }
if ($env:CYBER_REPO)  { $Repo       = $env:CYBER_REPO }
$CnbRepo = if ($env:CYBER_CNB_REPO) { $env:CYBER_CNB_REPO } else { 'funxlink/cyber-master' }
# ─── 平台检测（PowerShell 只支持 Windows 二进制；WSL 用户请用 install.sh）──
$architecture = if ($env:PROCESSOR_ARCHITEW6432) {
    $env:PROCESSOR_ARCHITEW6432
} else {
    $env:PROCESSOR_ARCHITECTURE
}
if ($architecture -ne 'AMD64') {
    throw "不支持的 Windows 架构：$architecture。当前仅发布 x86_64 (AMD64) 二进制。"
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir)
if ($InstallDir.Contains(';')) { throw "安装目录不能含 PATH 分隔符 ';'。" }
$Target = 'x86_64-pc-windows-msvc'
$Archive = "cyber-$Target.zip"

# ─── 解析版本（未指定时取 latest）──────────────────────────────────────────
if (-not $Version) {
    Write-Host "→ 查询最新版本…" -ForegroundColor Cyan
    if ($env:CYBER_USE_CNB) {
        try {
            $cnbReleases = Invoke-RestMethod -Uri "https://api.cnb.cool/$CnbRepo/-/releases" -Headers @{ 'Accept' = 'application/json' } -TimeoutSec 5
            if ($cnbReleases -and $cnbReleases.Count -gt 0) {
                $Version = $cnbReleases[0].tag_name
            }
        } catch {}
    }
    if (-not $Version) {
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
                                         -Headers @{ 'User-Agent' = 'cyber-installer' } -TimeoutSec 5
            $Version = $release.tag_name
        } catch {
            try {
                $cnbReleases = Invoke-RestMethod -Uri "https://api.cnb.cool/$CnbRepo/-/releases" -Headers @{ 'Accept' = 'application/json' } -TimeoutSec 5
                if ($cnbReleases -and $cnbReleases.Count -gt 0) {
                    $Version = $cnbReleases[0].tag_name
                }
            } catch {}
        }
    }
    if (-not $Version) {
        Write-Error "无法获取最新版本。请设置 `$env:CYBER_VERSION 显式指定版本 tag。"
        exit 1
    }
}

# ─── 下载源候选列表与多源测速 ──────────────────────────────────────────────
$GithubBase = "https://github.com/$Repo/releases/download/$Version"
$CnbBase    = "https://cnb.cool/$CnbRepo/-/releases/download/$Version"
$CandidateList = @()
if ($env:CYBER_DOWNLOAD_MIRROR) {
    $prefix = $env:CYBER_DOWNLOAD_MIRROR.TrimEnd('/')
    $CandidateList += @{ Name = "自定义镜像 ($prefix)"; Base = "$prefix/$GithubBase" }
}
$CandidateList += @(
    @{ Name = "CNB 镜像源 (国内极速)"; Base = $CnbBase },
    @{ Name = "GitHub 官方源";       Base = $GithubBase },
    @{ Name = "gh-proxy.com 镜像";   Base = "https://gh-proxy.com/$GithubBase" },
    @{ Name = "ghfast.top 镜像";     Base = "https://ghfast.top/$GithubBase" },
    @{ Name = "ghproxy.net 镜像";    Base = "https://ghproxy.net/$GithubBase" }
)
Write-Host "→ 安装 cyber $Version ($Target) 到 $InstallDir" -ForegroundColor Cyan

# ─── 下载、强制 SHA256 校验及安装；所有失败路径均清理临时目录 ─────────────
$tmpDir = Join-Path ([IO.Path]::GetTempPath()) "cyber-install-$([Guid]::NewGuid().ToString('N'))"
$zipFile = Join-Path $tmpDir $Archive
$shaFile = "$zipFile.sha256"
$extractPath = Join-Path $tmpDir 'extract'
$destBinary = Join-Path $InstallDir 'cyber.exe'
try {
    New-Item -ItemType Directory -Path $tmpDir | Out-Null

    Write-Host "→ 测速各下载源并选择最优节点…" -ForegroundColor Cyan
    $TestedSources = @()
    $speedtestSha = Join-Path $tmpDir "speedtest.sha256"

    foreach ($item in $CandidateList) {
        $base = $item.Base
        $name = $item.Name
        $shaUrl = "$base/$Archive.sha256"
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            Invoke-WebRequest -Uri $shaUrl -OutFile $speedtestSha -UseBasicParsing -TimeoutSec 4
            $sw.Stop()
            $ms = [int]$sw.ElapsedMilliseconds
            $content = (Get-Content -LiteralPath $speedtestSha -TotalCount 1).Trim()
            if ($content -match '^[0-9a-fA-F]{64}') {
                Write-Host ("   [✓] {0,-18} : {1}ms" -f $name, $ms) -ForegroundColor Green
                $TestedSources += [PSCustomObject]@{
                    Name = $name
                    Base = $base
                    Latency = $ms
                }
            } else {
                Write-Host ("   [✗] {0,-18} : 返回内容无效" -f $name) -ForegroundColor DarkGray
            }
        } catch {
            $sw.Stop()
            Write-Host ("   [✗] {0,-18} : 超时/不可达" -f $name) -ForegroundColor DarkGray
        }
    }

    if ($TestedSources.Count -gt 0) {
        $SortedSources = $TestedSources | Sort-Object -Property Latency
        $Fastest = $SortedSources[0]
        Write-Host ("→ 选用最优源: {0} ({1}ms)" -f $Fastest.Name, $Fastest.Latency) -ForegroundColor Cyan
        $Sources = @($SortedSources | ForEach-Object { $_.Base })
    } else {
        Write-Host "   所有源测速未响应，回退至默认源重试…" -ForegroundColor Yellow
        $Sources = @($CandidateList | ForEach-Object { $_.Base })
    }

    $downloaded = $false
    foreach ($base in $Sources) {
        $zipUrl = "$base/$Archive"
        $shaUrl = "$zipUrl.sha256"
        try {
            Write-Host "→ 下载 $zipUrl"
            Invoke-WebRequest -Uri $zipUrl -OutFile $zipFile -UseBasicParsing -TimeoutSec 120
            Invoke-WebRequest -Uri $shaUrl -OutFile $shaFile -UseBasicParsing -TimeoutSec 60
            $downloaded = $true
            break
        } catch {
            Write-Host "   该源下载失败（$($_.Exception.Message.Split([char]10)[0])），尝试备用源…" -ForegroundColor DarkGray
        }
    }
    if (-not $downloaded) {
        throw "所有下载源均失败。请检查网络/代理（需放行 release-assets.githubusercontent.com），或用 `$env:CYBER_DOWNLOAD_MIRROR 指定可用镜像前缀。"
    }
    Write-Host "→ 校验 SHA256…"
    $checksum = Get-Content -LiteralPath $shaFile -TotalCount 1
    if ($checksum -notmatch '^([0-9a-fA-F]{64})(\s|$)') {
        throw 'SHA256 校验文件格式无效，安装已中止。'
    }
    $expected = $Matches[1].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $zipFile -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        throw "SHA256 校验失败：expected=$expected actual=$actual"
    }
    Expand-Archive -LiteralPath $zipFile -DestinationPath $extractPath -Force
    $srcBinary = Join-Path $extractPath 'cyber.exe'
    if (-not (Test-Path -LiteralPath $srcBinary -PathType Leaf)) {
        throw '压缩包内未找到 cyber.exe'
    }
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    # 不删除正在使用的旧二进制；覆盖失败时提示关闭进程后重试。
    try {
        Copy-Item -LiteralPath $srcBinary -Destination $destBinary -Force
    } catch {
        throw "无法写入 $destBinary；请关闭正在运行的 cyber 并检查目录权限后重试。$_"
    }
} finally {
    Remove-Item -LiteralPath $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
}

# ─── 添加到用户 PATH ──────────────────────────────────────────────────────
function Test-CyberPath([string]$PathValue) {
    foreach ($item in ($PathValue -split ';')) {
        if (-not $item.Trim()) { continue }
        $expanded = [Environment]::ExpandEnvironmentVariables($item.Trim().Trim('"'))
        try {
            if ([IO.Path]::GetFullPath($expanded).TrimEnd('\') -eq $InstallDir.TrimEnd('\')) {
                return $true
            }
        } catch {
            # 保留用户原有 PATH 中无法规范化的条目。
        }
    }
    return $false
}
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (Test-CyberPath $userPath)) {
    $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host "✓ 已将 $InstallDir 添加到用户 PATH" -ForegroundColor Green
} else {
    Write-Host "✓ $InstallDir 已在用户 PATH 中" -ForegroundColor DarkGray
}
# 即便持久用户 PATH 已有该目录，当前会话也可能尚未继承它。
if (-not (Test-CyberPath $env:Path)) {
    $env:Path = if ($env:Path) { "$env:Path;$InstallDir" } else { $InstallDir }
}
Write-Host "  请重新打开其他终端；当前 PowerShell 会话的 PATH 已同步。" -ForegroundColor DarkGray

# ─── 完成提示 ────────────────────────────────────────────────────────────
Write-Host ""
Write-Host "✓ 已安装: $destBinary" -ForegroundColor Green
Write-Host "  运行: cyber" -ForegroundColor Green
Write-Host ""
Write-Host "首次运行 cyber 会自动在 $env:USERPROFILE\.cyber\ 创建配置目录。" -ForegroundColor DarkGray
Write-Host "文档: https://github.com/$Repo#readme" -ForegroundColor DarkGray
