# 🎵 Soundpad — MicYou 手机遥控音效板插件

在 **MicYou 安卓端**一键播放桌面端音效库里的声音，直接**混入麦克风流**：通话 / 直播对方都能听到。支持**多音效同时播放**，手机端可对每条播放单独控制**进度（seek）、音量、循环、停止**。可选**播放到耳机**（默认开启）：音效同时在本机默认输出设备出声，**无需打开宿主监听**，也不会回放自己的麦克风；未串流时也能本地试听。

与官方 `native-soundpad` 示例（宿主 `play_sound` 排队播放 WAV，不可控进度）不同，本插件**自带实时混音器**：注册为 DSP 节点，在 `micyou_plugin_process` 中做加性混音，每条声音是一个独立"播放会话"，完整暴露传输控制给手机端。

| | |
|---|---|
| 插件 id | `opss.soundpad` |
| 运行时 | Native（cdylib，C ABI v1 / apiVersion 1） |
| 平台 | **Windows（x86_64）· Linux（x86_64）· macOS（universal：arm64 + x86_64）**；单包分发，宿主按平台自选 |
| 插件类型 | `dsp`（实时处理链节点，`realtimeSafe: true`，`insertAfter: VAD`） |
| 权限 | `dsp.node`、`config.read`、`config.write`、`message.send`、`control.observe`（宿主监听状态，用于自动暂停本地出声）、`device.list`（无网络/剪贴板权限） |
| 音频格式 | WAV · MP3 · FLAC · OGG (Vorbis) · M4A/AAC（纯 Rust 解码，[symphonia](https://github.com/pdeljanov/Symphonia)） |
| 依赖 | 无外部运行库（纯 Rust + 系统库） |
| 安卓端 | [OrientCOMPASS/MicYou](https://github.com/OrientCOMPASS/MicYou) `soundpad` 分支（连接桌面端后自动出现 SoundPad 按钮） |
| 许可 | Unlicense（公有领域） |

## 安装

1. 下载 Release 中的 `plugin.zip`；开发版见每次 push 构建的 Actions artifact `soundpad-development`
2. 把 zip 内容解压到插件目录的 `opss.soundpad/` 子目录（Windows `%APPDATA%\micyou\plugins\`，Linux/macOS `~/.config/micyou/plugins/`）；包内含 `soundpad.dll / soundpad.so / soundpad.dylib`，**宿主按平台自动选用**
3. 回到插件页点「刷新」，启用 **Soundpad**
4. 启用后宿主会自动把合成节点 `Plugins` 插入处理链

## 使用

### 音效库

- 音效目录可在插件设置里**自定义**（`soundsDir`，绝对路径；留空 = 插件目录下 `sounds/`），修改约 2 秒内生效并同步到手机端与面板
- **支持子文件夹分类**：递归扫描（深度 ≤8、≤4096 个文件），手机端面板呈现文件夹导航（进入/返回上级），音效 id = 相对路径（如 `memes/laugh`）
- 音效 = 音效目录内的音频文件：**WAV**（PCM 8/16/24/32-bit、float 32/64-bit）· **MP3** · **FLAC** · **OGG**(Vorbis) · **M4A/AAC**，任意采样率/声道数，插件内自动重采样到 48 kHz
- 首次启用时若 `sounds/` 为空，自动生成 3 个演示音效（beep / ding / chime）
- 增删文件后点面板「重新扫描」（插件每 ~2 秒也会自动扫描一次），手机端音效列表随之更新
- 其它格式（如 Opus）可先转换：`ffmpeg -i in.opus out.mp3`
- 音效 id = 文件名主干（`laugh.mp3` → `laugh`；同 id 冲突时按文件名序取先者）；显示名可在插件设置 `sounds` 配置里用 `{"id":"laugh","label":"大笑"}` 覆盖

### 手机端（安卓）

1. 手机与桌面端连接（Wi-Fi / USB）且桌面端已**启用**本插件
2. 手机底部栏自动出现 **SoundPad** 按钮 → 点开面板
3. 点卡片 = 播放（可同时播多条）；选中正在播放的卡片后可**拖动进度条 seek**、调**音量**、开**循环**、**停止**
4. 桌面端音效库变化时手机列表自动刷新
5. **手机断开连接时桌面端自动停止全部播放**（主控端离开即收声；桌面面板本地播放不受影响）

### 桌面端面板

设置侧边栏 🎵 Soundpad：音效网格点击播放、当前会话进度/停止、全部停止、主音量、重新扫描。

> **混音只在串流运行时送达对方**：MicYou 需要已连接手机/Web 端（音频线程在处理链上调用本插件）。未串流时播放命令仍会建立会话，但声音在开始串流后才会真正混入。

## 跨端协议（摘要）

手机 → 桌面：`PluginMessage { source: "micyou.android.soundpad", target: "opss.soundpad", topic: "cmd", payload: JSON }`；桌面 → 手机经宿主 `send_message`（remote）回包。JSON 命令：`hello / list / play / stop / seek / volume / loop / state`，回包与主动推送：`hello / list / play / state / error`。完整定义见 [docs/PROTOCOL.md](docs/PROTOCOL.md)。

`hello` 兼作发现与保活：手机连接后周期询问，收到回包才显示 SoundPad 按钮（= "确定桌面端有 soundpad 插件"）。

## 配置

| 键 | 默认 | 说明 |
|---|---|---|
| `masterVolume` | `1.0` | 主音量（0 – 2，混入麦克风流 = 对方听到的响度），热生效 |
| `localOutput` | `true` | **播放到耳机**：本机默认输出设备直接出声（独立于宿主监听开关；未串流时也可试听）。热生效（设备开合在解码 worker 线程完成，不阻塞音频） |
| `localVolume` | `1.0` | 耳机输出独立音量（0 – 2），与主音量互不影响 |
| `soundsDir` | `""` | 自定义音效目录（绝对路径）；留空 = `<插件目录>/sounds/`。热生效（自动重扫并推送新列表到手机端） |
| `sounds` | `[]` | 可选显示名覆盖：`[{"id":"beep","label":"哔"}]`（文件列表以 `sounds/` 目录扫描为准） |
| `sp_state` | — | 插件写入的状态快照（面板轮询用，勿手改） |

## 构建

```bash
cargo build --release      # 产物 target/release/libsoundpad.{so,dylib} / soundpad.dll
cargo test --release       # 单元测试（混音数学 / 多格式解码 / 协议解析 / ring / monitor）
```

CI（GitHub Actions）：push → 四目标矩阵构建 + 单测 + `soundpad-development` artifact；手动 workflow_dispatch → 打 tag 发 Release（`plugin.zip` + `plugin.json`）。macOS 两架构经 `lipo` 合成 universal dylib。

## 实现要点

- **三线程纪律**（与 Focus-Capture 同源）：总线线程（`handle_message`/watchdog，唯一可调 Host API 处，微秒级返回）· 解码 worker（文件 I/O / 解码 / 重采样，绝不碰 Host API）· 宿主实时音频线程（`process` 只做无锁无分配的混音）
- **16 声部槽位表**：原子状态机 `EMPTY → LOADING → ACTIVE → STOPPING → EMPTY`，轨道以 `Arc<Track>` 泄漏指针交接，回收只发生在总线线程且以 Release/Acquire 与音频线程握手
- **10 ms 淡入淡出**包络，起停无咔哒；链尾宿主统一 soft-clip，插件只做线性加性混音
- 解码缓存（≤192 MiB，按 路径+mtime+大小 键控），同一音效重播零解码
- 非 WAV 格式经 symphonia 解码（MP3/FLAC/OGG Vorbis/AAC-M4A，纯 Rust 零 C 依赖）；时长列表用容器元数据探测（MP3 Xing / FLAC STREAMINFO / OGG 末页 granule / M4A），无需整曲解码
- **播放到耳机**（monitor.rs，cpal 跨平台输出）：宿主串流中 → 实时线程把「预主音量」的单声道混音写入 SPSC tap ring，cpal 回调消费（40ms 预热 + 100ms 延迟钳制吸收时钟漂移）；宿主空闲 → cpal 回调经信标判定（150ms 冻结）切换为**自渲染**（同一槽位机，时间门控保证单一渲染者），离线也能试听。设备开合/play/pause 全部在解码 worker 线程（总线线程零设备 I/O），3 秒空闲滞回自动暂停
- 设计细节见 [docs/TECHNICAL.md](docs/TECHNICAL.md)

## 已知限制

- Linux aarch64 未提供构建（zip 单包每平台只能携带一个 `.so`）；Windows on ARM 经 x86_64 仿真运行
- 「播放到耳机」固定使用**系统默认输出设备**（不跟随宿主耳返的设备选择；插入/拔出设备后需重新启用插件或改动 localOutput 开关触发重开）
- 宿主串流启动瞬间（≤1 个回调周期 ~10ms）本地出声可能与混入麦克风的信号轻微重叠推进，听感无异常
- 不支持 Opus（symphonia 无 Opus 解码器；.opus 请先转 MP3/OGG-Vorbis）
- 播放进度推送为"离散变化即推 + 手机端本地插值/轮询"，非逐帧同步
- 宿主全局快捷键（`register_hotkey`）仅 X11：本插件未注册快捷键，控制在手机/面板端完成
