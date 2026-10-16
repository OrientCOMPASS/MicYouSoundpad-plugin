# Soundpad 技术文档

MicYou 原生插件 `opss.soundpad` 的实现说明：需求分解、宿主机制结论、线程模型、实时混音器设计、内存纪律、解码缓存与测试方法。工程范式承自 [Focus-Capture](https://github.com/OrientCOMPASS/Focus-Capture)（同一 ABI 防御性绑定、同一"三线程纪律"、同一 CI 发布链路）。

---

## 1. 需求与目标

> 桌面端插件从安卓端接收控制指令，播放音效并混入麦克风音频流；安卓端可对每条播放控制进度、音量、循环，并支持多条同时播放。

| 子问题 | 方案 |
|---|---|
| 混入麦克风流 | 注册为 DSP 节点（`kind: dsp`，`insertAfter: VAD`，`realtimeSafe: true`），在 `micyou_plugin_process` 中做加性混音。**不用**宿主 `play_sound`：它排队播整只 WAV，无进度/音量/循环/多声部控制 |
| 手机发现插件 | 手机经跨设备 `PluginMessage` 发 `hello`；插件回包（含音效列表）即"确定有 soundpad 插件" |
| 多声部 + 传输控制 | 自研 16 槽位实时混音器（§4），每条播放 = 一个会话 |
| 音效来源 | 插件目录 `sounds/*.wav`，目录扫描 + 头部探测时长；首次运行自动生成演示音效 |
| 桌面本地控制 | panel.html（桥接 `get_config`/`trigger`），与手机端共用同一会话状态机 |

## 2. 宿主机制结论（实现依据）

来自 MicYou 源码（master @ 2.1.1）与 `docs/plugins/*` 的核对，与 Focus-Capture TECHNICAL §2 一致的部分从简：

- **DSP 链**固定 48 kHz；`process(data, samples, channels, queued_ms, bypass)`，`samples` 为交错 f32 总数；`bypass=1` 宿主保留原帧；链尾统一 soft-clip → 插件只做线性加性混音
- **消息分发锁模型**：dispatcher 对插件实例 `try_lock`（忙则丢消息），音频线程对同一把锁**阻塞 lock** → `handle_message` 必须微秒级返回，**任何文件 I/O / 解码都不允许出现在其中**（连 `interval:tick` watchdog 也走这把锁）
- **Host API 线程约束**：只允许在 `init`/`deinit`/`handle_message`/`handle_event`（宿主派发线程）调用；`process` 内禁止
- **跨端消息**：手机侧 `MessageWrapper.pluginMessage`（proto 字段 7）→ 桌面 `bus.handle_incoming` → dispatcher 按 `target` 路由到插件；插件 `send_message {"type":"remote","pluginId":X}` 回手机时，宿主把 wire `topic`/`target` 都写成 X → **协议按 `source`+`op` 路由**（docs/PROTOCOL.md）
- 原生插件 `handle_message(source, topic, payload, len)` **拿不到 correlationId** → RPC 用 payload 内 `req` 回声配对
- 插件 id 仅小写字母数字 `.` `-`；`entry` 无后缀自动补 `.dll/.so/.dylib`；产物去 `lib` 前缀
- `device_connected`/`device_disconnected` 事件经 `handle_event(type, json)` 派发 → 连接即广播 `hello`，手机免等轮询
- 面板 `trigger {action,payload}` → 插件收到 topic `ui:<action>`，payload 可能是桥接包装 `{"action":…,"payload":{…}}` 或裸参数对象 → 两种形态都解析（`protocol::parse_ui_action`）
- `config:changed` 消息（宿主在 `set_plugin_config` 后派发）→ 热加载 masterVolume / labels

## 3. 线程模型

| 线程 | 职责 | 触碰的资源 | Host API |
|---|---|---|---|
| 宿主总线线程（init / deinit / handle_message / handle_event / interval tick） | 会话注册表、命令解析、回包、watchdog 对账、面板状态写入 | `STATE` 互斥锁、mixer 槽位控制面（reserve/stop/seek/gain/reclaim）、mpsc 发任务 | ✅ 仅此处（init/deinit 允许文件 I/O；消息处理路径零文件 I/O） |
| 解码 worker（`soundpad-decode`，单线程） | WAV 解码、重采样到 48k、目录重扫、轨道缓存、`publish`/`fail` 槽位交接 | `JobResult` 原子量、`SoundRegistry`（Mutex<Vec> + version 计数）、mixer 槽位 LOADING 面 | ❌ 绝不调用 |
| 宿主实时音频线程（process） | 混音 | 槽位音频面（state/track 指针/pos/gain/loop/env） | ❌ 绝不调用，零分配零锁 |

`handle_message` 的成本审计：JSON 解析（≤几百字节）+ 注册表 Vec 查找（≤16 项）+ 原子操作 + mpsc `send`（有界队列非阻塞）+ `send_message`（宿主 `try_send`）。无文件 I/O、无解码、无大分配（回包 JSON 序列化 ~1 KB 级，在总线线程可接受，与 Focus-Capture 的 write_status 同级）。

## 4. 实时混音器（mixer.rs）

### 4.1 槽位状态机

16 个静态槽位，每个槽位一组原子量 + 一个音频线程私有的包络 `UnsafeCell<f32>`：

```
EMPTY ──reserve(总线)──► LOADING ──publish(worker, CAS)──► ACTIVE ──┬─ begin_stop(总线) ─► STOPPING ─(env→0, 音频)─► EMPTY
                            │                                      └─ 自然播完(音频: 检测到 pos≥len) ─► STOPPING ─► EMPTY
                            └─ cancel/fail(worker) ─► EMPTY
```

- **谁写 state**：LOADING 面只归 worker（publish/fail），ACTIVE→STOPPING 只归总线或音频（自然结束），STOPPING→EMPTY 只归音频线程（淡出完成）——单写者纪律避免 CAS 环
- **交接内存序**：worker 先写 track 指针/len（Relaxed）再 `state: LOADING→ACTIVE`（Release）；音频线程 `state.load(Acquire)` 成功后才读指针 → happens-before 成立
- **回收**：`reclaim` 只在总线线程、且 `state==EMPTY`（Acquire）时把 `Box<Arc<Track>>` 指针还原释放；音频线程从不 free

### 4.2 process 快路径

- 全槽 EMPTY → `bypass=1`（宿主原样保留帧，零成本）
- 每槽每帧：包络斜坡（10 ms = 480 样本线性）→ 读 `track.samples[pos]` → `gain × master × env` 加性写入 → `pos += 1`；循环轨 `pos %= len`；自然结束转入淡出（无源数据只骑包络到 0）
- 声道映射：单声道轨 → 复制到全部输出声道；立体声轨 → 单声道输出取均值、双声道输出 L/R 直映；>2 声道在**解码期**就降混为 ≤2（音频线程分支数固定）
- 每帧成本：≤16 次原子 load + FMA 级混音，无分支预测陷阱、无分配、无锁

### 4.3 浮点边界（实测教训）

480 次 `env -= 1/480` 的累减在 f32 下**不精确归零**（残留 ~1e-8），会让已结束的声部多活一个调用周期、槽位晚一拍回收（单测抓到）。修复：淡出侧 `env - target < 1e-4` 时直接吸附到 target。另一个边界：音轨恰好在调用末尾播完时，`pos == len` 会留到**下一次**调用才触发自然结束检测——行为正确（淡出晚 ≤10 ms 开始），文档化于此。

## 5. 解码 worker（decode.rs）

- **命令队列**：`mpsc`，总线线程非阻塞 `send`；worker `recv_timeout(200ms)` 兼做空闲期目录重扫（重扫间隔 ≥2 s，rescan 请求先到先记账后执行，不丢请求）
- **解码链**：WAV 头遍历（跳 LIST 等填充块）→ PCM 8/16/24/32 与 float 32/64 → f32 交错 → >2ch 降混 → 非 48 kHz 时 SincResampler（8-tap Hann 窗 sinc，512 相位查表，与 Focus-Capture 同款，分块喂入保持边界连续）
- **轨道缓存**：键 = `路径|mtime|大小`，值 = `Arc<Track>`；上限 192 MiB，超限 FIFO 淘汰。淘汰只移除缓存项——在用会话自持 Arc，释放发生在最后一个持有者（总线线程 reclaim）处，音频线程永不 free
- **失败路径**：解码失败 → `mixer::fail(slot)`（LOADING→EMPTY）+ `JobResult{phase: FAILED, error}`；watchdog 摘除会话、必要时回 `error` 给手机
- **取消竞态**：LOADING 期间 stop → 总线置 `cancel`；worker publish 前查 cancel（是→槽位直接 EMPTY），CAS 成功后再查一次（补窗口期），双检收敛

## 6. watchdog 与状态推送

`set_interval(250ms, "spwd")`，tick 在总线线程：

1. **会话对账**：Loading→(JOB_PUBLISHED)→Playing（推 `state` 给手机）；Loading 失败/超时（20 s）→ 摘除 + 记错；Playing 且槽位 EMPTY（自然播完）→ 摘除 + 推 `state`；Stopping 且槽位 EMPTY → 摘除 + reclaim
2. **注册表版本**变化（worker 重扫发现文件增删）→ 推 `list` 给手机
3. 每 8 tick（~2 s）：重读 masterVolume 配置（JSON 编辑器改的也能生效）+ 触发重扫
4. 有离散变化才 `push_state` + 写 `sp_state` 配置（面板轮询源；**按变化写**而非按 tick 写，控制宿主 I/O）

进度连续推进不靠推送：手机端本地插值 + 按需 `state` 轮询（PROTOCOL.md §sessions）。

## 7. 生命周期与双进程现实

- `init`：拷贝 host 表（按值）→ `mixer::reset_all()`（宿主保证 DSP 链未挂载，无并发 process）→ 解析 plugin_dir → 空目录时生成演示音效（init 允许文件 I/O）→ 读配置 → 同步首扫 + 启动 worker → arm watchdog → 面板图标 + 首写 `sp_state`
- `deinit`：**必须** join worker（库即将被卸载，残留线程执行本库代码 = 宿主崩溃；预算 6 s）→ clear_interval → `mixer::reclaim_all()`（强制回收全部轨道）→ 清 host 表
- GUI 与 CLI 同时运行时两进程各自加载本插件：手机 `hello` 会收到**两份**回包（会话各自独立、混音各自进行，声音只在真正串流的那个进程生效）。手机端按 `source` 幂等处理回包即可；该行为与 Focus-Capture 的双进程约束一致，属宿主既有现实

## 8. 安全与健壮性

- 所有 FFI 入口 `catch_unwind`（panic 不跨 ABI 边界；cdylib 保持 unwind，**绝不** `panic=abort`）
- host 表扩展槽位全部 `Option<fn>` 判空后调用；结构体在 `set_panel_icon` 截止（不越读 API v2 字段）
- 音效路径不出 `sounds/` 目录（列表来自目录扫描；`play` 按 id 查表，**不接受**路径参数）
- 会话音量夹取 0–2、主音量夹取 0–2、JSON 命令字段全校验（`protocol.rs` 单测覆盖）
- 文件尺寸上限 512 MiB、缓存上限 192 MiB，恶意/超大文件不致 OOM

## 9. 测试

`cargo test --release`（31 用例，CI 每平台跑）：

- **mixer**：空闲 bypass、发布/自然结束/淡出回收、循环回绕、seek 夹取、会话音量与主音量、单声道↔立体声映射、取消竞态、双声部加性（峰值 2×）、槽位满员。共享静态槽位表 → 全部用例经测试锁串行 + 每用例前 `reclaim_all` 清场（防失败级联）
- **wav**：PCM16 编解码往返、立体声交错、float32/PCM24、LIST 填充块跳读、非 WAV 拒绝、演示音生成
- **decode**：目录扫描（大小写扩展名/时长探测/label 覆盖）、44.1→48k 重采样与缓存命中（`Arc::ptr_eq`）、worker 端到端（解码→publish→槽位 ACTIVE）、坏文件报错路径
- **protocol**：全部 op 解析、默认值/夹取、垃圾输入拒绝、面板动作两种包装形态、回包构造
- **resample**：恒等/上采样/下采样/分块边界连续性（承自 Focus-Capture）

音频线程行为无法在单测中真并发复现，采用"直接驱动 `process_slice`"模型测试（单线程即音频线程语义）；跨平台产物由 CI 矩阵构建把关（windows/linux/macos×2 四目标 + lipo universal）。

## 10. 已知限制

- zip 单包每平台只能带一个产物 → Linux 仅 x86_64（Windows on ARM 走仿真、macOS universal 双架构）
- 仅 WAV：有意零解码依赖（mp3/ogg 需引入大依赖或宿主 API）
- `hello` 广播在 GUI+CLI 双进程下收到两份（§7）
- WASM 运行时不可用本插件形态（realtimeSafe DSP + 跨端消息为 native 专属组合）
