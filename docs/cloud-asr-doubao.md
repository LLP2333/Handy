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

> 注意:Handy 选用的是 `bigmodel_nostream` 端点(非实时,准确率最高),与 Handy "录完一段再识别" 的工作流最契合。如果你想要实时上屏功能,请到 [GitHub Discussions](https://github.com/cjpais/Handy/discussions) 留言。

## 第二步:在 Handy 中配置凭据

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

1. 按下你绑定的 Handy 转录快捷键开始录音
2. 说一段话(中英文均可,默认开启 ITN 数字标点 / 标点恢复)
3. 松开快捷键 / 停止录音 → 等待 1~3 秒
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
- 把语种设为 "Auto" 或对应 BCP-47 代码(`zh-CN`、`en-US` 等)

### 切换为豆包后又想用本地模型

直接回 Settings → Models 选其它模型即可。豆包的凭据保留在本地设置文件中,下次切回不需要重新输入。

## 已知限制

- **不支持翻译模式**:豆包 ASR 接口本身不做翻译,Handy 的 "Translate to English" 开关对豆包无效
- **不支持自定义词表**:Handy 的 `custom_words` 字段对豆包暂不生效(豆包侧要走另外的"热词"接口,后续可能扩展)
- **依赖外网**:无网络时无法转录;请保留至少一个本地模型作为离线备份
- **音频上传到火山引擎**:请阅读 [火山引擎隐私政策](https://www.volcengine.com/docs/6561) 后再决定是否使用

## 开发者信息

如果你是开发者,想了解 Handy 内部如何调用豆包:

- 协议层(二进制 header / payload):[`src-tauri/src/cloud_asr/doubao/`](../src-tauri/src/cloud_asr/doubao/)
- 引擎注入点:[`managers::transcription::TranscriptionManager`](../src-tauri/src/managers/transcription.rs) 的 `LoadedEngine::Doubao` 分支
- 凭据 schema:[`AppSettings.doubao_credentials`](../src-tauri/src/settings.rs)
- 协议参考文档:[`docs/豆包语音输入接入.md`](豆包语音输入接入.md) + [`docs/sauc_go/`](sauc_go/)(官方 Go 示例代码)

欢迎在 [GitHub Discussions](https://github.com/cjpais/Handy/discussions) 提交反馈或扩展请求。
