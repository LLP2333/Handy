//! 豆包「边录边传」流式会话。
//!
//! 与 [`super::client::DoubaoClient`] 的「录完整段再发」不同,本模块在**录音开始时**就建立 WebSocket
//! 连接,把录音过程中实时产出的 16 kHz / 单声道 / VAD 过滤后的语音帧持续上传(双向流式优化版
//! `bigmodel_async`),松手时只需发送末包并等待最终结果。由于音频已在录音期间边录边传、服务端也边收
//! 边算,「松手 → 出字」的尾部延迟被压到最低。
//!
//! 线程模型:每个会话在一个**独立 OS 线程**上跑一个 `current_thread` tokio runtime(与
//! `DoubaoClient::run_blocking` 同样的理由:避免在 Tauri/tokio worker 线程上嵌套 runtime)。音频线程
//! 通过 unbounded channel 把帧推给该 worker;录音结束时 [`DoubaoStreamSession::finish`] 阻塞取回最终
//! 文本(阻塞时长 ≈ 末包后的尾部处理,亚秒级)。

use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::client::{build_doubao_handshake, handle_parsed, DEFAULT_ENDPOINT, SEGMENT_SIZE_BYTES};
use super::payload::{
    build_audio_only_request, build_full_client_request, f32_samples_to_pcm_s16le,
};
use super::response::parse_response;

/// 音频线程 → 会话 worker 的消息。
enum StreamMsg {
    /// 一段 16 kHz、单声道、`f32` 语音帧(录音过程中实时推送)。
    Frame(Vec<f32>),
    /// 录音正常结束:把缓冲里剩余音频作为末包(负 sequence)发出,然后收尾。
    Finish,
}

/// 一次豆包流式识别会话的句柄。
///
/// 用 [`DoubaoStreamSession::start`] 启动(立即建连并开始收帧),用 [`Self::frame_sink`] 取得可挂到
/// 录音器的实时帧回调,用 [`Self::finish`] 收尾取回最终文本,或用 [`Self::abort`] 中止(取消录音)。
pub struct DoubaoStreamSession {
    frame_tx: mpsc::UnboundedSender<StreamMsg>,
    result_rx: std::sync::mpsc::Receiver<Result<String>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DoubaoStreamSession {
    /// 启动会话:在独立线程上建连并开始接收音频帧。
    ///
    /// `api_key` / `resource_id` 取自设置中的豆包凭据。建连是异步进行的——若网络/鉴权失败,错误会在
    /// [`Self::finish`] 时返回,调用方可据此回退到「整段批量转写」。
    ///
    /// `on_partial` 为可选的中间结果回调:每当服务端回包使识别全量文本发生变化时,以最新**全量**文本
    /// 调用一次(用于「逐字上屏」)。回调在会话 worker 线程上同步触发,实现方应自行 marshal 到需要的
    /// 线程并保持轻量、非阻塞。`None` 时退化为原有「只在 [`Self::finish`] 取最终文本」的行为。
    pub fn start(
        api_key: String,
        resource_id: String,
        on_partial: Option<Box<dyn FnMut(String) + Send>>,
    ) -> Self {
        let (frame_tx, frame_rx) = mpsc::unbounded_channel::<StreamMsg>();
        let (result_tx, result_rx) = std::sync::mpsc::channel::<Result<String>>();

        let worker = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = result_tx.send(Err(anyhow!(
                        "failed to build tokio runtime for Doubao stream: {e}"
                    )));
                    return;
                }
            };
            let res = rt.block_on(run_session(api_key, resource_id, frame_rx, on_partial));
            let _ = result_tx.send(res);
        });

        Self {
            frame_tx,
            result_rx,
            worker: Some(worker),
        }
    }

    /// 生成可挂到录音器([`crate::managers::audio::AudioRecordingManager::set_frame_sink`])的实时帧回调。
    ///
    /// 回调持有 channel 发送端的克隆,从音频线程同步调用、**非阻塞**(只做一次 unbounded send)。
    /// 会话结束/中止后向已关闭 channel 发送会静默失败,不影响音频管线。
    pub fn frame_sink(&self) -> impl FnMut(&[f32]) + Send + 'static {
        let tx = self.frame_tx.clone();
        move |frame: &[f32]| {
            let _ = tx.send(StreamMsg::Frame(frame.to_vec()));
        }
    }

    /// 录音结束:通知 worker 发末包并阻塞取回最终文本。
    ///
    /// 阻塞时长 ≈ 末包后的尾部处理(通常亚秒级);worker 自带 30s 尾部超时,这里再套 35s 兜底,
    /// 避免在异常情况下永久阻塞调用线程。返回 `Err` 时调用方应回退到整段批量转写。
    pub fn finish(mut self) -> Result<String> {
        // 通知 worker:录音结束,flush 末包。即使发送失败(worker 已退出)也继续取结果。
        let _ = self.frame_tx.send(StreamMsg::Finish);

        let res = match self.result_rx.recv_timeout(Duration::from_secs(35)) {
            Ok(r) => r,
            Err(_) => Err(anyhow!("Doubao streaming worker did not return within 35s")),
        };

        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        res
    }

    /// 中止会话(取消录音时调用):丢弃句柄即可。
    ///
    /// 配合上游先 `clear_frame_sink`,两个发送端都被丢弃后 worker 的 `recv()` 返回 `None`,worker 自行
    /// 发末包、收尾并退出(结果无人接收,被忽略)。不阻塞调用线程。
    pub fn abort(self) {
        debug!("Doubao streaming session aborted");
        // Drop self → frame_tx 关闭;worker 检测到 channel 关闭后自行退出。
    }
}

/// 会话 worker 主体:建连 → 发配置 → 边收帧边上传 → 末包后读到结束 → 返回最终文本。
async fn run_session(
    api_key: String,
    resource_id: String,
    mut frame_rx: mpsc::UnboundedReceiver<StreamMsg>,
    mut on_partial: Option<Box<dyn FnMut(String) + Send>>,
) -> Result<String> {
    if api_key.trim().is_empty() {
        return Err(anyhow!("Doubao API key is empty"));
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let request = build_doubao_handshake(&api_key, &resource_id, &request_id, DEFAULT_ENDPOINT)?;
    let (ws_stream, response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| anyhow!("Doubao stream dial failed: {e}"))?;

    if let Some(logid) = response
        .headers()
        .get("X-Tt-Logid")
        .and_then(|v| v.to_str().ok())
    {
        info!("Doubao stream logid={} request_id={}", logid, request_id);
    }

    // 拆成读写两半:select! 里同时读响应与写音频,避免对同一 ws 的可变借用冲突。
    let (mut write, mut read) = ws_stream.split();

    // full client request(seq=1,async 不下发 language)
    write
        .send(Message::Binary(build_full_client_request(None)?))
        .await
        .map_err(|e| anyhow!("send full client request failed: {e}"))?;

    let mut pcm_buf: Vec<u8> = Vec::with_capacity(SEGMENT_SIZE_BYTES * 2);
    let mut seq: i32 = 2; // seq=1 是 config,音频包从 2 起
    let mut text_buf = String::new();
    let mut got_last = false;
    // 上一次已通过 on_partial 发出的全量文本,用于去重(豆包 async「仅结果变化时回包」,但仍可能
    // 回相同内容),避免重复触发逐字上屏的退格-重打。
    let mut last_emitted = String::new();

    // 文本若较上次变化则发出最新全量给「逐字上屏」回调。`text_buf` 在 handle_parsed 后可能更新。
    macro_rules! emit_partial {
        () => {
            if let Some(cb) = on_partial.as_mut() {
                if text_buf != last_emitted && !text_buf.is_empty() {
                    cb(text_buf.clone());
                    last_emitted = text_buf.clone();
                }
            }
        };
    }

    // 阶段一:录音进行中——并发地「收帧→满 200ms 就发」与「读服务端增量结果」。
    // 收到 Finish(或两端发送端都被丢弃 → None)时,把剩余音频作为负 seq 末包发出,进入收尾。
    let entered_finishing = loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Ok(parsed) = parse_response(&bytes) {
                            let was_last = parsed.is_last_package;
                            handle_parsed(parsed, &mut text_buf)?;
                            emit_partial!();
                            if was_last {
                                got_last = true;
                                break false; // 服务端提前给了末包(罕见),直接收尾
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return Err(anyhow!("Doubao stream closed before recording finished"));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(anyhow!("Doubao stream read failed: {e}")),
                }
            }
            cmd = frame_rx.recv() => {
                match cmd {
                    Some(StreamMsg::Frame(frame)) => {
                        pcm_buf.extend_from_slice(&f32_samples_to_pcm_s16le(&frame));
                        while pcm_buf.len() >= SEGMENT_SIZE_BYTES {
                            let chunk: Vec<u8> = pcm_buf.drain(..SEGMENT_SIZE_BYTES).collect();
                            write
                                .send(Message::Binary(build_audio_only_request(seq, &chunk)?))
                                .await
                                .map_err(|e| anyhow!("send audio packet seq={seq} failed: {e}"))?;
                            seq += 1;
                        }
                    }
                    Some(StreamMsg::Finish) | None => {
                        // 录音结束 / 会话被中止:剩余不足 200ms 的尾巴作为末包(负 seq)发出。
                        let tail: Vec<u8> = std::mem::take(&mut pcm_buf);
                        write
                            .send(Message::Binary(build_audio_only_request(-seq, &tail)?))
                            .await
                            .map_err(|e| anyhow!("send last packet failed: {e}"))?;
                        break true;
                    }
                }
            }
        }
    };

    // 阶段二:末包发完后继续读,直到末包标志或 30s 尾部超时。
    if entered_finishing && !got_last {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match tokio::time::timeout_at(deadline, read.next()).await {
                Ok(Some(Ok(Message::Binary(bytes)))) => {
                    let parsed = parse_response(&bytes)?;
                    let was_last = parsed.is_last_package;
                    handle_parsed(parsed, &mut text_buf)?;
                    emit_partial!();
                    if was_last {
                        break;
                    }
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => return Err(anyhow!("Doubao stream tail read failed: {e}")),
                Err(_) => {
                    warn!("Doubao streaming tail read timed out after 30s; returning partial text");
                    break;
                }
            }
        }
    }

    let _ = write.send(Message::Close(None)).await;

    if text_buf.is_empty() {
        warn!("Doubao streaming returned empty text (no recognized speech?)");
    }
    Ok(text_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空 API key 时:`run_session` 在建连前就返回 Err,`finish()` 应快速拿到该 Err 而非阻塞/网络访问。
    /// 顺带验证 worker 线程 + 结果回传 + `finish` 收尾这套管线在离线环境下可正常工作。
    #[test]
    fn test_empty_api_key_finishes_with_error_offline() {
        let session = DoubaoStreamSession::start(String::new(), String::new(), None);
        let err = session.finish().expect_err("empty api key must error");
        assert!(
            err.to_string().contains("API key is empty"),
            "unexpected error: {err}"
        );
    }

    /// 帧 sink 必须是 `Send + 'static` 且可被调用而不 panic(即使 worker 已退出、channel 已关闭)。
    #[test]
    fn test_frame_sink_is_send_and_callable() {
        let session = DoubaoStreamSession::start(String::new(), String::new(), None);
        let mut sink = session.frame_sink();
        // 编译期约束:sink 能跨线程移动(录音器消费线程会持有它)。
        fn assert_send<T: Send + 'static>(_: &T) {}
        assert_send(&sink);
        // 向(可能已关闭的)channel 发送不应 panic。
        sink(&[0.0_f32; 480]);
        let _ = session.finish();
    }

    /// 真实端到端「边录边传」测试:读取 `DOUBAO_TEST_WAV`(16 kHz 单声道),按 ~30ms 帧通过 sink
    /// 实时喂入,再 `finish()` 收尾,打印识别结果。
    ///
    /// ```bash
    /// DOUBAO_API_KEY=xxx DOUBAO_TEST_WAV=/tmp/test.wav \
    ///     cargo test -p handy --lib doubao::stream -- --ignored --nocapture
    /// ```
    #[ignore]
    #[test]
    fn test_real_doubao_stream_with_wav() {
        let api_key = match std::env::var("DOUBAO_API_KEY") {
            Ok(k) => k,
            Err(_) => {
                eprintln!("DOUBAO_API_KEY not set; skipping streaming integration test");
                return;
            }
        };
        let wav_path = match std::env::var("DOUBAO_TEST_WAV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("DOUBAO_TEST_WAV not set; skipping streaming integration test");
                return;
            }
        };
        let resource_id = std::env::var("DOUBAO_RESOURCE_ID")
            .unwrap_or_else(|_| "volc.bigasr.sauc.duration".into());

        let mut reader = hound::WavReader::open(&wav_path).expect("open wav");
        let spec = reader.spec();
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
            other => panic!("unsupported wav sample format: {other:?}"),
        };

        let session = DoubaoStreamSession::start(api_key, resource_id, None);
        let mut sink = session.frame_sink();
        // 模拟录音:每 ~30ms(480 个 16 kHz 采样)推一帧,帧间 sleep 模拟实时节奏。
        for frame in audio.chunks(480) {
            sink(frame);
            std::thread::sleep(Duration::from_millis(30));
        }
        let text = session.finish().expect("豆包流式转录失败");
        eprintln!("流式识别结果: {text}");
        assert!(!text.trim().is_empty(), "真实语音应当能识别出文本");
    }
}
