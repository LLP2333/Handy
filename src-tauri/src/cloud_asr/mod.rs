//! 云端 ASR 提供商客户端集合。
//!
//! 与 [`managers`](crate::managers) 下基于 [`transcribe-rs`] 的本地推理引擎不同,本模块下的
//! 子模块通过网络协议(WebSocket / HTTP)调用第三方 ASR 服务。每个子模块导出一个 `Client`
//! 类型,由 [`crate::managers::transcription::TranscriptionManager`] 统一调度。
//!
//! [`transcribe-rs`]: https://crates.io/crates/transcribe-rs

pub mod doubao;
