//! 豆包流式语音识别 WebSocket 客户端。
//!
//! 与 Handy 的"录完一段批量发"语义对齐:本客户端**不**做 push-to-talk 实时上屏,只在 audio
//! 全部分包发送完(末包用负 sequence 标志)后,聚合服务端返回的最终 `result.text` 一次性返回。
//!
//! 转录端点用**双向流式优化版** `wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async`:
//! 服务端「只在识别结果有变化时才回包」,首字/尾字时延比流式输入(nostream)更低,适合追求低
//! 延迟的场景。代价是该模式不支持 `language` 字段、准确率略逊于 nostream 二遍。
//!
//! 注意:async 优化版**不会**在收到 full client request 后回 ack(没有"每包一回"),因此发完
//! 配置包必须立即开始灌音频、并发读响应——绝不能像 nostream 那样阻塞等首包,否则会死等。
//!
//! 「测试连接」(`verify_credentials`)仍走 nostream 端点:它发完配置即回 ack、无需音频、零成本,
//! 且鉴权与 `resource_id` 与端点无关,用它验证凭据最稳。
//!
//! 参考:`docs/sauc_go/client/client.go`、`docs/豆包语音输入接入.md`。

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::{
    handshake::client::generate_key,
    http::{header::HeaderValue, Request, Uri},
    Message,
};

use super::payload::{
    build_audio_only_request, build_full_client_request, f32_samples_to_pcm_s16le,
};
use super::response::parse_response;

/// 转录默认端点:双向流式优化版(低延迟,首字/尾字时延更优,只在结果变化时回包)。
pub const DEFAULT_ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async";

/// 凭据验证端点:流式输入模式。它在收到 full client request 后即回 ack,无需发送音频即可
/// 验证鉴权与 `resource_id`,被 [`DoubaoClient::verify_credentials`]("测试连接")使用。
const VERIFY_ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_nostream";

/// 豆包流式语音识别 2.0 小时版资源 ID(默认值)。
pub const DEFAULT_RESOURCE_ID: &str = "volc.seedasr.sauc.duration";

/// 单包音频时长(毫秒)。豆包文档建议 100~200ms,200ms 为最优。
const SEGMENT_DURATION_MS: usize = 200;
/// 16 kHz、16 bit、单声道的字节速率 = 16000 * 2 * 1 = 32000 B/s。
const PCM_BYTES_PER_SECOND: usize = 16000 * 2;
/// 每包字节数(200ms)。流式会话([`super::stream`])复用同一分包尺寸。
pub(super) const SEGMENT_SIZE_BYTES: usize = PCM_BYTES_PER_SECOND * SEGMENT_DURATION_MS / 1000;

/// 豆包 ASR 客户端。
///
/// 一次构造可重复调用 [`Self::transcribe`]——每次都会建立独立的 WebSocket 连接(豆包要求
/// 每次会话独立)。客户端本身**不持有**连接,因此可在 `Send + Sync` 上下文(例如 Handy 的
/// `LoadedEngine`)安全跨线程使用。
pub struct DoubaoClient {
    api_key: String,
    resource_id: String,
    endpoint: String,
}

impl DoubaoClient {
    /// 用给定的鉴权信息构造客户端。
    ///
    /// `api_key` 取自火山引擎新版控制台的 X-Api-Key;`resource_id` 选定模型版本(默认
    /// [`DEFAULT_RESOURCE_ID`])。如果 `resource_id` 为空字符串,自动回落到默认值。
    pub fn new(api_key: String, resource_id: String) -> Self {
        let resource_id = if resource_id.trim().is_empty() {
            DEFAULT_RESOURCE_ID.to_string()
        } else {
            resource_id
        };
        Self {
            api_key,
            resource_id,
            endpoint: DEFAULT_ENDPOINT.to_string(),
        }
    }

    /// 在一个 **干净的 OS 线程** 上跑给定的 future 并 block 直到返回。
    ///
    /// # 为什么需要这层包装
    ///
    /// 1. Tokio 通过 thread-local 标记「当前线程是否在驱动 runtime」。一旦调用线程被外层
    ///    runtime 标记(Tauri 的 async 命令处理器、`#[tokio::main]` worker 等),**任何**
    ///    `Runtime::block_on` 都会 panic with "Cannot start a runtime from within a runtime"——
    ///    它检查的是 OS 线程标记,与是哪个 runtime 实例无关。
    /// 2. 反过来,**长生命周期** 的 runtime 持有又会带来另一个坑:[`Runtime`] 的 [`Drop`]
    ///    会阻塞等待 worker 完成,而在 async context 里 drop runtime 会触发
    ///    "Cannot drop a runtime in a context where blocking is not allowed"。
    ///
    /// 所以我们采用最朴素的方案:**每次调用都启动一个临时 OS 线程,在那个线程上现场建一个
    /// `current_thread` runtime,跑完 future 立即销毁**。开销在 µs 量级,相比一次豆包
    /// WebSocket 往返(秒级)完全可以忽略,但彻底回避两类 runtime 嵌套陷阱。
    fn run_blocking<F, T>(future: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>> + Send,
        T: Send,
    {
        std::thread::scope(|scope| {
            let handle = scope.spawn(move || -> Result<T> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed to build temporary tokio runtime for blocking call")?;
                rt.block_on(future)
            });
            handle
                .join()
                .map_err(|_| anyhow!("Doubao blocking worker thread panicked"))?
        })
    }

    /// 同步入口:把 [`Self::transcribe`] 包装成可从 **任意线程** 安全调用的同步 API。
    pub fn transcribe_blocking(&self, audio: &[f32], language: Option<&str>) -> Result<String> {
        Self::run_blocking(self.transcribe(audio, language))
            .context("Doubao transcribe_blocking failed")
    }

    /// 同步入口:对应 [`Self::verify_credentials`]。
    /// 主要给同步上下文(未来可能的 CLI 自检)使用;UI 路径仍可直接 `await` async 版本。
    #[allow(dead_code)]
    pub fn verify_credentials_blocking(&self) -> Result<Option<String>> {
        Self::run_blocking(self.verify_credentials())
            .context("Doubao verify_credentials_blocking failed")
    }

    /// 转录一段已录完的 16 kHz / 单声道 / `f32` 音频(双向流式优化版 `bigmodel_async`)。
    ///
    /// `language` 参数仅为接口兼容保留:**async 优化版不支持 `language` 字段**(见
    /// `docs/豆包语音输入接入.md` 第 211 行),传入值只用于调试日志,不会下发给服务端,语种由
    /// 豆包默认的中英文+方言模型自动判别。
    ///
    /// 流程:
    /// 1. f32 → pcm_s16le
    /// 2. WebSocket 建连(鉴权 header)
    /// 3. full client request(JSON 配置,seq=1)
    /// 4. **不等 ack**,立即按 200ms/包顺序发送 audio only request(seq 从 2 递增),末包用负 sequence,
    ///    边发边并发读响应——async 优化版只在结果变化时回包,阻塞等首包会死等
    /// 5. 末包发完后继续读,直到末包标志或错误码非零
    /// 6. 返回最终 `result.text`(默认 `result_type=full`,全量,逐包覆盖)
    ///
    /// # 错误
    /// - 网络/握手失败
    /// - 鉴权失败(返回服务端错误码)
    /// - 服务端返回非零业务错误码
    /// - 末包之前连接被远端关闭
    pub async fn transcribe(&self, audio: &[f32], language: Option<&str>) -> Result<String> {
        if self.api_key.trim().is_empty() {
            return Err(anyhow!("Doubao API key is empty"));
        }
        if audio.is_empty() {
            return Err(anyhow!("Empty audio buffer"));
        }

        let pcm = f32_samples_to_pcm_s16le(audio);
        debug!(
            "Doubao(async): transcribing {} f32 samples ({} PCM bytes); language hint {:?} ignored (async optimized mode doesn't support it)",
            audio.len(),
            pcm.len(),
            language,
        );

        let request_id = uuid::Uuid::new_v4().to_string();
        let request = self.build_handshake_request(&request_id, &self.endpoint)?;

        let (mut ws_stream, response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| anyhow!("Doubao WebSocket dial failed: {e}"))?;

        if let Some(logid) = response
            .headers()
            .get("X-Tt-Logid")
            .and_then(|v| v.to_str().ok())
        {
            info!("Doubao logid={} request_id={}", logid, request_id);
        }

        // 1) full client request(async 不下发 language)
        let full_req = build_full_client_request(None)?;
        ws_stream
            .send(Message::Binary(full_req))
            .await
            .map_err(|e| anyhow!("send full client request failed: {e}"))?;

        // 2) 分包发送音频
        // 首包 seq=1 用于 full client request,音频包从 seq=2 开始递增。
        // 关键:async 优化版「只在结果变化时回包」、不保证有 ack,因此发完 config 直接灌音频,
        // 绝不能在这里阻塞等首包(否则在没结果可回时会死等)。
        let total_packets = pcm.len().div_ceil(SEGMENT_SIZE_BYTES);
        let mut text_buf = String::new();
        let mut got_last = false;

        for (seq, (idx, chunk)) in (2_i32..).zip(pcm.chunks(SEGMENT_SIZE_BYTES).enumerate()) {
            let is_last = idx + 1 == total_packets;
            let seq_to_send = if is_last { -seq } else { seq };

            let frame = build_audio_only_request(seq_to_send, chunk)?;
            ws_stream
                .send(Message::Binary(frame))
                .await
                .map_err(|e| anyhow!("send audio packet seq={seq_to_send} failed: {e}"))?;

            // 边发边并发读:async 会随结果变化推增量/全量包,这里及时收掉避免堆积;
            // 短 tick(10ms)在不影响读取的前提下尽快把音频灌完,压低上传耗时。
            tokio::select! {
                msg = ws_stream.next() => {
                    if let Some(Ok(Message::Binary(bytes))) = msg {
                        if let Ok(parsed) = parse_response(&bytes) {
                            let was_last = parsed.is_last_package;
                            handle_parsed(parsed, &mut text_buf)?;
                            if was_last {
                                got_last = true;
                                break;
                            }
                        }
                    }
                }
                _ = sleep(Duration::from_millis(10)) => {}
            }
        }

        // 3) 末包发完后(若发送循环里还没读到末包标志),继续读直到末包标志或错误。
        // 30s 总尾部超时——async 优化版通常在末包后亚秒级返回最终结果。如果服务端因任何
        // 原因不发结束帧,这里至少不会让录音完毕的用户无限等下去。
        let tail_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !got_last {
            let msg = match tokio::time::timeout_at(tail_deadline, ws_stream.next()).await {
                Ok(Some(m)) => m.map_err(|e| anyhow!("read tail response failed: {e}"))?,
                Ok(None) => {
                    warn!("Doubao server closed before sending last_package frame");
                    break;
                }
                Err(_) => {
                    warn!("Doubao tail read timed out after 30s; returning what we have so far");
                    break;
                }
            };
            match msg {
                Message::Binary(bytes) => {
                    let parsed = parse_response(&bytes)?;
                    let was_last = parsed.is_last_package;
                    handle_parsed(parsed, &mut text_buf)?;
                    if was_last {
                        got_last = true;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }

        // 关闭连接(豆包侧通常已经关了,这里是兜底)
        let _ = ws_stream.send(Message::Close(None)).await;

        if text_buf.is_empty() {
            warn!("Doubao returned empty text (no recognized speech?)");
        }
        Ok(text_buf)
    }

    /// 仅做一次 WebSocket 握手 + full client request + 首包 ack,用来验证凭据是否可用。
    ///
    /// **刻意走 nostream 端点([`VERIFY_ENDPOINT`])而非转录用的 async 优化版**:nostream 在收到
    /// 配置包后立即回 ack,无需发送音频即可探活;而 async 优化版「只在结果变化时回包」,纯握手不发
    /// 音频会拿不到响应、导致「测试连接」一直转圈。鉴权与 `resource_id` 都与端点无关,因此用 nostream
    /// 验证凭据完全等价,且零音频成本。
    ///
    /// 不发送音频,不读取最终结果,因此对豆包计费几乎为零。成功时返回 `X-Tt-Logid`(可选,
    /// 用于排错);失败时返回错误描述(包含豆包返回的 `code` / `error` 信息)。
    pub async fn verify_credentials(&self) -> Result<Option<String>> {
        if self.api_key.trim().is_empty() {
            return Err(anyhow!("Doubao API key is empty"));
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = self.build_handshake_request(&request_id, VERIFY_ENDPOINT)?;
        let (mut ws_stream, response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| anyhow!("Doubao WebSocket dial failed: {e}"))?;

        let logid = response
            .headers()
            .get("X-Tt-Logid")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let full_req = build_full_client_request(None)?;
        ws_stream
            .send(Message::Binary(full_req))
            .await
            .map_err(|e| anyhow!("send full client request failed: {e}"))?;

        // 加超时,避免服务端不响应导致 UI「测试连接」按钮无限转圈。豆包 ack 通常 <1s,
        // 给 10s 既宽裕又不至于让用户误以为程序卡死。
        match tokio::time::timeout(Duration::from_secs(10), ws_stream.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let parsed = parse_response(&bytes)?;
                if parsed.code != 0 {
                    return Err(anyhow!(
                        "Doubao server returned error code {}: {:?}",
                        parsed.code,
                        parsed.payload.and_then(|p| p.error)
                    ));
                }
            }
            Ok(Some(Ok(_))) => {} // 非 binary 帧(ping/text 之类),忽略
            Ok(Some(Err(e))) => {
                return Err(anyhow!("read first response failed: {e}"));
            }
            Ok(None) => {
                return Err(anyhow!("Doubao server closed connection before responding"));
            }
            Err(_) => {
                return Err(anyhow!(
                    "Doubao server did not respond within 10s; check network or X-Api-Resource-Id"
                ));
            }
        }

        let _ = ws_stream.send(Message::Close(None)).await;
        Ok(logid)
    }

    /// 构造带鉴权 header 的 WebSocket 握手 Request(委托给模块级 [`build_doubao_handshake`])。
    ///
    /// `endpoint` 显式传入而非固定读 `self.endpoint`:转录走 async 优化版,而「测试连接」需要走
    /// nostream 端点(见 [`Self::verify_credentials`]),两者复用同一套鉴权头。
    fn build_handshake_request(&self, request_id: &str, endpoint: &str) -> Result<Request<()>> {
        build_doubao_handshake(&self.api_key, &self.resource_id, request_id, endpoint)
    }
}

/// 构造带鉴权 header 的豆包 WebSocket 握手 Request。
///
/// 被 [`DoubaoClient`](批量/验证)与 [`super::stream::DoubaoStreamSession`](边录边传)共用,
/// 避免鉴权头逻辑各写一份漂移。`endpoint` 决定接入模式(async / nostream)。
///
/// # Errors
/// `endpoint` 不是合法 URI、缺少 host,或 `api_key` / `resource_id` 含非法 header 字符时返回 `Err`。
pub(super) fn build_doubao_handshake(
    api_key: &str,
    resource_id: &str,
    request_id: &str,
    endpoint: &str,
) -> Result<Request<()>> {
    let uri: Uri = endpoint
        .parse()
        .map_err(|e| anyhow!("invalid Doubao endpoint URI {}: {e}", endpoint))?;

    let host = uri
        .host()
        .ok_or_else(|| anyhow!("Doubao endpoint missing host: {}", endpoint))?
        .to_string();

    let mut req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Host", host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .header("X-Api-Key", HeaderValue::from_str(api_key)?)
        .header("X-Api-Resource-Id", HeaderValue::from_str(resource_id)?)
        .header("X-Api-Request-Id", HeaderValue::from_str(request_id)?)
        .header("X-Api-Connect-Id", HeaderValue::from_str(request_id)?)
        .header("X-Api-Sequence", HeaderValue::from_static("-1"))
        .body(())
        .map_err(|e| anyhow!("build Doubao handshake request failed: {e}"))?;

    // headers() 看起来是只读视图;上面 builder 已加完所有头,这里仅留作扩展点。
    let _ = req.headers_mut();
    Ok(req)
}

pub(super) fn handle_parsed(
    parsed: super::response::DoubaoResponse,
    text_buf: &mut String,
) -> Result<()> {
    if parsed.code != 0 {
        return Err(anyhow!(
            "Doubao server returned error code {}: {:?}",
            parsed.code,
            parsed.payload.and_then(|p| p.error)
        ));
    }
    if let Some(payload) = parsed.payload {
        if !payload.result.text.is_empty() {
            // bigmodel_nostream "result_type":"full" 默认全量返回,直接覆盖
            *text_buf = payload.result.text;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_size_constants_consistent() {
        assert_eq!(SEGMENT_SIZE_BYTES, 6400); // 200ms * 32000 B/s = 6400
    }

    #[test]
    fn test_new_falls_back_to_default_resource_id() {
        let client = DoubaoClient::new("dummy".to_string(), "  ".to_string());
        assert_eq!(client.resource_id, DEFAULT_RESOURCE_ID);
    }

    #[test]
    fn test_new_keeps_custom_resource_id() {
        let client = DoubaoClient::new(
            "dummy".to_string(),
            "volc.seedasr.sauc.concurrent".to_string(),
        );
        assert_eq!(client.resource_id, "volc.seedasr.sauc.concurrent");
    }

    #[test]
    fn test_handshake_request_has_required_headers() {
        let client = DoubaoClient::new("test-key".to_string(), DEFAULT_RESOURCE_ID.to_string());
        let req = client
            .build_handshake_request("rid-1234", &client.endpoint)
            .unwrap();
        let headers = req.headers();
        assert_eq!(headers.get("X-Api-Key").unwrap(), "test-key");
        assert_eq!(
            headers.get("X-Api-Resource-Id").unwrap(),
            DEFAULT_RESOURCE_ID
        );
        assert_eq!(headers.get("X-Api-Request-Id").unwrap(), "rid-1234");
        assert_eq!(headers.get("X-Api-Sequence").unwrap(), "-1");
    }

    /// 转录端点必须是双向流式优化版(低延迟),验证端点必须是 nostream(发配置即回 ack)。
    #[test]
    fn test_endpoints_select_expected_modes() {
        assert!(
            DEFAULT_ENDPOINT.ends_with("/bigmodel_async"),
            "transcribe must use the async optimized endpoint for low latency, got {DEFAULT_ENDPOINT}"
        );
        assert!(
            VERIFY_ENDPOINT.ends_with("/bigmodel_nostream"),
            "verify must use the nostream endpoint (it acks on config without audio), got {VERIFY_ENDPOINT}"
        );
    }

    /// async 优化版的握手请求应当指向 async 端点,且鉴权头完整。
    #[test]
    fn test_handshake_uses_given_endpoint() {
        let client = DoubaoClient::new("k".to_string(), DEFAULT_RESOURCE_ID.to_string());
        let req = client
            .build_handshake_request("rid", VERIFY_ENDPOINT)
            .unwrap();
        assert!(req.uri().to_string().ends_with("/bigmodel_nostream"));
    }

    /// 真实的端到端测试,需要 `DOUBAO_API_KEY` 环境变量与可用的网络。
    /// CI 默认跳过,本地运行:
    /// ```bash
    /// DOUBAO_API_KEY=xxx cargo test -p handy --lib doubao -- --ignored
    /// ```
    #[ignore]
    #[tokio::test]
    async fn test_real_doubao_transcribe_ignored() {
        let api_key = match std::env::var("DOUBAO_API_KEY") {
            Ok(k) => k,
            Err(_) => {
                eprintln!("DOUBAO_API_KEY not set; skipping integration test");
                return;
            }
        };
        let client = DoubaoClient::new(api_key, DEFAULT_RESOURCE_ID.to_string());

        // 1 秒静音(全 0)。预期返回空 text 但不应报错码 != 0。
        let audio = vec![0.0f32; 16000];
        let result = client.transcribe(&audio, Some("zh-CN")).await;
        assert!(
            result.is_ok(),
            "transcribe should succeed even on silence, got {:?}",
            result.err()
        );
    }

    /// 真实语音转录测试。读取 `DOUBAO_TEST_WAV` 指定的 16kHz 单声道 WAV 文件,
    /// 通过豆包接口转录后把结果打到 stdout。
    ///
    /// ```bash
    /// DOUBAO_API_KEY=xxx DOUBAO_TEST_WAV=/tmp/test.wav \
    ///     cargo test -p handy --lib test_real_doubao_transcribe_with_wav -- --ignored --nocapture
    /// ```
    #[ignore]
    #[tokio::test]
    async fn test_real_doubao_transcribe_with_wav() {
        let api_key = match std::env::var("DOUBAO_API_KEY") {
            Ok(k) => k,
            Err(_) => {
                eprintln!("DOUBAO_API_KEY not set; skipping wav integration test");
                return;
            }
        };
        let wav_path = match std::env::var("DOUBAO_TEST_WAV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("DOUBAO_TEST_WAV not set; skipping wav integration test");
                return;
            }
        };

        let mut reader = hound::WavReader::open(&wav_path).expect("open wav");
        let spec = reader.spec();
        eprintln!(
            "Loaded WAV: {} Hz, {} ch, {} bits, {:?}",
            spec.sample_rate, spec.channels, spec.bits_per_sample, spec.sample_format
        );
        assert_eq!(spec.sample_rate, 16000, "豆包需要 16 kHz");
        assert_eq!(spec.channels, 1, "豆包需要单声道");

        let audio: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
            (hound::SampleFormat::Int, 16) => reader
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect(),
            (hound::SampleFormat::Float, 32) => {
                reader.samples::<f32>().map(|s| s.unwrap()).collect()
            }
            other => panic!("unsupported wav sample format: {:?}", other),
        };

        let client = DoubaoClient::new(api_key, DEFAULT_RESOURCE_ID.to_string());
        let text = client
            .transcribe(&audio, Some("zh-CN"))
            .await
            .expect("豆包转录失败");

        eprintln!("识别结果: {}", text);
        assert!(!text.trim().is_empty(), "真实语音应当能识别出文本");
    }

    /// 回归测试:验证 [`DoubaoClient::transcribe_blocking`] 可以在 **已有 tokio runtime
    /// 的线程** 上安全调用,而不触发 "Cannot start a runtime from within a runtime" panic。
    ///
    /// 这正是 Handy 快捷键录音路径会遇到的实际场景——actions.rs 在 async 任务里同步调
    /// `transcribe()`,如果 `transcribe_blocking` 内部直接 `Runtime::block_on`(没切换到
    /// 干净 OS 线程),就会 panic。Tokio 的 thread-local runtime 标记与 runtime flavor
    /// 无关,因此 default `current_thread` flavor 同样足以复现。
    ///
    /// ```bash
    /// DOUBAO_API_KEY=xxx cargo test -p handy --lib \
    ///     test_transcribe_blocking_in_tokio_runtime -- --ignored --nocapture
    /// ```
    #[ignore]
    #[tokio::test]
    async fn test_transcribe_blocking_in_tokio_runtime() {
        let api_key = match std::env::var("DOUBAO_API_KEY") {
            Ok(k) => k,
            Err(_) => {
                eprintln!("DOUBAO_API_KEY not set; skipping nested-runtime test");
                return;
            }
        };
        let client = DoubaoClient::new(api_key, DEFAULT_RESOURCE_ID.to_string());

        // 1 秒静音 PCM,16 kHz mono,够走完一次 WS 会话。重点不是结果,而是
        // 调用本身不能 panic——这是修复 std::thread::scope 的核心验证点。
        let audio = vec![0.0f32; 16000];

        // 直接在当前 tokio worker thread 上调同步 API。修复前必 panic;修复后正常返回。
        // 这是核心验证点 —— Handy 快捷键路径下,actions.rs 的 async 任务就是这样直接调
        // 同步的 transcribe(),内部走到 transcribe_blocking,如果不脱离当前 worker 就会
        // 触发 "Cannot start a runtime from within a runtime"。
        let result = client.transcribe_blocking(&audio, Some("zh-CN"));
        eprintln!("transcribe_blocking on tokio worker = {:?}", result);
        assert!(
            result.is_ok(),
            "transcribe_blocking on tokio worker should not panic, got {:?}",
            result.err()
        );
    }
}
