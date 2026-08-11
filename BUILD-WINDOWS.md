# 在 GitHub 云端构建 Windows 安装包（零本地环境）

本项目是 Tauri 2 桌面应用（Vue 3 前端 + Rust 后端）。你**不需要**在本地安装 Node、Rust、Tauri 或 Visual Studio Build Tools——所有编译都在 GitHub 的云端 Runner 上完成，本地只要有 Git 即可。

打包逻辑由 `.github/workflows/package-windows.yml` 实现：**只构建 Windows x64**，且**不依赖任何密钥**（自动更新签名已临时关闭）。

---

## 前置条件

- 一个 GitHub 账号，且已 Fork 本项目（示例：`softwarevax/dbx`）。
- 本地安装 Git（用于提交工作流文件、触发标签）。
- 工作流文件已存在于你 fork 的 **默认分支**（如 `main`）。

---

## 方式一：手动触发（最省事，推荐）

1. 打开你 fork 的仓库页面：`https://github.com/<你的用户名>/dbx`。
2. 点击顶部 **Actions** 标签页。
3. 左侧工作流列表选择 **Package Windows**。
4. 点击 **Run workflow**，确认分支为 `main`，再点绿色 **Run workflow**。
5. 等待构建完成：
   - 首次（无缓存）冷编译约 **30–50 分钟**（Rust + DuckDB 等重型依赖）。
   - 后续构建因 `rust-cache` 命中而显著加快。
6. 构建结束后，在 run 页面底部的 **Artifacts** 区域下载 `dbx-windows`（zip）。
7. 解压后得到安装包：
   - `DBX_<版本>_x64-setup.exe` —— 标准 NSIS 安装包（安装时按需联网下载 WebView2）。
   - `DBX_<版本>_x64-webview2-offline-setup.exe` —— **离线安装包**，已内嵌 WebView2 运行时，适合无外网环境。

## 方式二：打标签触发

适合想用固定版本号归档时：

```bash
git tag v0.5.81
git push origin v0.5.81
```

推送 `v*` 标签会自动触发 `Package Windows` 工作流，后续下载步骤同方式一。

---

## 安装与运行

1. 双击 `DBX_*_x64-setup.exe` 按向导安装（默认当前用户安装，无需管理员）。
2. 首次启动若被 **SmartScreen** 拦截，点击"仍要运行"即可正常使用——这是因为安装包**未做代码签名**，不影响功能。
3. 离线版安装包无需联网即可完成安装（已含 WebView2 运行时）。

---

## 工作流做了什么（简述）

| 步骤 | 说明 |
|---|---|
| Checkout | 拉取代码 |
| Setup Node/pnpm | Node 22 + pnpm（版本取 `package.json` 的 `packageManager`） |
| pnpm install | 安装前端依赖 |
| Setup Rust | 安装 `1.97.1` + 目标 `x86_64-pc-windows-msvc` |
| Rust cache | 缓存编译产物，加速后续构建 |
| Disable updater signing | 用 `jq` 把 `createUpdaterArtifacts` 设为 `false` 并删除 `plugins.updater`，**绕开上游签名私钥要求** |
| Build Tauri app | `pnpm tauri build --target x86_64-pc-windows-msvc` |
| Build offline installer | 额外生成内嵌 WebView2 的离线安装包（`continue-on-error`，失败不阻塞主包） |
| Upload artifact | 上传 `src-tauri/target/x86_64-pc-windows-msvc/release/bundle/**/*` 为 `dbx-windows`，保留 30 天 |

---

## 常见问题

**Q：构建失败了，怎么看原因？**
进入对应 run 页面，展开各步骤日志；本工作流是零密钥设计，已规避 `release.yml` 在 fork 上因缺 `TAURI_SIGNING_PRIVATE_KEY` / `DOCKERHUB_*` / `CNB_TOKEN` 而失败的问题。

**Q：想启用自动更新怎么办？**
需自行生成 Tauri updater 密钥对：
```bash
npx @tauri-apps/cli@latest signer generate
```
然后把私钥 base64 后存为仓库 Secret `TAURI_SIGNING_PRIVATE_KEY_BASE64`，并把 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey` 换成你自己的公钥，同时将 `updater.endpoints` 改为指向你 fork 的 releases。完成后可改用 `release.yml`（需再裁剪 Docker/CNB/Apple 相关依赖）。

**Q：需要 arm64（Win ARM 设备）安装包？**
当前工作流仅出 x64。如需 arm64，在工作流 `build-windows` job 中增加 `aarch64-pc-windows-msvc` 目标即可，可告知我补充。

**Q：想去掉 SmartScreen 拦截提示？**
需要 Windows 代码签名证书（如 Azure Trusted Signing / 商业 EV 证书），属另一套密钥配置，按需再加。
