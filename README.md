# 局域网串玩

用 Android 手机玩 Windows 电脑上的游戏。主机发送对应的游戏窗口和声音，手机回传 Xbox 手柄或键鼠操作。

## 连接

1. 让电脑与手机连接同一个局域网。把 `lan-stream-host.exe` 和 `lan-stream-games.json` 放在同一目录，双击启动电脑主机。
2. 在电脑打开对应游戏并保持窗口在前台。窗口化和无边框窗口适合采集。
3. 手机上打开游戏专用 APK。App 会自动在局域网发现已配对的主机并进入对应游戏。连接中断时会重试。

Windows 端通过 Windows Graphics Capture 取画，依赖当前电脑 PATH 中的 FFmpeg 编码 H.264。可把完整的 `ffmpeg.exe` 路径设到 `LAN_STREAM_FFMPEG`。主机会探测 NVIDIA 和 AMD 硬件编码，必要时使用 `libx264`。

## 串流质量和延迟

- 默认优先让视频走 UDP。交错发送视频分片，每个校验组可恢复一片丢失。手机丢弃过时帧，遇到无法恢复的帧就等下一个关键帧。UDP 不通时自动切回 TCP。配对、手柄按钮、音频和画幅信息走可靠连接，支持时 Xbox 摇杆用带序号的 UDP 最新状态，超时释放方向。
- 主机复用 D3D11 采集纹理和像素缓冲，H.264 硬编码关闭帧重排和前瞻。新版会压缩 48 kHz 立体声音频，Android 用硬件解码并按时间戳安排显示。
- `--quality latency` 优先在 NVIDIA 适配器上采集 D3D11 纹理，直接提交 NVENC。游戏画面在 GPU 上裁出窗口内容。黑边检测只取样少量行列。显卡、驱动或窗口尺寸不支持直送时，由 FFmpeg 路径接力。Android 使用 SurfaceView 即时提交最新帧。游戏显示器达到 120 Hz 时，该档位以 120 fps 为目标，手机会请求匹配的刷新率。UDP 无法稳定启用时切回低码率 60 fps。
- `--quality auto` 会在 UDP 启用时选择 `balanced`，在 TCP 下选择 `smooth`。`smooth`、`balanced` 和 `sharp` 也可以手动选。画质档位只调整编码尺寸和码率，显示仍按游戏比例适配。

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
.\build-android.ps1 -Exe 'C:\Games\Brotato\Brotato.exe' -GameName '土豆兄弟' -AppSlug brotato -Xbox
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
```

## 许可证

项目源码采用 [MIT](LICENSE) 许可证。游戏通过各自的发行渠道安装。在实际构建与运行中使用的第三方工具和依赖沿用它们各自的许可证。

`host/native/nvEncodeAPI.h` 来自 FFmpeg 的 [nv-codec-headers n12.2.72.0](https://github.com/FFmpeg/nv-codec-headers/tree/n12.2.72.0)，文件内保留 NVIDIA 的许可证声明。
