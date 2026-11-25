# WinLaunchpad

Windows 11 上的 macOS 风格启动台。一个快捷键呼出/收起，自动搜罗已安装的应用，分页网格 + 即时搜索，托盘常驻。
纯 Win32 + GDI 实现（Rust），**后台常驻约 30 MB，打开约 55 MB，远低于 100 MB 的硬指标**，不加载任何显卡驱动。

作者：**CrazyFigure**

## 功能

| 功能 | 说明 |
| --- | --- |
| 启动台 | 铺满鼠标所在显示器（保留任务栏），7～8 列网格、页码圆点、滚轮 / `PgUp` `PgDn` 翻页 |
| 搜索 | 支持中文、英文应用名和可执行文件名，输入即时筛选；中文输入法组词不受影响，上屏后立即过滤 |
| 自动发现 | 开始菜单（当前用户 + 公共）、`shell:AppsFolder` 打包应用（UWP / MSIX）、`App Paths` 中未登记到开始菜单的程序 |
| 过滤 | 自动排除 `卸载*`、`unins000.exe`、`安装*`、`配置*`、`设置`、`帮助`、`Readme`、`检查更新`、`官网` 等辅助入口；`Uninstall Tool`、`UniGetUI` 这类产品名会保留 |
| 手动管理 | 右键「添加应用…」拖放或浏览 `.exe` / `.lnk`；条目右键「从启动台移除」，设置里可恢复 |
| 全局快捷键 | 默认 `Ctrl + Alt + Shift + A`，可在设置中改录（支持 `Alt + A` 等），冲突时保留旧配置并提示 |
| 开机启动 | 首次运行默认创建「启动」文件夹快捷方式（`--background` 仅托盘）；托盘菜单 / 设置可关闭 |
| 托盘 | 左键呼出/收起，右键菜单：打开/收起、重新扫描、设置、随 Windows 登录启动、退出；Explorer 重启后自动恢复 |
| 命令行 | `--toggle` 切换（供 Quicker 等调用）、`--background` 仅托盘、`--settings` 打开设置、`--add` 打开添加窗口 |
| 内置右键手势（可选，默认关闭） | 按住右键滑动：`←` 切到右侧桌面、`→` 切到左侧桌面、`↑` 切换窗口、`∧` 启动台、`∨` 任务视图 |

## 快速开始

1. 从 Release 下载 `WinLaunchpad_<版本>_windows_x64_setup.exe`，双击打开安装向导。默认安装到当前用户的 `%LOCALAPPDATA%\Programs\WinLaunchpad`，完成后可从开始菜单打开。
2. 按 `Ctrl + Alt + Shift + A` 呼出 / 收起；输入几个字母即可搜索；`Esc` 或点击其它位置收起。
3. 想改快捷键：托盘右键 →「设置…」→「修改…」→ 按下新组合键 → 确定。
4. 检测更新：打开「设置 → 关于」，点击「检测更新」。发现新版本后可一键更新并重启，展开版本行可查看更新日志。

可以从 Windows「设置 → 应用 → 已安装的应用」卸载，用户配置会保留。

### 配合 Quicker 使用（推荐沿用你现有的手势）

在 Quicker 里把原来指向 MyDockerFinder 的「启动台」动作改成 **启动程序**：

- 程序路径：`...\WinLaunchpad.exe`
- 参数：`--toggle`

这样右键 `∧` 手势与快捷键解耦，启动台尚未运行时也能直接拉起。桌面切换、切换窗口、任务视图这几个手势继续由 Quicker 原样处理即可。
如果想脱离 Quicker，可在设置里勾选「启用右键手势」，WinLaunchpad 自己接管这五个手势（见下文说明）。

## 内存与稳定性

实测机器：Windows 11，3072×1920 @ 200%，约 200 个应用条目。

| 状态 | 工作集 | 私有提交 |
| --- | --- | --- |
| 后台常驻（托盘） | ~30 MB | ~8 MB |
| 打开显示 | ~55 MB | ~20 MB |
| 连续开关 20 次后 | ~60–77 MB | ~23–47 MB |

- 渲染不使用 Direct2D/Direct3D：实测硬件渲染目标会把 Intel 显卡驱动（`igc64.dll` 等 100 MB+）映射进进程，无法满足 100 MB 要求。改为 32bpp DIB 后备缓冲 + 软件抗锯齿 + GDI 文字 + `AlphaBlend` 贴图标。
- 隐藏后释放后备缓冲与图标缓存；图标缓存按字节设上限（8 MB）。
- 窗口过程、扫描线程、图标线程均带 `catch_unwind`，panic 写入 `%LOCALAPPDATA%\WinLaunchpad\log.txt` 后进程继续运行。
- 设置写入先写 `.tmp` 再替换，并保留 `.bak`；目录缓存损坏时自动重建。

## 项目结构

```
src/main.rs      入口：单实例互斥、命令转发（--toggle 等）、消息循环
src/app.rs       主窗口：显隐、前台切换、搜索/分页/键盘、右键菜单、托盘与热键集成
src/render.rs    GDI 渲染：布局计算、DIB 后备缓冲、软件抗锯齿图形、图标缓存
src/catalog.rs   应用发现：开始菜单 / AppsFolder / App Paths、过滤、去重、排序
src/icons.rs     图标提取线程（IShellItemImageFactory → 预乘 BGRA 像素）
src/config.rs    settings.json / catalog.json 持久化
src/settings.rs  设置窗口（快捷键录入、登录启动、手势开关、恢复隐藏项）
src/add.rs       「添加应用」窗口（拖放 / 浏览 exe、lnk）
src/tray.rs      托盘图标与菜单
src/hotkey.rs    RegisterHotKey 封装、按键名称
src/startup.rs   「启动」文件夹快捷方式、StartupApproved 状态
src/launch.rs    ShellExecuteExW / IApplicationActivationManager 启动
src/gesture.rs   可选右键手势：WH_MOUSE_LL 钩子、轨迹识别、SendInput 动作
```

数据目录：`%LOCALAPPDATA%\WinLaunchpad\`（`settings.json`、`catalog.json`、`log.txt`）。

## 构建

需要 Rust stable（MSVC 工具链）与 Windows SDK（`rc.exe`，用于内嵌图标；缺失时会跳过资源但不影响功能）。

```
cargo build --release
cargo test --release
```

使用 NSIS 3.11 打包安装程序，`makensis.exe` 需在 PATH 上，也可以通过 `-Makensis` 指定路径。

```powershell
./tools/package-windows.ps1 -Executable ./target/release/winlaunchpad.exe
```

安装程序生成在 `dist`。`tools\make_icon.ps1` 可重新生成 `assets\app.ico`。

发布时更新 Cargo 和 Windows 资源版本，并在 `assets/release-notes.json` 列表开头添加对应版本的日志。推送对应标签后，GitHub Actions 构建安装程序和签名更新附件，再将它们连同 Git 引用与发布说明同步到 CNB。

## 内置右键手势的取舍

启用后，WinLaunchpad 会在独立线程安装低级鼠标钩子：按下右键时先"吞掉"事件，松开时若识别出轨迹就执行动作，否则回放一次普通右键，保证菜单正常弹出。
这与 Quicker 的实现方式相同，代价是右键拖动类操作（如 3D 软件里按住右键旋转）会受到影响，所以**默认关闭**，并且同一时间只应有一个软件接管右键手势。注入事件（含 Quicker 发出的）会被钩子直接放行。

## 与 DEVELOPMENT_PLAN.md 的差异

- 渲染层由 Direct2D 改为 GDI + 软件抗锯齿（内存硬指标，见上文）。
- 首版不包含 UI Automation 暴露、通用手势映射编辑与应用排除列表；手势预设固定为五个。
- 应用启动在短时线程中执行，但未做进程级回收；Shell 启动失败会在界面上提示。

## 许可

仅限个人使用与修改。署名 CrazyFigure。
