# Soundpad 跨端协议（v1）

安卓端（MicYou `soundpad` 分支）与桌面端插件 `opss.soundpad` 之间的控制协议。

## 传输层

MicYou 宿主的跨设备插件消息（`docs/plugins/api-reference.md` §跨端通信）：

- 线格式：protobuf `micyou.PluginMessage`，挂在 TCP 控制通道 `MessageWrapper` 字段 7
- 手机 → 桌面：`source = "micyou.android.soundpad"`，`target = "opss.soundpad"`，`topic = "cmd"`，`payload = UTF-8 JSON`
- 桌面 → 手机：插件经宿主 `send_message {"type":"remote","pluginId":<peer>}` 回包。**注意**：宿主该路径会把 wire `topic` 与 `target` 都改写为对端插件 id，因此**两端一律按 `source` + payload 内的 `op` 字段路由**，不依赖 topic
- 桌面端在 `device_connected` 事件时会向广播发一条 `hello`（`req: 0`），手机可据此免轮询立即发现插件
- `correlationId` 不参与本协议（原生插件 ABI 收不到它）；请求-响应靠 payload 里的 `req` 序号回声配对，`req: 0` 表示主动推送

## 命令（手机 → 桌面，`op`）

| op | 参数 | 语义 |
|---|---|---|
| `hello` | `req`, `client?`, `clientVersion?` | 发现 + 保活。桌面回 `hello`（含音效列表与会话快照）。手机连接期间周期发送：未回包 → 隐藏 SoundPad 按钮 |
| `list` | `req` | 请求音效列表（同时触发目录重扫；重扫结果经 `list` 推送） |
| `play` | `req`, `id`, `volume?`(0–2, 默认 1), `loop?`(默认 false) | 播放音效。立即回 `play`（会话 phase=`loading`），解码完成后推 `state`（phase=`playing`） |
| `stop` | `req`, `sessionId?` | 停止指定会话；缺省 `sessionId` = 全部停止。回 `stop` + 推 `state` |
| `seek` | `req`, `sessionId`, `positionMs` | 跳转进度（自动夹取到音轨内）。回 `ok` + 推 `state` |
| `volume` | `req`, `sessionId`, `volume`(0–2) | 会话音量。回 `ok` + 推 `state` |
| `loop` | `req`, `sessionId`, `loop`(bool) | 会话循环开关。回 `ok` + 推 `state` |
| `state` | `req` | 请求会话快照。回 `state` |

## 回包与推送（桌面 → 手机，`op`）

| op | 字段 | 说明 |
|---|---|---|
| `hello` | `req`, `plugin`, `version`, `sounds[]`, `sessions[]` | 握手应答 / 设备连接时的主动广播（`req:0`） |
| `list` | `req`, `sounds[]` | 音效列表（应答或目录变化推送） |
| `play` | `req`, `session` | 播放受理，`session.phase = "loading"` |
| `state` | `req`, `sessions[]` | 会话快照（应答，或播放开始/结束/停止/参数变化时推送 `req:0`） |
| `ok` | `req`, `op` | seek / volume / loop 的确认 |
| `error` | `req`, `message` | 失败原因（unknown sound / all voices busy / unknown session …） |

### `sounds[]` 条目

```json
{ "id": "laugh", "label": "大笑", "durationMs": 2350 }
```

`durationMs` 来自 WAV 头解析；`0` 表示文件存在但头不可解析（播放时会报 `error`）。

### `sessions[]` 条目

```json
{
  "sessionId": 3, "id": "laugh", "label": "大笑",
  "phase": "playing",            // loading | playing | stopping
  "positionMs": 1200, "durationMs": 2350,
  "volume": 1.0, "loop": false
}
```

进度模型：桌面只在**离散变化**时推送 `state`（播放开始、自然结束、停止、参数修改）；`positionMs` 的连续推进由手机端**本地插值**（收到快照后按墙钟外推，`loop` 会话按 `durationMs` 取模），并可发 `state` 轮询纠偏。

## 会话生命周期

```
play ──► loading ──(解码/重采样完成)──► playing ──(自然结束/stop)──► stopping ──(10ms 淡出)──► 移除
              └──(解码失败/取消: error 或静默移除)──► 移除
```

- 同时最多 **16** 个会话（声部槽位）；超员时 `play` 回 `error: "all voices busy (16 max)"`
- 同一音效可重复播放（各自独立会话）
- `sessionId` 由桌面端单调分配，跨连接不复用旧值（手机重连后应以 `hello.sessions` 为准）

## 发现流程（手机端视角）

1. 与桌面端建立连接（`StreamState.Streaming`）后，每 2 s 发 `hello`
2. 收到 `hello` 回包（或桌面广播的 `req:0` hello）→ 判定"桌面端有 soundpad 插件" → 显示底部栏 SoundPad 按钮，缓存 `sounds[]`
3. 连续多次 hello 无回包（默认 3 次）或连接断开 → 隐藏按钮、清空会话
4. 已发现后降为 5 s 保活（桌面插件被禁用/宿主重启会自然掉线）

## 版本与兼容

- 协议 v1：以 `hello` 回包中的 `plugin` / `version` 标识插件身份；未来不兼容变更将换 `topic`（如 `cmd2`）而非复用 `cmd`
- JSON 解析两端都必须**忽略未知字段**、缺省字段取默认值（前向兼容）
