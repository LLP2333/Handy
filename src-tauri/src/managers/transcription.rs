use crate::audio_toolkit::{apply_custom_words, filter_transcription_output};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::model::{EngineType, ModelManager};
use crate::settings::{
    get_settings, AppSettings, ModelUnloadTimeout, OrtAcceleratorSetting, WhisperAcceleratorSetting,
};
use anyhow::Result;
use log::{debug, error, info, warn};
use serde::Serialize;
use specta::Type;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter, Manager};
use transcribe_rs::{
    onnx::{
        canary::CanaryModel,
        cohere::CohereModel,
        gigaam::GigaAMModel,
        moonshine::{MoonshineModel, MoonshineVariant, StreamingModel},
        parakeet::{ParakeetModel, ParakeetParams, TimestampGranularity},
        sense_voice::{SenseVoiceModel, SenseVoiceParams},
        Quantization,
    },
    whisper_cpp::{WhisperEngine, WhisperInferenceParams},
    SpeechModel, TranscribeOptions,
};

/// 豆包流式「中间结果回调」的装箱类型:每次识别全量文本变化时以最新文本调用一次。
type DoubaoPartialCb = Box<dyn FnMut(String) + Send>;

/// 引擎原始输出的统一文本后处理:自定义词汇纠正 + 填充词/幻觉过滤。
///
/// `skip_custom_words` 为 `true` 时跳过词汇纠正——Whisper 引擎已把词表注入
/// `initial_prompt`,再跑相似度纠正属于重复处理。
///
/// 批量路径([`TranscriptionManager::transcribe`])与豆包流式收尾(`actions.rs`)
/// 共用本函数,保证两条路径对同一段原始文本产出一致结果。
pub fn postprocess_transcript_text(
    raw_text: &str,
    skip_custom_words: bool,
    settings: &AppSettings,
) -> String {
    let corrected = if !settings.custom_words.is_empty() && !skip_custom_words {
        apply_custom_words(
            raw_text,
            &settings.custom_words,
            settings.word_correction_threshold,
        )
    } else {
        raw_text.to_string()
    };
    filter_transcription_output(
        &corrected,
        &settings.app_language,
        &settings.custom_filler_words,
    )
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelStateEvent {
    pub event_type: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub error: Option<String>,
}

enum LoadedEngine {
    Whisper(WhisperEngine),
    Parakeet(ParakeetModel),
    Moonshine(MoonshineModel),
    MoonshineStreaming(StreamingModel),
    SenseVoice(SenseVoiceModel),
    GigaAM(GigaAMModel),
    Canary(CanaryModel),
    Cohere(CohereModel),
    /// 豆包云端 ASR 客户端。每次 transcribe 调用都新建 WebSocket 连接,客户端本身无状态。
    Doubao(crate::cloud_asr::doubao::DoubaoClient),
}

/// RAII guard that clears the `is_loading` flag and notifies waiters on drop.
/// Ensures the loading flag is always reset, even on early returns or panics.
pub struct LoadingGuard {
    is_loading: Arc<Mutex<bool>>,
    loading_condvar: Arc<Condvar>,
}

impl Drop for LoadingGuard {
    fn drop(&mut self) {
        let mut is_loading = self.is_loading.lock().unwrap();
        *is_loading = false;
        self.loading_condvar.notify_all();
    }
}

#[derive(Clone)]
pub struct TranscriptionManager {
    engine: Arc<Mutex<Option<LoadedEngine>>>,
    model_manager: Arc<ModelManager>,
    app_handle: AppHandle,
    current_model_id: Arc<Mutex<Option<String>>>,
    last_activity: Arc<AtomicU64>,
    shutdown_signal: Arc<AtomicBool>,
    watcher_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    is_loading: Arc<Mutex<bool>>,
    loading_condvar: Arc<Condvar>,
    /// 当前进行中的豆包「边录边传」流式会话(仅豆包引擎、且已配置 API key 时存在)。
    /// 录音开始时由 [`Self::begin_doubao_stream`] 创建,结束时由 [`Self::take_doubao_stream`] 取走收尾。
    doubao_stream: Arc<Mutex<Option<crate::cloud_asr::doubao::DoubaoStreamSession>>>,
    /// 当前「逐字上屏」会话状态(仅 `streaming_paste` 开启 + 豆包 + 非 None 粘贴方式时存在)。
    /// 与 `doubao_stream` 同生命周期:录音开始创建,松手由 [`Self::finalize_streaming_paste`] 收尾、
    /// 取消由 [`Self::abort_streaming_paste`] 清理。
    streaming_paste: Arc<Mutex<Option<crate::streaming_paste::StreamingPaste>>>,
}

impl TranscriptionManager {
    pub fn new(app_handle: &AppHandle, model_manager: Arc<ModelManager>) -> Result<Self> {
        let manager = Self {
            engine: Arc::new(Mutex::new(None)),
            model_manager,
            app_handle: app_handle.clone(),
            current_model_id: Arc::new(Mutex::new(None)),
            last_activity: Arc::new(AtomicU64::new(Self::now_ms())),
            shutdown_signal: Arc::new(AtomicBool::new(false)),
            watcher_handle: Arc::new(Mutex::new(None)),
            is_loading: Arc::new(Mutex::new(false)),
            loading_condvar: Arc::new(Condvar::new()),
            doubao_stream: Arc::new(Mutex::new(None)),
            streaming_paste: Arc::new(Mutex::new(None)),
        };

        // Start the idle watcher
        {
            let app_handle_cloned = app_handle.clone();
            let manager_cloned = manager.clone();
            let shutdown_signal = manager.shutdown_signal.clone();
            let handle = thread::spawn(move || {
                debug!("Idle watcher thread started");
                while !shutdown_signal.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(10)); // Check every 10 seconds

                    // Check shutdown signal again after sleep
                    if shutdown_signal.load(Ordering::Relaxed) {
                        break;
                    }

                    let settings = get_settings(&app_handle_cloned);
                    let timeout = settings.model_unload_timeout;

                    // Skip Immediately — that variant is handled by
                    // maybe_unload_immediately() after each transcription.
                    // Treating it as 0s here would unload the model mid-recording.
                    if timeout == ModelUnloadTimeout::Immediately {
                        continue;
                    }

                    // While recording, keep the idle timer fresh so the
                    // model is never unloaded mid-session.
                    let is_recording = app_handle_cloned
                        .try_state::<Arc<AudioRecordingManager>>()
                        .map_or(false, |a| a.is_recording());
                    if is_recording {
                        manager_cloned.touch_activity();
                        continue;
                    }

                    if let Some(limit_seconds) = timeout.to_seconds() {
                        let last = manager_cloned.last_activity.load(Ordering::Relaxed);
                        let now_ms = TranscriptionManager::now_ms();
                        let idle_ms = now_ms.saturating_sub(last);
                        let limit_ms = limit_seconds * 1000;

                        if idle_ms > limit_ms {
                            // idle -> unload
                            if manager_cloned.is_model_loaded() {
                                let unload_start = std::time::Instant::now();
                                info!(
                                    "Model idle for {}s (limit: {}s), unloading",
                                    idle_ms / 1000,
                                    limit_seconds
                                );
                                match manager_cloned.unload_model() {
                                    Ok(()) => {
                                        let unload_duration = unload_start.elapsed();
                                        info!(
                                            "Model unloaded due to inactivity (took {}ms)",
                                            unload_duration.as_millis()
                                        );
                                    }
                                    Err(e) => {
                                        error!("Failed to unload idle model: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }
                debug!("Idle watcher thread shutting down gracefully");
            });
            *manager.watcher_handle.lock().unwrap() = Some(handle);
        }

        Ok(manager)
    }

    /// Lock the engine mutex, recovering from poison if a previous transcription panicked.
    fn lock_engine(&self) -> MutexGuard<'_, Option<LoadedEngine>> {
        self.engine.lock().unwrap_or_else(|poisoned| {
            warn!("Engine mutex was poisoned by a previous panic, recovering");
            poisoned.into_inner()
        })
    }

    pub fn is_model_loaded(&self) -> bool {
        let engine = self.lock_engine();
        engine.is_some()
    }

    /// Atomically check whether a model load is in progress and, if not, mark
    /// one as starting. Returns a [`LoadingGuard`] whose [`Drop`] impl will
    /// clear the flag and wake waiters. Returns `None` if a load is already in
    /// progress.
    pub fn try_start_loading(&self) -> Option<LoadingGuard> {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading {
            return None;
        }
        *is_loading = true;
        Some(LoadingGuard {
            is_loading: self.is_loading.clone(),
            loading_condvar: self.loading_condvar.clone(),
        })
    }

    pub fn unload_model(&self) -> Result<()> {
        let unload_start = std::time::Instant::now();
        debug!("Starting to unload model");

        {
            let mut engine = self.lock_engine();
            // Dropping the engine frees all resources
            *engine = None;
        }
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            *current_model = None;
        }

        // Emit unloaded event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "unloaded".to_string(),
                model_id: None,
                model_name: None,
                error: None,
            },
        );

        let unload_duration = unload_start.elapsed();
        debug!(
            "Model unloaded manually (took {}ms)",
            unload_duration.as_millis()
        );
        Ok(())
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// Reset the idle timer to now.
    fn touch_activity(&self) {
        self.last_activity.store(Self::now_ms(), Ordering::Relaxed);
    }

    /// Unloads the model immediately if the setting is enabled and the model is loaded
    pub fn maybe_unload_immediately(&self, context: &str) {
        let settings = get_settings(&self.app_handle);
        if settings.model_unload_timeout == ModelUnloadTimeout::Immediately
            && self.is_model_loaded()
        {
            info!("Immediately unloading model after {}", context);
            if let Err(e) = self.unload_model() {
                warn!("Failed to immediately unload model: {}", e);
            }
        }
    }

    pub fn load_model(&self, model_id: &str) -> Result<()> {
        let load_start = std::time::Instant::now();
        debug!("Starting to load model: {}", model_id);

        // Emit loading started event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_started".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: None,
                error: None,
            },
        );

        let model_info = self
            .model_manager
            .get_model_info(model_id)
            .ok_or_else(|| anyhow::anyhow!("Model not found: {}", model_id))?;

        if !model_info.is_downloaded {
            let error_msg = "Model not downloaded";
            let _ = self.app_handle.emit(
                "model-state-changed",
                ModelStateEvent {
                    event_type: "loading_failed".to_string(),
                    model_id: Some(model_id.to_string()),
                    model_name: Some(model_info.name.clone()),
                    error: Some(error_msg.to_string()),
                },
            );
            return Err(anyhow::anyhow!(error_msg));
        }

        let model_path = self.model_manager.get_model_path(model_id)?;

        // Create appropriate engine based on model type
        let emit_loading_failed = |error_msg: &str| {
            let _ = self.app_handle.emit(
                "model-state-changed",
                ModelStateEvent {
                    event_type: "loading_failed".to_string(),
                    model_id: Some(model_id.to_string()),
                    model_name: Some(model_info.name.clone()),
                    error: Some(error_msg.to_string()),
                },
            );
        };

        let loaded_engine = match model_info.engine_type {
            EngineType::Whisper => {
                let engine = WhisperEngine::load(&model_path).map_err(|e| {
                    let error_msg = format!("Failed to load whisper model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::Whisper(engine)
            }
            EngineType::Parakeet => {
                let engine =
                    ParakeetModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                        let error_msg =
                            format!("Failed to load parakeet model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::Parakeet(engine)
            }
            EngineType::Moonshine => {
                let engine = MoonshineModel::load(
                    &model_path,
                    MoonshineVariant::Base,
                    &Quantization::default(),
                )
                .map_err(|e| {
                    let error_msg = format!("Failed to load moonshine model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::Moonshine(engine)
            }
            EngineType::MoonshineStreaming => {
                let engine = StreamingModel::load(&model_path, 0, &Quantization::default())
                    .map_err(|e| {
                        let error_msg = format!(
                            "Failed to load moonshine streaming model {}: {}",
                            model_id, e
                        );
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::MoonshineStreaming(engine)
            }
            EngineType::SenseVoice => {
                let engine =
                    SenseVoiceModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                        let error_msg =
                            format!("Failed to load SenseVoice model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                LoadedEngine::SenseVoice(engine)
            }
            EngineType::GigaAM => {
                let engine = GigaAMModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load gigaam model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::GigaAM(engine)
            }
            EngineType::Canary => {
                let engine = CanaryModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load canary model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::Canary(engine)
            }
            EngineType::Cohere => {
                let engine = CohereModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load cohere model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                LoadedEngine::Cohere(engine)
            }
            EngineType::Doubao => {
                // 云端模型不依赖 model_path,只用凭据构造客户端。
                // 读取设置中的 doubao_credentials(SecretMap),api_key 必填、resource_id 可选。
                let settings = get_settings(&self.app_handle);
                let api_key = settings
                    .doubao_credentials
                    .get("api_key")
                    .cloned()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        let msg =
                            "Doubao API key not configured. Please set it in Settings → Models.";
                        emit_loading_failed(msg);
                        anyhow::anyhow!(msg)
                    })?;
                let resource_id = settings
                    .doubao_credentials
                    .get("resource_id")
                    .cloned()
                    .unwrap_or_default();
                LoadedEngine::Doubao(crate::cloud_asr::doubao::DoubaoClient::new(
                    api_key,
                    resource_id,
                ))
            }
        };

        // Update the current engine and model ID
        {
            let mut engine = self.lock_engine();
            *engine = Some(loaded_engine);
        }
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            *current_model = Some(model_id.to_string());
        }

        // Reset idle timer so the watcher doesn't immediately unload a just-loaded model
        self.touch_activity();

        // Emit loading completed event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_completed".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: Some(model_info.name.clone()),
                error: None,
            },
        );

        let load_duration = load_start.elapsed();
        debug!(
            "Successfully loaded transcription model: {} (took {}ms)",
            model_id,
            load_duration.as_millis()
        );
        Ok(())
    }

    /// Returns true when the currently loaded engine matches `settings.selected_model`.
    ///
    /// Guards against "stale engine" — `is_model_loaded()` alone is not enough because
    /// the loaded engine may belong to the *previously* selected model (e.g. cloud
    /// models intentionally skip eager load on selection, so the old engine survives
    /// a model switch). Without this check, transcribe() would silently dispatch
    /// audio to the wrong engine.
    fn loaded_engine_matches_selection(&self) -> bool {
        let target = get_settings(&self.app_handle).selected_model;
        self.get_current_model().as_deref() == Some(target.as_str()) && self.is_model_loaded()
    }

    /// Kicks off the model loading in a background thread if it's not already loaded,
    /// or if the loaded engine doesn't match the currently selected model.
    pub fn initiate_model_load(&self) {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading || self.loaded_engine_matches_selection() {
            return;
        }

        *is_loading = true;
        let self_clone = self.clone();
        thread::spawn(move || {
            let settings = get_settings(&self_clone.app_handle);
            if let Err(e) = self_clone.load_model(&settings.selected_model) {
                error!("Failed to load model: {}", e);
            }
            let mut is_loading = self_clone.is_loading.lock().unwrap();
            *is_loading = false;
            self_clone.loading_condvar.notify_all();
        });
    }

    pub fn get_current_model(&self) -> Option<String> {
        let current_model = self.current_model_id.lock().unwrap();
        current_model.clone()
    }

    /// 若当前模型是豆包且已配置 API key,则在录音开始时启动一个「边录边传」流式会话,并把实时帧 sink
    /// 挂到录音器上。非豆包 / 未配置凭据时直接返回(走原有「整段批量转写」)。
    ///
    /// 这是低延迟路径的入口:音频在录音过程中就被持续上传,松手时只需收尾。失败不致命——录音器仍照常
    /// 累积整段音频,[`Self::take_doubao_stream`] 收尾失败时上层会回退到 [`Self::transcribe`]。
    ///
    /// 副作用:建立 WebSocket 连接(后台线程)、向 `rm` 注册实时帧 sink、写入 `doubao_stream` 状态。
    pub fn begin_doubao_stream(&self, rm: &AudioRecordingManager) {
        let settings = get_settings(&self.app_handle);
        let is_doubao = self
            .model_manager
            .get_model_info(&settings.selected_model)
            .map(|m| matches!(m.engine_type, EngineType::Doubao))
            .unwrap_or(false);
        if !is_doubao {
            return;
        }

        let Some(api_key) = settings
            .doubao_credentials
            .get("api_key")
            .cloned()
            .filter(|s| !s.trim().is_empty())
        else {
            debug!("Doubao streaming skipped: API key not configured");
            return;
        };
        let resource_id = settings
            .doubao_credentials
            .get("resource_id")
            .cloned()
            .unwrap_or_default();

        // 「逐字上屏」:开启且粘贴方式非 None 时,构建中间结果回调,把豆包流式中间结果实时键入输入框。
        let streaming_enabled =
            settings.streaming_paste && settings.paste_method != crate::settings::PasteMethod::None;
        let sp_state = streaming_enabled.then(crate::streaming_paste::StreamingPaste::new);
        let on_partial: Option<DoubaoPartialCb> = sp_state.as_ref().map(|sp| {
            let sp_for_cb = sp.clone();
            let app = self.app_handle.clone();
            // 回调在豆包网络线程触发 → marshal 到主线程执行 enigo 键入(与正常粘贴一致)。
            Box::new(move |full_text: String| {
                let sp = sp_for_cb.clone();
                let app_main = app.clone();
                let _ = app.run_on_main_thread(move || {
                    sp.apply_partial(&app_main, &full_text);
                });
            }) as DoubaoPartialCb
        });

        let session =
            crate::cloud_asr::doubao::DoubaoStreamSession::start(api_key, resource_id, on_partial);
        rm.set_frame_sink(Box::new(session.frame_sink()));
        *self.doubao_stream.lock().unwrap() = Some(session);
        *self.streaming_paste.lock().unwrap() = sp_state;
        debug!(
            "Doubao streaming session started (record-while-streaming, streaming_paste={})",
            streaming_enabled
        );
    }

    /// 取走当前的豆包流式会话(若有)。上层在录音停止后调用:`Some` 时用 [`DoubaoStreamSession::finish`]
    /// 收尾取最终文本,`None` 时走原有整段批量转写。
    ///
    /// [`DoubaoStreamSession::finish`]: crate::cloud_asr::doubao::DoubaoStreamSession::finish
    pub fn take_doubao_stream(&self) -> Option<crate::cloud_asr::doubao::DoubaoStreamSession> {
        self.doubao_stream.lock().unwrap().take()
    }

    /// 按当前设置对一段引擎原始输出做统一后处理(自定义词纠正 + 填充词过滤)。
    ///
    /// [`Self::transcribe`](批量路径)内部已应用同样的处理;豆包「边录边传」流式收尾
    /// 的文本不经过 `transcribe`,由 `actions.rs` 对 `finish()` 结果显式调用本方法,
    /// 保证流式与批量两条路径行为一致。
    pub fn postprocess_transcript(&self, raw_text: &str) -> String {
        let settings = get_settings(&self.app_handle);
        let is_whisper = self
            .model_manager
            .get_model_info(&settings.selected_model)
            .map(|info| matches!(info.engine_type, EngineType::Whisper))
            .unwrap_or(false);
        postprocess_transcript_text(raw_text, is_whisper, &settings)
    }

    /// 中止进行中的豆包流式会话(取消录音时调用):先卸载录音器 sink,再丢弃会话,并清理「逐字上屏」
    /// 已键入的中间文本(退格删除)。
    pub fn abort_doubao_stream(&self, rm: &AudioRecordingManager) {
        rm.clear_frame_sink();
        if let Some(session) = self.doubao_stream.lock().unwrap().take() {
            session.abort();
        }
        self.abort_streaming_paste();
    }

    /// 「逐字上屏」松手收尾:把屏幕文本对齐到 `final_text` 并处理尾随空格 / 自动提交 / 剪贴板。
    ///
    /// 返回 `None` 表示本次录音未启用逐字上屏(无会话状态),上层应回退到整段 [`crate::utils::paste`];
    /// 返回 `Some(Ok)` 表示已通过增量键入完成上屏(不应再整段粘贴);`Some(Err)` 为键入过程出错。
    ///
    /// 内部在主线程执行 enigo 键入,因此调用方需保证已处于主线程上下文(actions.rs 的粘贴回调即是)。
    pub fn finalize_streaming_paste(&self, final_text: &str) -> Option<Result<(), String>> {
        let sp = self.streaming_paste.lock().unwrap().take()?;
        Some(sp.finalize(&self.app_handle, final_text))
    }

    /// 清理「逐字上屏」会话:把已键入的中间文本退格删除并丢弃状态。无会话时为无操作。
    ///
    /// 取消录音、最终文本为空、转写失败等场景调用,避免在输入框遗留半截识别文本。键入在主线程执行
    /// (内部 marshal)。
    pub fn abort_streaming_paste(&self) {
        if let Some(sp) = self.streaming_paste.lock().unwrap().take() {
            let app = self.app_handle.clone();
            let _ = self.app_handle.run_on_main_thread(move || sp.abort(&app));
        }
    }

    pub fn transcribe(&self, audio: Vec<f32>) -> Result<String> {
        #[cfg(debug_assertions)]
        if std::env::var("HANDY_FORCE_TRANSCRIPTION_FAILURE").is_ok() {
            return Err(anyhow::anyhow!(
                "Simulated transcription failure (HANDY_FORCE_TRANSCRIPTION_FAILURE)"
            ));
        }

        // Update last activity timestamp
        self.touch_activity();

        let st = std::time::Instant::now();

        debug!("Audio vector length: {}", audio.len());

        if audio.is_empty() {
            debug!("Empty audio vector");
            self.maybe_unload_immediately("empty audio");
            return Ok(String::new());
        }

        // Check if model is loaded, if not try to load it
        {
            // If the model is loading, wait for it to complete.
            let mut is_loading = self.is_loading.lock().unwrap();
            while *is_loading {
                is_loading = self.loading_condvar.wait(is_loading).unwrap();
            }

            let engine_guard = self.lock_engine();
            if engine_guard.is_none() {
                return Err(anyhow::anyhow!("Model is not loaded for transcription."));
            }
        }

        // Get current settings for configuration
        let settings = get_settings(&self.app_handle);

        // Validate selected language against the model's supported languages.
        // If the language isn't supported, fall back to "auto" to prevent errors.
        let validated_language = if settings.selected_language == "auto" {
            "auto".to_string()
        } else {
            let is_supported = self
                .model_manager
                .get_model_info(&settings.selected_model)
                .map(|info| {
                    info.supported_languages.is_empty()
                        || info
                            .supported_languages
                            .contains(&settings.selected_language)
                })
                .unwrap_or(true);

            if is_supported {
                settings.selected_language.clone()
            } else {
                warn!(
                    "Language '{}' not supported by current model, falling back to auto-detect",
                    settings.selected_language
                );
                "auto".to_string()
            }
        };

        // Perform transcription with the appropriate engine.
        // We use catch_unwind to prevent engine panics from poisoning the mutex,
        // which would make the app hang indefinitely on subsequent operations.
        let result = {
            let mut engine_guard = self.lock_engine();

            // Take the engine out so we own it during transcription.
            // If the engine panics, we simply don't put it back (effectively unloading it)
            // instead of poisoning the mutex.
            let mut engine = match engine_guard.take() {
                Some(e) => e,
                None => {
                    return Err(anyhow::anyhow!(
                        "Model failed to load after auto-load attempt. Please check your model settings."
                    ));
                }
            };

            // Release the lock before transcribing — no mutex held during the engine call
            drop(engine_guard);

            let transcribe_result = catch_unwind(AssertUnwindSafe(
                || -> Result<transcribe_rs::TranscriptionResult> {
                    match &mut engine {
                        LoadedEngine::Whisper(whisper_engine) => {
                            let whisper_language = if validated_language == "auto" {
                                None
                            } else {
                                let normalized = if validated_language == "zh-Hans"
                                    || validated_language == "zh-Hant"
                                {
                                    "zh".to_string()
                                } else {
                                    validated_language.clone()
                                };
                                Some(normalized)
                            };

                            let params = WhisperInferenceParams {
                                language: whisper_language,
                                translate: settings.translate_to_english,
                                initial_prompt: if settings.custom_words.is_empty() {
                                    None
                                } else {
                                    Some(settings.custom_words.join(", "))
                                },
                                ..Default::default()
                            };

                            whisper_engine
                                .transcribe_with(&audio, &params)
                                .map_err(|e| anyhow::anyhow!("Whisper transcription failed: {}", e))
                        }
                        LoadedEngine::Parakeet(parakeet_engine) => {
                            let params = ParakeetParams {
                                timestamp_granularity: Some(TimestampGranularity::Segment),
                                ..Default::default()
                            };
                            parakeet_engine
                                .transcribe_with(&audio, &params)
                                .map_err(|e| {
                                    anyhow::anyhow!("Parakeet transcription failed: {}", e)
                                })
                        }
                        LoadedEngine::Moonshine(moonshine_engine) => moonshine_engine
                            .transcribe(&audio, &TranscribeOptions::default())
                            .map_err(|e| anyhow::anyhow!("Moonshine transcription failed: {}", e)),
                        LoadedEngine::MoonshineStreaming(streaming_engine) => streaming_engine
                            .transcribe(&audio, &TranscribeOptions::default())
                            .map_err(|e| {
                                anyhow::anyhow!("Moonshine streaming transcription failed: {}", e)
                            }),
                        LoadedEngine::SenseVoice(sense_voice_engine) => {
                            let language = match validated_language.as_str() {
                                "zh" | "zh-Hans" | "zh-Hant" => Some("zh".to_string()),
                                "en" => Some("en".to_string()),
                                "ja" => Some("ja".to_string()),
                                "ko" => Some("ko".to_string()),
                                "yue" => Some("yue".to_string()),
                                _ => None,
                            };
                            let params = SenseVoiceParams {
                                language,
                                use_itn: Some(true),
                            };
                            sense_voice_engine
                                .transcribe_with(&audio, &params)
                                .map_err(|e| {
                                    anyhow::anyhow!("SenseVoice transcription failed: {}", e)
                                })
                        }
                        LoadedEngine::GigaAM(gigaam_engine) => gigaam_engine
                            .transcribe(&audio, &TranscribeOptions::default())
                            .map_err(|e| anyhow::anyhow!("GigaAM transcription failed: {}", e)),
                        LoadedEngine::Canary(canary_engine) => {
                            let lang = if validated_language == "auto" {
                                None
                            } else {
                                Some(validated_language.clone())
                            };
                            let options = TranscribeOptions {
                                language: lang,
                                translate: settings.translate_to_english,
                                ..Default::default()
                            };
                            canary_engine
                                .transcribe(&audio, &options)
                                .map_err(|e| anyhow::anyhow!("Canary transcription failed: {}", e))
                        }
                        LoadedEngine::Cohere(cohere_engine) => {
                            let lang = if validated_language == "auto" {
                                None
                            } else if validated_language == "zh-Hans"
                                || validated_language == "zh-Hant"
                            {
                                Some("zh".to_string())
                            } else {
                                Some(validated_language.clone())
                            };
                            let options = TranscribeOptions {
                                language: lang,
                                ..Default::default()
                            };
                            cohere_engine
                                .transcribe(&audio, &options)
                                .map_err(|e| anyhow::anyhow!("Cohere transcription failed: {}", e))
                        }
                        LoadedEngine::Doubao(client) => {
                            // 把 Handy 的简码映射到豆包要求的 BCP-47 语种代码。
                            // 未列出的代码原样透传(豆包文档列了 25 种,常见前缀不冲突)。
                            let lang = match validated_language.as_str() {
                                "auto" => None,
                                "zh" | "zh-Hans" | "zh-Hant" => Some("zh-CN".to_string()),
                                "en" => Some("en-US".to_string()),
                                "ja" => Some("ja-JP".to_string()),
                                "ko" => Some("ko-KR".to_string()),
                                "yue" => Some("yue-CN".to_string()),
                                "ru" => Some("ru-RU".to_string()),
                                "fr" => Some("fr-FR".to_string()),
                                "de" => Some("de-DE".to_string()),
                                "es" => Some("es-MX".to_string()),
                                "pt" => Some("pt-BR".to_string()),
                                "id" => Some("id-ID".to_string()),
                                "fil" => Some("fil-PH".to_string()),
                                "ms" => Some("ms-MY".to_string()),
                                "th" => Some("th-TH".to_string()),
                                "ar" => Some("ar-SA".to_string()),
                                "it" => Some("it-IT".to_string()),
                                "bn" => Some("bn-BD".to_string()),
                                "el" => Some("el-GR".to_string()),
                                "nl" => Some("nl-NL".to_string()),
                                "tr" => Some("tr-TR".to_string()),
                                "vi" => Some("vi-VN".to_string()),
                                "pl" => Some("pl-PL".to_string()),
                                "ro" => Some("ro-RO".to_string()),
                                "ne" => Some("ne-NP".to_string()),
                                "uk" => Some("uk-UA".to_string()),
                                other => Some(other.to_string()),
                            };
                            // 必须用 client 自带 runtime 的同步入口,而不是 tauri::async_runtime
                            // 的 block_on——快捷键录音路径下,这个同步 transcribe() 是从一个
                            // 已经持有 tokio runtime 的 worker thread 调过来的,在那种线程上
                            // 调外层 runtime 的 block_on 会触发 "Cannot start a runtime from
                            // within a runtime" panic。client 内部维护独立 runtime 来避开这个坑。
                            let text = client
                                .transcribe_blocking(&audio, lang.as_deref())
                                .map_err(|e| {
                                    anyhow::anyhow!("Doubao transcription failed: {}", e)
                                })?;
                            Ok(transcribe_rs::TranscriptionResult {
                                text,
                                segments: None,
                            })
                        }
                    }
                },
            ));

            match transcribe_result {
                Ok(inner_result) => {
                    // Success or normal error — put the engine back
                    let mut engine_guard = self.lock_engine();
                    *engine_guard = Some(engine);
                    inner_result?
                }
                Err(panic_payload) => {
                    // Engine panicked — do NOT put it back (it's in an unknown state).
                    // The engine is dropped here, effectively unloading it.
                    let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic".to_string()
                    };
                    error!(
                        "Transcription engine panicked: {}. Model has been unloaded.",
                        panic_msg
                    );

                    // Clear the model ID so it will be reloaded on next attempt
                    {
                        let mut current_model = self
                            .current_model_id
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        *current_model = None;
                    }

                    let _ = self.app_handle.emit(
                        "model-state-changed",
                        ModelStateEvent {
                            event_type: "unloaded".to_string(),
                            model_id: None,
                            model_name: None,
                            error: Some(format!("Engine panicked: {}", panic_msg)),
                        },
                    );

                    return Err(anyhow::anyhow!(
                        "Transcription engine panicked: {}. The model has been unloaded and will reload on next attempt.",
                        panic_msg
                    ));
                }
            }
        };

        // Whisper 已把自定义词表注入 initial_prompt,后处理里跳过相似度纠正。
        let is_whisper = self
            .model_manager
            .get_model_info(&settings.selected_model)
            .map(|info| matches!(info.engine_type, EngineType::Whisper))
            .unwrap_or(false);

        let filtered_result = postprocess_transcript_text(&result.text, is_whisper, &settings);

        let et = std::time::Instant::now();
        let translation_note = if settings.translate_to_english {
            " (translated)"
        } else {
            ""
        };
        info!(
            "Transcription completed in {}ms{}",
            (et - st).as_millis(),
            translation_note
        );

        let final_result = filtered_result;

        if final_result.is_empty() {
            info!("Transcription result is empty");
        } else {
            info!("Transcription result: {}", final_result);
        }

        self.maybe_unload_immediately("transcription");

        Ok(final_result)
    }
}

/// Apply the user's accelerator preferences to the transcribe-rs global atomics.
/// Called on startup and whenever the user changes the setting.
pub fn apply_accelerator_settings(app: &tauri::AppHandle) {
    use transcribe_rs::accel;

    let settings = get_settings(app);

    let whisper_pref = match settings.whisper_accelerator {
        WhisperAcceleratorSetting::Auto => accel::WhisperAccelerator::Auto,
        WhisperAcceleratorSetting::Cpu => accel::WhisperAccelerator::CpuOnly,
        WhisperAcceleratorSetting::Gpu => accel::WhisperAccelerator::Gpu,
    };
    accel::set_whisper_accelerator(whisper_pref);
    accel::set_whisper_gpu_device(settings.whisper_gpu_device);
    info!(
        "Whisper accelerator set to: {}, gpu_device: {}",
        whisper_pref,
        if settings.whisper_gpu_device == accel::GPU_DEVICE_AUTO {
            "auto".to_string()
        } else {
            settings.whisper_gpu_device.to_string()
        }
    );

    let ort_pref = match settings.ort_accelerator {
        OrtAcceleratorSetting::Auto => accel::OrtAccelerator::Auto,
        OrtAcceleratorSetting::Cpu => accel::OrtAccelerator::CpuOnly,
        OrtAcceleratorSetting::Cuda => accel::OrtAccelerator::Cuda,
        OrtAcceleratorSetting::DirectMl => accel::OrtAccelerator::DirectMl,
        OrtAcceleratorSetting::Rocm => accel::OrtAccelerator::Rocm,
    };
    accel::set_ort_accelerator(ort_pref);
    info!("ORT accelerator set to: {}", ort_pref);
}

#[derive(Serialize, Clone, Debug, Type)]
pub struct GpuDeviceOption {
    pub id: i32,
    pub name: String,
    pub total_vram_mb: usize,
}

static GPU_DEVICES: OnceLock<Vec<GpuDeviceOption>> = OnceLock::new();

fn cached_gpu_devices() -> &'static [GpuDeviceOption] {
    use transcribe_rs::whisper_cpp::gpu::list_gpu_devices;

    GPU_DEVICES.get_or_init(|| {
        // ggml's Vulkan backend uses FMA3 instructions internally.
        // On older CPUs without FMA3 (e.g. Sandy Bridge Xeons) this causes
        // a SIGILL crash that cannot be caught. Skip enumeration entirely
        // on those CPUs — GPU-accelerated whisper won't work there anyway.
        #[cfg(target_arch = "x86_64")]
        if !std::arch::is_x86_feature_detected!("fma") {
            warn!("CPU lacks FMA3 support — skipping GPU device enumeration");
            return Vec::new();
        }

        list_gpu_devices()
            .into_iter()
            .map(|d| GpuDeviceOption {
                id: d.id,
                name: d.name,
                total_vram_mb: d.total_vram / (1024 * 1024),
            })
            .collect()
    })
}

#[derive(Serialize, Clone, Debug, Type)]
pub struct AvailableAccelerators {
    pub whisper: Vec<String>,
    pub ort: Vec<String>,
    pub gpu_devices: Vec<GpuDeviceOption>,
}

/// Return which accelerators are compiled into this build.
pub fn get_available_accelerators() -> AvailableAccelerators {
    use transcribe_rs::accel::OrtAccelerator;

    let ort_options: Vec<String> = OrtAccelerator::available()
        .into_iter()
        .map(|a| a.to_string())
        .collect();

    let whisper_options = vec!["auto".to_string(), "cpu".to_string(), "gpu".to_string()];

    AvailableAccelerators {
        whisper: whisper_options,
        ort: ort_options,
        gpu_devices: cached_gpu_devices().to_vec(),
    }
}

impl Drop for TranscriptionManager {
    fn drop(&mut self) {
        // Skip shutdown unless this is the very last clone. TranscriptionManager
        // is cloned by initiate_model_load() and the watcher thread — those
        // clones dropping must not kill the watcher. The watcher thread holds
        // its own clone, so engine's strong_count is always >= 2 while the
        // watcher is alive. When it reaches 1, only this instance remains
        // and we can safely shut down.
        if Arc::strong_count(&self.engine) > 1 {
            return;
        }

        // Signal the watcher thread to shutdown
        self.shutdown_signal.store(true, Ordering::Relaxed);

        // Wait for the thread to finish gracefully
        if let Some(handle) = self.watcher_handle.lock().unwrap().take() {
            if let Err(e) = handle.join() {
                warn!("Failed to join idle watcher thread: {:?}", e);
            } else {
                debug!("Idle watcher thread joined successfully");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::get_default_settings;

    /// 精确小写匹配的自定义词应被纠正为词表原始大小写;`skip_custom_words`(Whisper 路径)
    /// 时保持原样。这是流式/批量两条路径共用后处理的核心行为。
    #[test]
    fn test_postprocess_applies_custom_words_unless_skipped() {
        let mut settings = get_default_settings();
        settings.custom_words = vec!["Handy".to_string()];

        let corrected = postprocess_transcript_text("handy is nice", false, &settings);
        assert_eq!(corrected, "Handy is nice");

        let skipped = postprocess_transcript_text("handy is nice", true, &settings);
        assert_eq!(skipped, "handy is nice");
    }

    /// 没有配置自定义词时,普通文本原样通过(不被误改)。
    #[test]
    fn test_postprocess_passthrough_without_custom_words() {
        let settings = get_default_settings();
        assert_eq!(
            postprocess_transcript_text("hello world", false, &settings),
            "hello world"
        );
    }
}
