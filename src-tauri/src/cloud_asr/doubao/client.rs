//! 豆包流式语音识别 WebSocket 客户端。
//!
//! 与 Handy 的"录完一段批量发"语义对齐:本客户端**不**做 push-to-talk 实时上屏,只在 audio
//! 全部分包发送完(末包用负 sequence 标志)后,聚合服务端返回的最终 `result.text` 一次性返回。
//!
//! 端点固定为 `wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_nostream`(流式输入模式,
//! 准确率最高,与 Handy 场景最契合)。
//!
//! 参考:`docs/sauc_go/client/client.go`、`docs/豆包语音输入接入.md`。

use std::time::Duration;

use anyhow::{anyhow, Result};
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

/// 默认接入端点(流式输入模式 / 火山引擎大模型 ASR)。
pub const DEFAULT_ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_nostream";

/// 豆包流式语音识别 2.0 小时版资源 ID(默认值)。
pub const DEFAULT_RESOURCE_ID: &str = "volc.seedasr.sauc.duration";

/// 单包音频时长(毫秒)。豆包文档建议 100~200ms,200ms 为最优。
const SEGMENT_DURATION_MS: usize = 200;
/// 16 kHz、16 bit、单声道的字节速率 = 16000 * 2 * 1 = 32000 B/s。
const PCM_BYTES_PER_SECOND: usize = 16000 * 2;
/// 每包字节数。
const SEGMENT_SIZE_BYTES: usize = PCM_BYTES_PER_SECOND * SEGMENT_DURATION_MS / 1000;

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

    /// 转录一段已录完的 16 kHz / 单声道 / `f32` 音频。
    ///
    /// `language` 用 BCP-47 风格(`"zh-CN"` / `"en-US"`),`None` 时由豆包按默认中英文+方言识别。
    /// 流程:
    /// 1. f32 → pcm_s16le
    /// 2. WebSocket 建连(鉴权 header)
    /// 3. full client request(JSON 配置)
    /// 4. 按 200ms/包顺序发送 audio only request,末包用负 sequence
    /// 5. 接收所有 server response,直到末包标志或错误码非零
    /// 6. 返回最终 `result.text`
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
            "Doubao: transcribing {} f32 samples ({} PCM bytes)",
            audio.len(),
            pcm.len()
        );

        let request_id = uuid::Uuid::new_v4().to_string();
        let request = self.build_handshake_request(&request_id)?;

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

        // 1) full client request
        let full_req = build_full_client_request(language.map(|s| s.to_string()))?;
        ws_stream
            .send(Message::Binary(full_req))
            .await
            .map_err(|e| anyhow!("send full client request failed: {e}"))?;

        // 等待首包响应(豆包通常会先回 ack)
        if let Some(msg) = ws_stream.next().await {
            let msg = msg.map_err(|e| anyhow!("read first response failed: {e}"))?;
            if let Message::Binary(bytes) = msg {
                let parsed = parse_response(&bytes)?;
                if parsed.code != 0 {
                    return Err(anyhow!(
                        "Doubao server error on full client request: code={}, msg={:?}",
                        parsed.code,
                        parsed.payload.and_then(|p| p.error)
                    ));
                }
            }
        }

        // 2) 分包发送音频
        // 首包 seq=1 用于 full client request,音频包从 seq=2 开始递增。
        let total_packets = pcm.len().div_ceil(SEGMENT_SIZE_BYTES);
        let mut text_buf = String::new();

        for (seq, (idx, chunk)) in (2_i32..).zip(pcm.chunks(SEGMENT_SIZE_BYTES).enumerate()) {
            let is_last = idx + 1 == total_packets;
            let seq_to_send = if is_last { -seq } else { seq };

            let frame = build_audio_only_request(seq_to_send, chunk)?;
            ws_stream
                .send(Message::Binary(frame))
                .await
                .map_err(|e| anyhow!("send audio packet seq={seq_to_send} failed: {e}"))?;

            // 一边发包一边非阻塞读取响应(避免末包前丢响应)。
            // 豆包 nostream 通常只在末包前后才返回真正结果,这里轻读不强求。
            tokio::select! {
                msg = ws_stream.next() => {
                    if let Some(Ok(Message::Binary(bytes))) = msg {
                        if let Ok(parsed) = parse_response(&bytes) {
                            handle_parsed(parsed, &mut text_buf)?;
                        }
                    }
                }
                _ = sleep(Duration::from_millis(20)) => {}
            }
        }

        // 3) 末包发完后,继续读直到末包标志或错误
        while let Some(msg) = ws_stream.next().await {
            let msg = msg.map_err(|e| anyhow!("read tail response failed: {e}"))?;
            match msg {
                Message::Binary(bytes) => {
                    let parsed = parse_response(&bytes)?;
                    let was_last = parsed.is_last_package;
                    handle_parsed(parsed, &mut text_buf)?;
                    if was_last {
                        break;
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
    /// 不发送音频,不读取最终结果,因此对豆包计费几乎为零。成功时返回 `X-Tt-Logid`(可选,
    /// 用于排错);失败时返回错误描述(包含豆包返回的 `code` / `error` 信息)。
    pub async fn verify_credentials(&self) -> Result<Option<String>> {
        if self.api_key.trim().is_empty() {
            return Err(anyhow!("Doubao API key is empty"));
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let request = self.build_handshake_request(&request_id)?;
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

        if let Some(msg) = ws_stream.next().await {
            let msg = msg.map_err(|e| anyhow!("read first response failed: {e}"))?;
            if let Message::Binary(bytes) = msg {
                let parsed = parse_response(&bytes)?;
                if parsed.code != 0 {
                    return Err(anyhow!(
                        "Doubao server returned error code {}: {:?}",
                        parsed.code,
                        parsed.payload.and_then(|p| p.error)
                    ));
                }
            }
        }

        let _ = ws_stream.send(Message::Close(None)).await;
        Ok(logid)
    }

    /// 构造带鉴权 header 的 WebSocket 握手 Request。
    fn build_handshake_request(&self, request_id: &str) -> Result<Request<()>> {
        let uri: Uri = self
            .endpoint
            .parse()
            .map_err(|e| anyhow!("invalid Doubao endpoint URI {}: {e}", self.endpoint))?;

        let host = uri
            .host()
            .ok_or_else(|| anyhow!("Doubao endpoint missing host: {}", self.endpoint))?
            .to_string();

        let mut req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", generate_key())
            .header("X-Api-Key", HeaderValue::from_str(&self.api_key)?)
            .header(
                "X-Api-Resource-Id",
                HeaderValue::from_str(&self.resource_id)?,
            )
            .header("X-Api-Request-Id", HeaderValue::from_str(request_id)?)
            .header("X-Api-Connect-Id", HeaderValue::from_str(request_id)?)
            .header("X-Api-Sequence", HeaderValue::from_static("-1"))
            .body(())
            .map_err(|e| anyhow!("build Doubao handshake request failed: {e}"))?;

        // headers() 看起来是只读视图;上面 builder 已加完所有头,这里仅留作扩展点。
        let _ = req.headers_mut();
        Ok(req)
    }
}

fn handle_parsed(parsed: super::response::DoubaoResponse, text_buf: &mut String) -> Result<()> {
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
        let req = client.build_handshake_request("rid-1234").unwrap();
        let headers = req.headers();
        assert_eq!(headers.get("X-Api-Key").unwrap(), "test-key");
        assert_eq!(
            headers.get("X-Api-Resource-Id").unwrap(),
            DEFAULT_RESOURCE_ID
        );
        assert_eq!(headers.get("X-Api-Request-Id").unwrap(), "rid-1234");
        assert_eq!(headers.get("X-Api-Sequence").unwrap(), "-1");
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
}
