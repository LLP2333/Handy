//! 豆包(火山引擎)流式语音识别 2.0 客户端。
//!
//! 端点:转录走双向流式优化版 `wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_async`
//! (低延迟,只在结果变化时回包);「测试连接」走 `bigmodel_nostream`(发配置即回 ack)。
//! 鉴权:新版控制台只需 `X-Api-Key`(配合 `X-Api-Resource-Id` 选定模型版本)。
//!
//! 协议参考:`docs/豆包语音输入接入.md` 与 `docs/sauc_go/`(官方 Go 示例)。
//!
//! 对外只暴露 [`DoubaoClient`],内部分为四个子模块:
//!
//! - `protocol` —— 二进制帧 header 编码与 gzip 压缩工具
//! - `payload` —— full client request / audio only request 的封装
//! - `response` —— 服务端二进制帧解析
//! - `client` —— WebSocket 端到端调用流程

mod client;
mod payload;
mod protocol;
mod response;

pub use client::DoubaoClient;
