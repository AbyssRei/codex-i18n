# Codex i18n 管理器

Windows 图形工具：从当前用户安装的 Microsoft Store 版 Codex 创建独立副本，使远程语言配置缺失时默认启用 i18n，并创建 `ChatGPT(i18n)` 桌面快捷方式。

## 使用

1. 在 Windows 上运行 `codex-i18n.exe`，点击“创建 / 更新修复版”。副本默认保存在 `%LOCALAPPDATA%\CodexI18n`，不会修改 WindowsApps 中的原版。
2. 点击“创建桌面快捷方式”。快捷方式指向当前管理器位置；移动管理器后请删除旧快捷方式并重新创建。已有同名文件不会被覆盖。
3. 使用 `ChatGPT(i18n)` 快捷方式启动。程序先比较已记录的来源包与当前 Store 包；版本变化时可重新生成并启动，或继续运行已有副本。无法查询 Store 版本时可选择继续运行现有副本。

如果已生成副本的文件被移动或删除，可点击“创建 / 更新修复版”重新生成。若状态记录本身损坏或路径无效，程序会停止并显示错误，避免覆盖无法识别的记录。

生成、更新期间可取消；正在切换状态的最后一步不响应取消。更新失败不会切换到未完成的副本。管理器会保留以前的副本，不自动删除用户数据。修补会使副本 EXE 的官方数字签名失效，且副本不具有 Store 包身份；部分 Windows 集成功能可能与原版不同。原版和副本可能共用用户数据，启动时若检测到 `ChatGPT.exe` 已运行，会提示你先退出已有程序或自行选择继续启动。

## 构建与检查

需要 Windows x64、最新 Rust stable、MSVC C++ 构建工具及 Windows SDK：

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

生成文件为 `target/release/codex-i18n.exe`。项目对 Windows x64 MSVC 启用静态 CRT，以减少额外运行库依赖；系统 API、图形驱动和其他 Windows 自带组件仍然需要在目标机器上可用。

自动化测试不要求安装 Codex。可在已安装 Store 版本的本机运行额外的只读检查和隔离副本测试：

```powershell
cargo test --release --lib installed_package_and_signature -- --ignored --nocapture
cargo test --release --lib installed_package_builds_an_independent_copy -- --ignored --nocapture
```

第二个测试在本仓库 `target` 下创建临时目录并在测试结束后清理，需要足够空间容纳完整副本；不会运行修复版或修改 Store 原版。

## 版本与维护

项目遵循语义化版本和约定式提交。直接依赖在 `Cargo.toml` 中使用兼容版本范围，`Cargo.lock` 固定每次构建的依赖解析结果；依赖和 GitHub Actions 由 Dependabot 定期检查更新。Windows CI 使用 `rust-cache`，应根据实际运行的缓存命中率和恢复时间评估其效果。

更多设计和选型依据见 [技术选型文档](docs/technology-selection.md)。
