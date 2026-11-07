# GUI 原型

这里包含 egui 和 GPUI Kit 的独立实验项目，不属于正式管理器。
两个原型均显示示例版本和状态；点击“模拟创建修复版”会让进度增加 25%，
点击“重置演示”会清零。所有内容都是模拟，不读取、创建或修改 Codex 文件。

在 Windows 上分别运行：

```powershell
cargo run --release --manifest-path .prototypes/egui/Cargo.toml
cargo run --release --manifest-path .prototypes/gpui/Cargo.toml
```

构建产物位于各自的 `target/release` 目录，可以直接运行。
GPUI Kit 原型启动时读取系统主题，并在系统外观变化时同步组件主题。

`pattern-study` 是独立的只读匹配方案研究项目，包含构造测试和对指定
`app.asar` 的只读比较。它不执行修补。若反向引用引擎达到回溯上限，
程序报告不完整比较并以非零状态退出；片段验证不代表完整扫描成功。

```powershell
cargo test --release --manifest-path .prototypes/pattern-study/Cargo.toml
$package = Get-AppxPackage -Name 'OpenAI.Codex'
$archive = Join-Path $package.InstallLocation 'app/resources/app.asar'
cargo run --release --manifest-path .prototypes/pattern-study/Cargo.toml -- $archive
```

`concurrency-study` 是一个同时提供两种执行方式的 GPUI 实验程序。它使用
系统临时文件生成 128 MiB 数据，复制后读取目标并校验 SHA-256；两种方式
采用相同任务和异步进度通道。每 MiB 复制后加入 1 毫秒延迟以便观察
界面心跳。测试完成或进程退出后，临时文件由操作系统清理。

```powershell
cargo run --release --manifest-path .prototypes/concurrency-study/Cargo.toml
```

自动化测试支持 `--bench executor|thread <结果文件> [cancel|fail]`。
不提供最后一个参数时执行正常复制与校验；`cancel` 在首个进度事件后
协作式取消；`fail` 在复制过程中注入错误。`bench-*.txt` 是本机实测结果。
自动化测试采用隐藏窗口；心跳指标只能说明前台任务调度情况，不能替代
可见窗口的人工交互测试。
