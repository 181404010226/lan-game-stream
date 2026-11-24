# 局域网串玩

用 Android 手机玩 Windows 电脑上的游戏。主机发送对应的游戏窗口和声音，手机回传 Xbox 手柄或键鼠操作。

新增游戏、引擎插件或显卡编码后端时，可先查看当前取帧路径、回退条件和代码入口。[模块架构图与扩展指南](docs/architecture.md)

## 连接

1. 让电脑与手机连接同一个局域网。把 `lan-stream-host.exe` 和 `lan-stream-games.json` 放在同一目录，双击启动电脑主机。
2. 在电脑打开对应游戏并保持窗口在前台。窗口化和无边框窗口适合采集。
3. 手机上打开游戏专用 APK。App 会自动在局域网发现已配对的主机并进入对应游戏。连接中断时会重试。

Windows 主机默认通过 Windows Graphics Capture 取画，也支持从 Godot 插件读取游戏纹理。启动时会用 FFmpeg 试编码，依次探测 NVIDIA NVENC、AMD AMF 和 `libx264`。将 `ffmpeg.exe` 加入 PATH，或用 `LAN_STREAM_FFMPEG` 指定其完整路径。

## 串流质量和延迟

- 默认优先让视频走 UDP。交错发送视频分片，每个校验组可恢复一片丢失。手机丢弃过时帧，遇到无法恢复的帧就等下一个关键帧。UDP 不通时自动切回 TCP。配对、手柄按钮、音频和画幅信息走可靠连接，支持时 Xbox 摇杆用带序号的 UDP 最新状态，超时释放方向。
- 主机复用 D3D11 采集纹理和像素缓冲，H.264 硬编码关闭帧重排和前瞻。新版会压缩 48 kHz 立体声音频，Android 用硬件解码并按时间戳安排显示。
- `--quality latency` 优先在 NVIDIA 适配器上采集 D3D11 纹理。GPU 裁出游戏窗口，将 BGRA 转为 BT.709 有限范围 NV12，再直接提交 NVENC。黑边检测只取样少量行列。显卡或驱动不支持这条路径时，由 FFmpeg 接力。Android 使用 SurfaceView 即时提交最新帧。游戏显示器达到 120 Hz 时，该档位以 120 fps 为目标，手机会请求匹配的刷新率。UDP 无法稳定启用时切回低码率 60 fps。
- `--quality auto` 会在 UDP 启用时选择 `balanced`，在 TCP 下选择 `smooth`。`smooth`、`balanced` 和 `sharp` 也可以手动选。画质档位只调整编码尺寸和码率，显示仍按游戏比例适配。
- `--quality native` 在新版 APK 连接时，根据手机尺寸和游戏画幅调整窗口并输出原生像素。固定比例的游戏可以通过 `-NativeAspect '16:9'` 指定内容比例。例如 2800×1260 的手机显示 2240×1260 的 16:9 内容，两侧可放触控。该档位在 NVIDIA 直送路径用 18 Mb/s 和空间自适应量化保留细节。旧 APK 自动选择低延迟档，窗口尺寸仍受电脑可用空间和游戏设置限制。

从 v0.4.5 开始，D3D11 直送路径优先使用 NVENC 完成事件。主机会轮换四组 NV12 纹理和码流缓冲，最多同时提交两帧，让下一帧的采集和色彩转换与上一帧的编码重叠。主机窗口会显示实际采用的异步或同步模式。已有 v0.4.4 手机客户端可以直接连接新版主机。

v0.4.7 改进了 Wi-Fi 突发丢帧后的恢复。使用新版手机端和原生 NVENC 主机时，手机可以立即请求关键帧。UDP 发送端对数据和校验包一起分批，并向 Windows 请求音视频流量优先级。Android 10 及以上在前台串流时请求低延迟 Wi-Fi。低延迟解码按设备能力预留追帧余量，显示仍采用已协商的帧率。日志中的 `max-receive-gap-us`、`max-recovery-us` 和 `max-gap-us` 分别记录完整帧到达、恢复参考帧和显示回调的尾部耗时。

### Godot 游戏内取帧（v0.4.6 体验版）

Windows x64 游戏可通过 Godot 3 的 GDNative 单视口接口，把场景渲染到手机画幅的离屏纹理。Godot 插件在呈现之前分流，同一帧也会按比例显示在电脑窗口。主机从三张带 DXGI 键控互斥量的共享 BGRA 纹理中取最新画面，再用原有的 BT.709 转色和异步 NVENC 编码。OpenGL 直接写入共享纹理时显示「游戏内直读」。Godot 3 的二维 Canvas 经常写入自己的帧缓冲，插件核对实际颜色附件后会使用一次 GPU 中转复制，手机菜单显示「游戏内复制」。

使用 `render-bridge/build-adapter.ps1` 构建插件，需要 CMake 与 Visual Studio x64 C++ 工具链。首次构建会下载固定版本的 Godot 3.6.1 C 接口头文件并校验 SHA-256，也可通过 `-GodotHeaders` 指定本地头文件目录。退出游戏，把产物 `LanStreamRenderer.dll` 放在游戏 EXE 旁。支持用 Godot Mod Loader 加载本地 ZIP 的游戏，可将 `LanStream-Offscreen.zip` 放入加载器配置的模组目录。

Steam 版《土豆兄弟》1.2.0.3 已实测可通过 Godot 自动加载插件。将仓库中的 `render-bridge/mod/mod_main.gd` 复制到游戏目录的 `LanStream/mod_main.gd`。在 `Brotato.exe` 旁创建或编辑 `override.cfg`，将示例路径改为脚本的实际绝对路径。已有 `[autoload]` 节时，直接在该节内追加 `LanStreamOffscreen` 一行。每个游戏使用一种加载方式。

```ini
[autoload]
LanStreamOffscreen="D:/SteamLibrary/steamapps/common/Brotato/LanStream/mod_main.gd"
```

主机默认使用 `--capture window`。用 `--quality native --capture game` 开启实验路径，缺少游戏插件时主机会给出提示。`--capture game-copy` 用于对照中转复制；`--capture auto` 在启动时尝试游戏纹理，并可降级到窗口采集。手机右上角串流菜单显示当前路径，日志中的 `p95-gap-us` 表示解码器渲染回调或显示表面更新间隔的第 95 百分位，单位为微秒。

## 触控和画幅

- 手机右上角 `⋯` 进入「编辑触控布局」。拖动控件改变位置，点按控件修改键位、名称和大小。可以另加按键。每个游戏 APK 单独保存布局和画幅。
- 《土豆兄弟》使用虚拟 Xbox 360 手柄，默认只显示模拟左摇杆、A、B 和菜单键。电脑端通过 ViGEmBus 创建手柄。可为别的游戏选择键鼠布局，默认左摇杆发送 `WASD`，点画面操作鼠标。
- 默认「自动铺屏」会按手机横屏比例调整可缩放的游戏窗口，识别稳定黑边，然后把完整游戏画面等比显示到最大。游戏在宽窗口里仍按固定比例渲染时，手机会留出适量空白。「完整画面」显示原始串流画面，「两侧控件」独立留出空间，「铺满裁切」等比放大内容。调整的窗口在断开时尝试恢复。
- 游戏失去焦点、手机离开页面或串流断开时，主机会释放当前由手机按住的键。

## 构建

Windows 构建需要 Rust MSVC 和 C++ Build Tools。Android 构建需要 PowerShell 7、Rust Android 目标、JDK 17、Android SDK 35、Build Tools 35 或更新版本和 NDK。将实际游戏路径传给 `-Exe`，或通过主机的 `--list` 查看已打开游戏的 EXE 路径。

```powershell
rustup target add aarch64-linux-android x86_64-linux-android
cargo test --locked -p stream-wire -p lan-stream-host
cargo build --locked --release -p lan-stream-host
.\build-android.ps1 -Exe 'C:\Games\Brotato\Brotato.exe' -GameName '土豆兄弟' -AppSlug brotato -Xbox -NativeAspect '16:9'
Copy-Item -LiteralPath '.\target\release\lan-stream-host.exe' -Destination '.\build\lan-stream-host.exe'
```

构建另一个游戏时，传入实际游戏 EXE、显示名称和新的 `-AppSlug`。脚本生成独立的 Android 包名，并将绑定写入同一个主机配置。主机启动时读取配置，新增游戏后重启一次主机。`-HostHint` 可提供局域网地址作自动发现失败时的备用地址。

带 `-Xbox` 构建的包使用 [ViGEmBus](https://github.com/nefarius/ViGEmBus/releases/tag/v1.22.0)。新电脑需要已有 Windows 虚拟手柄驱动，键鼠模式只依赖系统输入 API。

电脑程序在 `target\release`，APK 和默认配对配置在 `build`。APK 的网络和输入协议使用 Rust，Android 界面、解码和声音播放调用平台 API。可以用 `-Sdk` 和 `-Jdk` 指定构建工具路径。

构建脚本为每个游戏生成配对配置。备份 `build\lan-stream-games.json` 与 `build\lan-stream-signing.p12` 以便覆盖安装。配对配置和编译后的 APK 是绑定在一起的本地文件。

### 主机命令

```powershell
.\lan-stream-host.exe --list
.\lan-stream-host.exe --bind 192.168.1.20 --port 24865
.\lan-stream-host.exe --quality latency
.\lan-stream-host.exe --quality native
```

## 许可证

项目源码采用 [MIT](LICENSE) 许可证。游戏通过各自的发行渠道安装。在实际构建与运行中使用的第三方工具和依赖沿用它们各自的许可证。

`host/native/nvEncodeAPI.h` 来自 FFmpeg 的 [nv-codec-headers n12.2.72.0](https://github.com/FFmpeg/nv-codec-headers/tree/n12.2.72.0)，文件内保留 NVIDIA 的许可证声明。
