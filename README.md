# 局域网串玩

用 Android 手机玩 Windows 电脑上的游戏。主机发送对应的游戏窗口和声音，手机回传 Xbox 手柄或键鼠操作。

## 连接

1. 让电脑与手机连接同一个局域网。把 `lan-stream-host.exe` 和 `lan-stream-games.json` 放在同一目录，双击启动电脑主机。
2. 在电脑打开对应游戏并保持窗口在前台。窗口化和无边框窗口适合采集。
3. 手机上打开游戏专用 APK。App 会自动在局域网发现已配对的主机并进入对应游戏。连接中断时会重试。

Windows 端通过 Windows Graphics Capture 取画，依赖当前电脑 PATH 中的 FFmpeg 编码 H.264。可把完整的 `ffmpeg.exe` 路径设到 `LAN_STREAM_FFMPEG`。主机会探测 NVIDIA 和 AMD 硬件编码，必要时使用 `libx264`。

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
```

## 许可证

项目源码采用 [MIT](LICENSE) 许可证。游戏通过各自的发行渠道安装。在实际构建与运行中使用的第三方工具和依赖沿用它们各自的许可证。
