# WinLaunchpad 开发计划

项目署名 **CrazyFigure** · 首版目标 **Windows 11 x64** · [项目仓库](https://github.com/CrazyFigure/WinLaunchpad)

WinLaunchpad 是一个常驻托盘的应用启动台。用户可以用同一个快捷键呼出和收起面板，从分页网格或搜索结果启动应用。程序自动发现已安装应用的可启动入口，允许手动添加、移除和恢复条目。发布版在冷启动、扫描、显示和常驻期间的内存上限为 **100 MB**，稳定性和响应速度以 Windows 实测为准。

## 1. 首版范围和默认设置

| 功能 | 首版行为 |
| --- | --- |
| 启动台 | 在当前操作的显示器展开，支持应用网格、中文和英文名称搜索、键盘导航、分页和一键启动。呼出时搜索框获得焦点。 |
| 应用目录 | 首次后台扫描，后续自动更新。提供「重新扫描」。可选择或拖入 `.exe`、`.lnk`。应用右键菜单提供「从启动台移除」，隐藏项可恢复。 |
| 开关快捷键 | 建议默认 `Ctrl+Alt+Shift+A`，可修改为 `Alt+A` 或其他组合，也可禁用。冲突时保留可用的原配置，托盘始终提供入口。 |
| 开机启动 | 首次安装默认启用。当前用户登录后仅驻留托盘。用户可关闭，更新保留其选择。 |
| 托盘 | 左键呼出或收起。右键菜单提供「打开／收起启动台」「设置」「重新扫描」「随 Windows 登录启动」「退出」。 |
| Quicker | 首版提供 `WinLaunchpad.exe --toggle`。由 Quicker 的启动程序动作调用，现有的窗口和虚拟桌面手势可以照常使用。 |
| 内置手势 | 独立可选里程碑，默认关闭。首批仅实现本计划中列出的五个动作。 |

图 1 提供搜索、分页和应用网格的交互参考，图 2 提供添加入口的交互参考，图 3 提供用户现有的手势映射。界面采用自有布局、色彩和图标，沿用原有用法。首版聚焦启动台。Dock、主题系统、应用卸载和通用自动化均留在首版范围之外。

## 2. 技术路线与职责

采用 **Rust + Win32**，使用 `windows` crate 调用系统 API。搜索输入、文件选择和设置优先使用原生控件，网格用 Direct2D 和 DirectWrite 绘制。安装后保留一个轻量托盘常驻进程，扫描与图标提取均按需执行。原型阶段先测真实窗口、实际图标和扫描同时运行的峰值。超出预算时，先定位 Shell、图标、渲染或数据结构的开销，再降级效果或调整实现。技术选型以实测通过为放行条件。[Rust for Windows](https://github.com/microsoft/windows-rs) · [Direct2D](https://learn.microsoft.com/en-us/windows/win32/direct2d/direct2d-overview)

| 职责 | 交付 | 主要依赖 |
| --- | --- | --- |
| Windows 集成 | 程序入口、窗口生命周期、托盘、快捷键、登录启动、单实例命令转发、安装和卸载。 | Win32 消息循环和用户会话。 |
| 应用目录和启动 | 扫描、归并、过滤、启动分派、数据持久化、图标资源、错误恢复。 | Windows Shell 和本地存储。 |
| 前端 | 启动面板、搜索和分页、添加、移除和恢复、设置、状态反馈、键盘和辅助功能。 | 本文界面接口与可替换的模拟目录。 |
| 可选手势 | 识别五类轨迹、转发对应动作、提供总开关和可调整映射。 | 独立输入线程，通过已有命令调用启动台。 |

窗口线程只处理消息、可见状态和绘制。扫描和图标提取复用同一可执行文件的短时工作模式，同时最多运行一个任务进程。超时回收进程并保留上次成功的目录。应用启动请求走独立的 Shell STA 执行路径，限制同一请求的重复提交。原型须验证 Shell 启动调用超时后能恢复执行。若调用无法安全取消，则将这条路径也放入可回收的短时进程，并保证回收范围仅包含 WinLaunchpad 自身。设置和目录状态由常驻进程统一写入。

## 3. 应用发现、过滤和持久化

### 自动扫描来源

| 来源 | 默认位置或注册表路径 | 读取内容 |
| --- | --- | --- |
| 当前用户的开始菜单 | `%APPDATA%\Microsoft\Windows\Start Menu\Programs` | `FOLDERID_Programs` 下的应用 `.lnk`，包括子目录。 |
| 公共开始菜单 | `%ProgramData%\Microsoft\Windows\Start Menu\Programs` | `FOLDERID_CommonPrograms` 下的应用 `.lnk`，包括子目录。 |
| Windows 应用列表 | `shell:AppsFolder`（`FOLDERID_AppsFolder`） | 枚举 Shell 虚拟文件夹，补齐商店和打包应用，记录其 AUMID。 |
| 桌面应用注册 | `HKCU\Software\Microsoft\Windows\CurrentVersion\App Paths` 及 `HKLM` 下同路径 | 从子键默认值读取已注册的 exe 完整路径，校验单个本地目标，补齐开始菜单缺少的入口。 |
| 已安装程序登记 | `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall` 及 `HKLM` 下同路径 | 名称和排除规则的辅助数据。仅当能匹配已知候选入口时使用。仅有安装记录的程序可手动添加。 |

两个开始菜单目录用 `SHGetKnownFolderPath` 获取实际位置。目录重定向到网络时跳过该来源，保留对应缓存。递归时跳过 `FOLDERID_Startup`、`FOLDERID_CommonStartup` 和指向扫描根外的目录链接。注册表用系统 API 读取 32 位和 64 位视图并归并重复记录。[Windows 已知文件夹](https://learn.microsoft.com/en-us/windows/win32/shell/knownfolderid) · [查询文件夹实际位置](https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shgetknownfolderpath) · [注册表视图](https://learn.microsoft.com/en-us/windows/win32/winprog64/accessing-an-alternate-registry-view)

`%ProgramFiles%`、`%ProgramFiles(x86)%` 和 `%LOCALAPPDATA%\Programs` 只在已知候选入口明确指向某个文件时校验目标。目录遍历限于上表两处开始菜单。便携软件或未注册启动入口的软件，可从桌面、下载目录或其他本地位置选择或拖入 `.exe`、`.lnk` 添加。[Windows 应用 ID](https://learn.microsoft.com/en-us/windows/configuration/store/find-aumid) · [桌面应用注册](https://learn.microsoft.com/en-us/windows/win32/shell/app-registration) · [安装程序卸载注册字段](https://learn.microsoft.com/en-us/windows/win32/msi/uninstall-registry-key)

### 过滤、启动和数据保存

1. **明确过滤对象。** 检查候选入口的名称、目标文件名、参数及 Shell 排除标记。排除卸载、配置、安装、修复等辅助入口，例如 `卸载.exe`、`配置.exe`、`unins000.exe` 和携带卸载参数的快捷方式。按完整文件名、词边界和参数规则匹配，结合产品元数据保留 `Uninstall Tool`、`UniGetUI` 这类本身就是可用产品的主入口。跳过明显失效的路径，逐条记录原因。扫描只读取目录和注册元数据，索引过程保持无弹窗、无自动启动和无网络路径探测。
2. **稳定去重。** 用打包应用的 AUMID、Win32 的规范化目标路径与启动参数组合标识同一入口。无法安全解析目标但可由 Shell 启动的快捷方式用独立 Shell 标识或快捷方式路径。可确认同一入口时优先保留有名称、图标和完整参数的条目。同一个 exe 的不同有效启动参数分别保留。更新后尽可能保持 ID 稳定，隐藏记录据此生效。
3. **刷新与启动。** 有缓存时先从缓存显示，后台合并扫描，目录通知去抖，面板打开时对过期目录补做检查。每个来源分别提交成功快照，避免某个来源暂时失败清空其旧数据。桌面程序及 `.lnk` 走 `ShellExecuteExW`，保留快捷方式语义。支持通用启动契约的商店应用可用 AUMID 激活，其他已注册条目走其 Shell 激活入口。目标缺失、访问拒绝和用户取消提升权限，均在界面反馈并保留启动台。[Windows Shell 启动](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecuteexw) · [应用激活](https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-iapplicationactivationmanager-activateapplication) · [Shell 变更通知](https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shchangenotifyregister)
4. **区分用户操作。** 移除自动发现项时按 ID 写入隐藏记录，刷新或升级后仍隐藏，可在设置中恢复。移除手工添加项时删除启动台中的记录，目标文件始终保留。显式添加同一已隐藏入口时恢复该条目，已显示的重复入口定位到原条目。手工入口暂时失效时保留记录，供用户修复或移除。
5. **安全保存状态。** 在 `%LOCALAPPDATA%\WinLaunchpad` 保存带版本号的设置、手动条目和隐藏记录。扫描缓存与用户数据分开保存。每次更新先在同目录写入临时文件并刷盘，再替换目标并保留最近可读备份。损坏时从备份恢复，保留损坏文件供定位。缓存可在后台重建。日志本地限量轮转，记录错误类别和必要上下文。[Windows 文件替换 API](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)

## 4. 前端 Agent 交付说明

前端 Agent 负责可运行的 Windows 原生界面。先用模拟目录实现完整交互，再接入真实目录、托盘和系统设置。共用生产版的数据和命令边界，模拟数据随开发预览提供。

### 主面板视觉与交互

主面板用 Direct2D 绘制深蓝灰渐层、搜索框外壳、图标和交互状态，用 DirectWrite 渲染应用名称。搜索框的文本录入和中文输入法交给原生编辑控件。应用图标用与屏幕 DPI 相匹配的资源并保留原始比例和透明边缘。用固定间距对齐标签和控件，键盘焦点有清晰描边。文件选择器与托盘菜单沿用系统样式。[Direct2D 渐层画刷](https://learn.microsoft.com/en-us/windows/win32/direct2d/direct2d-brushes-overview) · [DirectWrite 文本渲染](https://learn.microsoft.com/en-us/windows/win32/directwrite/direct-write-portal)

前端原型先交付一页使用真实应用图标的网格，附 1080p 100% 与 4K 150% 的运行截图及一次呼出和翻页演示。检查图标清晰度、名称可读性、留白，以及搜索和键盘焦点能否一眼辨认。背景使用静态渐层，动效保持简短。截图、实际操作和内存峰值一起用于确定正式视觉方案。

| 界面 | 实现要点 |
| --- | --- |
| 启动面板 | 优先铺满鼠标所在显示器的工作区，保留系统任务栏。顶部左侧放搜索，右侧放添加、设置入口。应用区采用图标网格和单行名称，底部用页码及前后翻页按钮。大屏可使用参考图的 7 列 × 4 行密度，实际列数和行数由可用空间与 DPI 推导。 |
| 搜索和分页 | 首版支持中文及英文应用名、可执行文件名，英文匹配忽略大小写。默认按显示名称排序，输入即时筛选并回到首页。滚轮和 `PageUp`／`PageDown` 翻页，方向键移动焦点，`Enter` 启动当前项。中文输入法组词期间保留输入法对方向键和 `Enter` 的处理。 |
| 添加应用 | 支持系统文件选择器和拖放 `.exe`、`.lnk`。显示候选名称、图标和路径，确认后写入。批量添加部分失败时保留成功项并逐条显示失败原因。 |
| 条目和设置 | 条目右键提供「打开」「从启动台移除」。设置包含快捷键录入、登录启动开关、隐藏应用恢复。内置手势交付后再显示相应入口。快捷键或系统设置保存成功才更新正式状态，取消保留原值。 |
| 空状态与故障 | 处理首次扫描、已有内容时刷新、无匹配结果、目录为空和局部扫描失败。图标缺失用通用图标，长名称用延迟 100 ms 的自定义提示查看全名。启动失败时保留当前页与查询。 |

单击应用或按 `Enter`，系统接受启动后收起面板。快捷键再次触发、`Esc` 或点按网格外空白区可收起。菜单、文件选择和设置打开期间保持交互稳定。应用条目和分页按钮具有悬停、按下及键盘焦点状态。自绘网格向 UI Automation 暴露应用名称、焦点和调用操作。使用系统字体，验证高对比度、中文输入法、100%／150%／200% 缩放与混合 DPI 双屏。开合使用轻量淡入，关闭动画时立即显示，设备丢失时重建渲染资源。[UI Automation 提供程序](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-providersoverview)

### 与 Windows 集成和应用目录的接口

以下名称用于约定 Rust 模块之间的边界，类型可在原型中定稿。窗口与系统菜单共用同一组命令，事件通过 Windows 消息循环切回窗口线程更新界面。

| 类型 | 合约 |
| --- | --- |
| 应用快照 | `CatalogSnapshot` 提供版本、扫描状态、轻量应用条目与来源错误。`AppEntry` 至少含 `id`、`display_name`、可搜索别名、`source`、`icon_key` 和可用状态。启动路径和 Shell 对象归目录模块持有。 |
| 应用操作 | `Search(query)` 返回稳定排序的条目 ID，`Launch(id)`、`Add(paths)`、`Remove(id)`、`Restore(id)`、`Rescan` 返回明确的结果。批量添加按路径返回结果。 |
| 系统操作 | `Show`、`Hide`、`Toggle`、`SetHotkey`、`SetStartup`。热键冲突、登录启动修改失败、目标缺失和启动失败分别反馈。 |
| 图标 | `RequestIcons(ids, pixel_size)` 仅请求可见页面及预算内的附近页面。结果附带条目 ID、图标版本和像素尺寸。翻页、搜索或缩放变化后丢弃过期结果。 |

前端交付包含可运行预览、可复用控件、与正式接口联调记录。模拟目录至少覆盖 1、28、29、1,000 项，含中英文长名称、缺图标、失效条目、隐藏项和快速翻页。展示主面板、添加、设置、无结果、启动失败的截图，并附键盘操作与混合 DPI 检查结果。

## 5. Windows 常驻行为

- **全局快捷键**用 `RegisterHotKey` 和 `MOD_NOREPEAT`。修改时先尝试注册候选值，成功保存后注销旧值。失败给出原因为占用或系统限制的提示，保留旧值。系统能报告已被全局注册的冲突，普通应用内部快捷键也应在用户常用软件中实测。[RegisterHotKey](https://learn.microsoft.com/en-us/windows/desktop/api/winuser/nf-winuser-registerhotkey)
- **托盘**用 `Shell_NotifyIconW` 与原生右键菜单。隐藏的顶层窗口处理 `TaskbarCreated` 广播，Explorer 重启后重新添加图标。收起只隐藏启动面板，托盘的「退出」释放全局热键、托盘、工作进程和可选手势资源。[托盘图标 API](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyiconw) · [任务栏创建通知](https://learn.microsoft.com/en-us/windows/win32/shell/taskbar#taskbar-creation-notification)
- **登录启动**用当前用户 Startup 文件夹下指向 `WinLaunchpad.exe --background` 的快捷方式。安装默认创建，应用开关负责创建或删除自己的条目。尊重 Windows 启动应用页面中的禁用状态。升级延续选择，卸载清理自己的启动条目。用真实注销和重新登录确认是否生效。[Windows 启动应用设置](https://support.microsoft.com/en-us/windows/experience/startup-boot/configure-startup-applications-in-windows)
- **单实例**按 Windows 用户和登录会话隔离。`--toggle` 向已有实例发出切换命令，进程未运行时启动并显示。`--background` 启动后驻留托盘，重复调用时保持当前窗口状态。内部消息采用有长度上限的命令白名单。热键、托盘和 Quicker 调用均走同一窗口切换逻辑，在原型阶段实测跨进程呼出能否获得搜索焦点，Windows 拒绝切到前台时保留托盘和热键入口。[Windows 窗口前台限制](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setforegroundwindow)

## 6. Quicker 接入和可选内置手势

首版在 Quicker 中保留现有五个轨迹。将原来指向 MyDockerFinder 的启动台动作改为「启动程序」，程序路径填写已安装的 `WinLaunchpad.exe`，参数为 `--toggle`。这样启动台动作与具体热键解耦，也能在启动台尚未运行时打开。[Quicker 快速操作](https://docs.getquicker.net/v2/features/quick-operations/) · [Quicker 鼠标手势](https://getquicker.net/kc/manual/doc/gestures)

| 按住右键的轨迹 | 动作 | 按键或调用 |
| --- | --- | --- |
| `←` 向左滑 | 切到右侧虚拟桌面 | `Ctrl+Win+→` |
| `→` 向右滑 | 切到左侧虚拟桌面 | `Ctrl+Win+←` |
| `↑` 向上滑 | 打开窗口切换器 | `Ctrl+Alt+Tab` |
| `∧` 先右上再右下 | 打开或收起启动台 | Quicker 调用 `--toggle`，内置手势调用 `Toggle` |
| `∨` 先右下再右上 | 打开任务视图，查看窗口和桌面 | `Win+Tab` |

左右滑动沿用图 3 的反向绑定，内置预设允许互换。键盘组合的行为以 Windows 自带窗口切换和任务视图为准。[Windows 键盘快捷键](https://support.microsoft.com/en-us/accessibility/windows/keyboard-shortcuts-in-windows)

内置手势交付后仍默认关闭。启用时由独立消息线程装载 `WH_MOUSE_LL`，在有限缓冲内采样，识别成功后在松开右键时触发一次。回调只做事件采样、透传裁决和排队，识别及动作执行在回调之外。按最短位移区分普通右键与划动，用方向和拐点识别五条轨迹，未知轨迹直接结束。验证窗口切换、屏幕切换和锁屏后状态复位。考虑 `SendInput` 的权限边界，普通右键和手势共用时重点测试菜单、右键拖动和按键释放。复现输入冲突的应用可以设为排除项。Quicker 和 WinLaunchpad 使用同一个右键触发器时由其中一个处理。系统权限限制或兼容性测试失败时，用户可以继续通过 Quicker 接入。[低级鼠标钩子](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelmouseproc) · [SendInput 与 UIPI](https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-sendinput)

## 7. 内存、响应与可靠性验收

**硬门槛是 100 MB（100,000,000 字节）**。对 WinLaunchpad 主进程和所有同时存活的自有临时进程，分别统计**工作集总和**和**私有提交内存总和**，两者各自的峰值均须 ≤ 100 MB。用户点开后启动的第三方应用独立计量。记录专用／共享 GPU 内存作渲染诊断。建议给硬上限留 20 MB 余量，以全场景 ≤ 80 MB、常驻空闲 ≤ 35 MB 作为设计目标。[进程内存统计字段](https://learn.microsoft.com/en-us/windows/win32/api/psapi/ns-psapi-process_memory_counters_ex)

Windows Release 构建用 `GetProcessMemoryInfo` 采样 `WorkingSetSize` 和 `PrivateUsage`，按同时刻汇总。保留生命周期内核计数 `PeakWorkingSetSize` 和 `PeakPagefileUsage` 辅助发现尖峰。100 ms 作为常规采样间隔，临时进程启动、目录交接和贴图生成时补采，包括转发命令的短时实例。若单进程峰值相加超过上限且时序无法证明峰值错开，则补做细粒度采样并确认预算后再放行。交付报告记录系统构建号、DPI、分辨率、应用数量、并发进程及各场景峰值。

图标按可见区域延迟加载，绘制和解码缓存按字节设上限，建议从 8 MB 开始验证。后台队列合并重复请求，快速翻页时优先最新页面。隐藏后释放页面位图和临时绘图资源，保留轻量名称索引。空闲时由消息或目录变更唤醒，绘制按需触发。以显示和隐藏后的内存曲线判断泄漏，避免通过强制清空工作集压低测量值。

| 验收项 | 场景和通过条件 |
| --- | --- |
| 内存和卡顿 | 真实 Windows 11 上覆盖冷启动、登录常驻、扫描和看图标同时进行、手动批量添加、100%／200% DPI、4K 快速翻页和锁屏恢复。用 1,000 条模拟记录压力测试，同时用实际安装的软件验证 Shell 及图标开销。热呼出 P95 目标 ≤ 150 ms，千项搜索 P95 目标 ≤ 50 ms。 |
| 稳定性 | 连续常驻 24 小时，至少开合 1,000 次，穿插搜索、翻页和刷新。主进程零非预期退出、零持续失去响应，句柄、GDI 对象和内存无持续增长。内置手势若交付，在开启状态下重复关键场景。 |
| 异常与恢复 | 故障注入覆盖损坏快捷方式、缺权限、图标提取超时、目录工作进程失败、写入中断、配置损坏、Explorer 重启和睡眠唤醒。面板继续响应，用户设置可恢复。验证工作进程退出后能重新扫描。 |
| 应用目录 | 准备两处开始菜单的快捷方式、仅有 `App Paths` 的程序、打包应用、便携 exe、带参数 lnk、不同路径同名项及卸载／配置入口。验证 Startup 子目录和指向扫描根外的目录链接被跳过，另测一条 32 位注册视图中的独有记录。逐项核对发现、过滤和启动，新增安装项能刷新，隐藏项重启后仍隐藏并可恢复。 |
| 安装与入口 | 按当前用户安装，首次登录仅常驻且有托盘。关闭开机启动后重新登录，确认无常驻实例。验证默认键、`Alt+A` 配置、冲突回退、Quicker 冷热两种调用、重复启动、升级保留选择及卸载清理。 |
| 界面与手势 | 验证鼠标、纯键盘、中文输入法、双屏混合 DPI 和辅助功能。在实际使用 Quicker 的机器上测试五个原有动作。计划启用内置手势时，加测普通右键与右拖行为、五个预设、管理员窗口和全屏应用。 |

24 小时结果表明版本在指定测试范围内的可靠性。发布门槛以零未解决崩溃、零持续卡死和内存硬门槛全部通过为准。

## 8. 里程碑与交接

| 阶段 | 可交付结果 | 进入下一阶段的条件 |
| --- | --- | --- |
| M0 原型与接口 | Rust Release 原型含托盘、快捷键、原生搜索控件、一页真实图标和代表性扫描。附第 4 节要求的运行截图与交互演示。 | 目标 Windows 11 能运行，完成主面板视觉检查和首次内存测量，确认接口及扫描与界面并行时的内存余量。 |
| M1 应用目录与前端并行 | 目录负责人交付自动发现、去重和过滤、添加、隐藏和恢复、启动和缓存。前端负责人凭第 4 节与模拟目录交付原生页面和状态。 | 前端可连接测试数据操作，目录独立通过夹具和真实 Windows 机样本，双方接口联调通过。 |
| M2 首版集成与发布候选 | 托盘、系统设置、登录启动、单实例、Quicker 命令入口、按用户安装包和卸载。 | 真机验收结果满足第 7 节，交付安装包、简短使用说明和可复现的测量记录。 |
| M3 可选手势 | 五个右键手势、总开关、映射设置、应用排除项。 | 通过右键兼容、权限边界和手势开启状态下的资源与稳定性验收。 |

源文件按窗口、系统集成、目录与持久化划分，使用中文注释记录 Windows 约束和关键边界。Agent 完成各自阶段后提交可核对的变更清单与验证证据，并清理测试进程。提交与推送均须获得用户授权。Git 署名使用 **CrazyFigure**，提交信息与交付材料仅保留项目作者署名。
