# 豆包 (Volcano Engine SeedASR) 云端 ASR 接入指南

Handy 自 0.8.x 版本起,在原有本地推理引擎之外,新增了**字节跳动火山引擎豆包流式语音识别 2.0**(SeedASR 2.0)作为第一个云端转录引擎。本文档面向 Handy 终端用户,介绍如何申请凭据、配置应用以及排错。

## 适用人群

如果你符合以下任意一条,推荐试用豆包云端模型:

- 需要更高的中文/中英混合识别准确率
- 不希望占用本地磁盘 1GB 以上的模型文件,但能接受网络往返延迟
- 经常使用粤语 / 日语 / 韩语等多语种,且不想下载多个本地模型

如果你**离线使用、追求最低延迟、注重隐私**,请继续使用 Handy 内置的 Whisper / Parakeet / Cohere 等本地模型。

## 第一步:在火山引擎控制台申请 API Key

1. 注册并登录 [火山引擎控制台](https://console.volcengine.com/)
2. 进入 **语音识别 - 大模型录音文件识别 / 流式语音识别** 页面
3. 在 **API 鉴权** 选项卡复制 `X-Api-Key`(**新版控制台**才会展示此字段;若你看到的是 `App ID + Access Key + Secret`,说明仍在旧版控制台,需要先切换到新版)
4. 记下你需要的资源类型:
   - `volc.seedasr.sauc.duration` —— 流式语音识别 2.0(**小时版**),按音频时长计费,用多少花多少,**Handy 默认值**
   - `volc.seedasr.sauc.concurrent` —— 流式语音识别 2.0(**并发版**),按并发数包年,适合大规模部署

> 注意:Handy 采用**边录边传**的双向流式优化端点(`bigmodel_async`)做转录——按下快捷键的那一刻就建立连接,录音过程中把音频实时上传、服务端边收边算,松手时只需收尾。相比"录完整段再上传",这把"松手 → 出字"的尾部延迟压到最低。「测试连接」按钮则走 `bigmodel_nostream` 端点(配置即时回 ACK,便于校验凭据)。如需"录音时实时上屏"的逐字效果,请到 [GitHub Discussions](https://github.com/cjpais/Handy/discussions) 留言。
>
> 若流式会话因网络异常中断,Handy 会自动回退到"整段批量转写"以保证不丢结果。

## 第二步:在 Handy 中配置凭据

> 全新安装的用户也可以在**首次启动的模型引导页**直接完成本步:引导页底部「或使用云端模型」区域点开豆包卡片,填入 API Key 后点「使用此模型」即可,无需先下载本地模型。

1. 启动 Handy,从托盘图标打开主窗口
2. 进入 **Settings → Models**(模型设置)
3. 在模型卡片列表里找到 **Doubao SeedASR 2.0**(右侧带云图标 ☁️),点击卡片切换为当前模型
4. 卡片下方会展开凭据面板:
   - **API Key (X-Api-Key)** —— 粘贴第一步拷贝的密钥(密码框,默认隐藏)
   - **Resource ID (X-Api-Resource-Id)** —— 选择 "Streaming ASR 2.0 (Hour)" 或 "Streaming ASR 2.0 (Concurrent)"
5. 离开输入框后(失焦)凭据会自动保存
6. 点击 **测试连接**,正常时显示 ✅ 成功并附带 Log ID;失败时显示 ❌ 错误码

凭据使用 [`SecretMap`](../src-tauri/src/settings.rs) 储存,在 Handy 调试日志中会以 `***REDACTED***` 隐藏,不会泄露到日志文件。

## 第三步:验证使用

1. 按下你绑定的 Handy 转录快捷键开始录音(此刻已在后台建连并边录边传)
2. 说一段话(中英文均可,默认开启 ITN 数字标点 / 标点恢复)
3. 松开快捷键 / 停止录音 → 由于音频已边录边传,通常**亚秒级**即可收尾出字
4. 文本应被自动粘贴到当前焦点应用

## 故障排查

### 测试连接失败

| 错误码 | 含义 | 处理 |
|---|---|---|
| `45000001` | API Key 无效 / 已过期 | 回控制台重新生成 Key,粘贴新值 |
| `45000151` | 资源 ID 不匹配你的开通项 | 检查你开通的是 `duration` 还是 `concurrent` |
| `network` / `dial failed` | DNS 失败 / 防火墙阻断 | 确认能 ping `openspeech.bytedance.com`;企业网络可能需要代理 |
| `timeout` | 网络抖动 | 重试一次 |

每次失败时 Handy 会显示豆包返回的 `X-Tt-Logid`(若有),把这个 ID 发给火山引擎工单可加速排错。

### 转录返回空文本

- 检查麦克风是否正确选中(Settings → General)
- 试试加大 `Extra Recording Buffer`(防止尾音被切掉)

### 转录失败弹出错误提示

转写管线失败(断网、API Key 失效、配额用尽等)时,Handy 会弹出「转写失败」通知并附带原始错误信息;本次录音会以空文本形式存入历史记录,可在 Settings → History 中重试。

### 语种选择说明

豆包的双向流式优化端点**不支持指定语种**,语种由云端自动判别。因此选中豆包后,Settings → General 的语言选项只保留「自动 / 简体中文 / 繁体中文」——它唯一仍然生效的作用是控制中文输出的简繁转换(OpenCC 后处理)。

### 切换为豆包后又想用本地模型

直接回 Settings → Models 选其它模型即可。豆包的凭据保留在本地设置文件中,下次切回不需要重新输入。

## 逐字上屏(边说边出字)

豆包流式在录音过程中会持续回传识别中间结果。开启「逐字上屏」后,Handy 会把这些中间结果**实时逐字键入**当前输入框(说一个字、上屏一个字),而不是等松手后整段粘贴。

- **在哪开**:Settings → Models → 选中豆包 → 展开「火山引擎」配置面板 → 打开「逐字上屏」开关。默认关闭。
- **生效条件**:仅豆包引擎、且粘贴方式不为 `None`。开启后录音过程改用**系统级键盘注入**(enigo),会忽略剪贴板粘贴方式;松手时再用最终(含后处理)文本做一次对齐。
- **会有跳字/回改**:豆包返回的是全量结果,后文可能修正前文,因此屏幕上偶尔会看到退格-重打的抖动,这是流式上屏的固有现象。
- **取消即撤销**:录音中按取消键(默认 Escape)会把已键入的中间文本退格删除,恢复输入框。
- **平台差异**:中文直接键入在 macOS 稳定;Windows / Linux(尤其 Wayland)的 Unicode 直接键入兼容性请自行验证。

## 已知限制

- **不支持翻译模式**:豆包 ASR 接口本身不做翻译,Handy 的 "Translate to English" 开关对豆包无效
- **自定义词表为本地后处理**:Handy 的 `custom_words` 通过本地相似度纠正应用到豆包输出(流式与批量路径一致),并非豆包服务端的"热词"接口;服务端热词后续可能扩展
- **不支持指定语种**:双向流式优化端点由云端自动判别语种,语言选项仅用于中文简繁输出
- **依赖外网**:无网络时无法转录;请保留至少一个本地模型作为离线备份
- **音频上传到火山引擎**:请阅读 [火山引擎隐私政策](https://www.volcengine.com/docs/6561) 后再决定是否使用

## 开发者信息

如果你是开发者,想了解 Handy 内部如何调用豆包:

- 协议层(二进制 header / payload):[`src-tauri/src/cloud_asr/doubao/`](../src-tauri/src/cloud_asr/doubao/)
- 边录边传流式会话:[`cloud_asr/doubao/stream.rs`](../src-tauri/src/cloud_asr/doubao/stream.rs) 的 `DoubaoStreamSession`;录音器侧实时帧 tap 见 [`audio_toolkit::audio::recorder::FrameSink`](../src-tauri/src/audio_toolkit/audio/recorder.rs) 与 `AudioRecordingManager::set_frame_sink`
- 编排:录音开始 `TranscriptionManager::begin_doubao_stream`,松手 `take_doubao_stream` + `DoubaoStreamSession::finish`(失败回退 `transcribe`),取消 `abort_doubao_stream`
- 逐字上屏:[`streaming_paste.rs`](../src-tauri/src/streaming_paste.rs) 的 `StreamingPaste`(公共前缀 diff + 退格 + enigo 直接键入);中间结果回调 `DoubaoStreamSession::start(.., on_partial)` → 主线程 `apply_partial`,松手 `TranscriptionManager::finalize_streaming_paste`,取消/失败 `abort_streaming_paste`;开关字段 `AppSettings.streaming_paste`
- 整段批量转写注入点:[`managers::transcription::TranscriptionManager`](../src-tauri/src/managers/transcription.rs) 的 `LoadedEngine::Doubao` 分支
- 凭据 schema:[`AppSettings.doubao_credentials`](../src-tauri/src/settings.rs)
- 协议参考文档:[`docs/豆包语音输入接入.md`](豆包语音输入接入.md) + [`docs/sauc_go/`](sauc_go/)(官方 Go 示例代码)

欢迎在 [GitHub Discussions](https://github.com/cjpais/Handy/discussions) 提交反馈或扩展请求。
