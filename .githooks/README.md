# Git Hooks 与发布预检系统 (Release Pre-Flight Verification)

本目录包含 Cyber Master 的 Git Hook 及本地发布预检脚本，直接对应 GitHub Actions 自动化发布工作流 [`.github/workflows/release.yml`](../.github/workflows/release.yml)。

在执行发布推送（如 `git push origin v0.5.0`）时，Git Hook 会在网络传输前**自动拦截并执行全量预检**，确保在代码推送到远程仓库之前排除所有已知可能导致 GitHub Actions 失败的隐患。

---

## 对应 GitHub Actions 流程检查项

| 序号 | 检查步骤 | 对应 GitHub Actions 步骤 | 失败风险与防护机制 |
|---|---|---|---|
| **1** | 工作区版本号解析 | `ref: ${{ inputs.tag ... }}` | 检查 `Cargo.toml` 的 `[workspace.package].version` 是否合法存在 |
| **2** | Tag 与版本号一致性 | `softprops/action-gh-release` | 防止打错 Tag（如本地版本为 `0.5.0` 却推送了 `v0.4.0`），避免 GHA 构建出错误版本的二进制产物 |
| **3** | CHANGELOG.md 日志检查 | `generate_release_notes` | 确保当前版本在 `CHANGELOG.md` 中已有更新摘要记录 |
| **4** | 关键配置与锁文件提交状态 | `actions/checkout@v4` | 检查 `Cargo.toml` 与 `Cargo.lock` 是否存在未提交修改（GHA 仅检出已提交代码，本地未提交的修改会导致远程依赖锁失败） |
| **5** | 安装脚本版本解析 | `install.sh` / `cyber update` 共用 | 离线回归检查（`.githooks/check-install.sh`）：release 接口返回**单行 JSON** 时，`install.sh` 必须只取 `"tag_name"`，不得把发布说明 `body` 当版本号；同时校验非法 `--version` 快速失败 |
| **6** | 代码格式校验 | `cargo fmt --all -- --check` | 100% 对齐 CI 代码规范要求 |
| **7** | Clippy 静态代码检查 | `cargo clippy --workspace --all-targets --locked -- -D warnings` | 严格禁止任何 clippy 告警进入主干及 Release 构建 |
| **8** | 全量单元/集成测试 | `cargo test --workspace --locked` | 全工作区测试套件验证，采用 `--locked` 保证依赖一致性 |

---

## 核心脚本清单

- **`.githooks/pre-push`**：Git 预推送 Hook 脚本。拦截 `git push`，分析推送引用，精准检测 Release Tag。
- **`.githooks/check-release.sh`**：POSIX Shell 规范的独立预检脚本（兼容 Linux、macOS、Git Bash / MSYS2）。
- **`.githooks/check-release.ps1`**：PowerShell 独立预检脚本（带 UTF-8 BOM，专为 Windows 本地开发者优化）。
- **`.githooks/check-install.sh`**：`install.sh` 版本解析离线回归检查（curl/uname/getconf 替身 + 合成 release JSON 与安装包，无需网络与 tty），由 `check-release.sh` 第 5 步调用。
- **`.githooks/install.sh`**：一键安装配置脚本（设置 `git config core.hooksPath .githooks`）。

---

## 工作机制与触发逻辑

1. **自动拦截 Release Tag**：
   当执行 `git push origin v0.5.0` 或推送匹配 `v*` / `[0-9]*` 的标签时，Hook 自动激活全量 8 步预检。
2. **日常开发零延迟快速放行**：
   当推送日常功能分支或修复提交（如 `git push origin feature/xxx`）时，Hook 毫秒级快速放行，不影响日常开发效率。
3. **手动强制触发**：
   在任何分支推送时，若希望完整验证 CI 状态，可设置环境变量：
   ```bash
   CYBER_RELEASE_CHECK=1 git push
   ```
4. **紧急绕过机制（Emergency Bypass）**：
   若因网络或环境问题需紧急跳过本地检查，可使用 Git 原生标志：
   ```bash
   git push --no-verify
   ```

---

## 手动执行方式

在正式创建或推送 Tag 之前，开发者可随时手动运行检查：

### POSIX Shell / Git Bash:
```bash
# 验证指定 Tag
sh .githooks/check-release.sh v0.5.0

# 或根据当前 Cargo.toml 自动推导版本进行验证
sh .githooks/check-release.sh
```

### Windows PowerShell:
```powershell
# 验证指定 Tag
powershell -ExecutionPolicy Bypass -File .githooks/check-release.ps1 -Tag "v0.5.0"

# 自动推导版本
powershell -ExecutionPolicy Bypass -File .githooks/check-release.ps1
```

---

## 团队或新环境一键生效

在重新克隆仓库后，仅需运行一次：
```bash
sh .githooks/install.sh
```
或者直接配置：
```bash
git config core.hooksPath .githooks
```
