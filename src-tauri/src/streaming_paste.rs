//! 「逐字上屏」(streaming partial paste):录音过程中把豆包流式识别的中间结果实时键入当前输入框。
//!
//! 豆包 async 流式每次结果变化都回一版**全量**文本(后文会修正前文),因此本模块维护「屏幕上已键入
//! 的文本」状态,每来一版新全量文本就算出与已键入文本的**公共前缀**,对差异尾部发退格、再直接键入
//! 新后缀(enigo direct typing,支持中文 Unicode)。松手时再用最终(可能经后处理的)文本做一次对齐
//! 兜底。
//!
//! 仅在 `streaming_paste` 设置开启、且引擎为豆包、且 paste_method != None 时启用。键入走系统级键盘
//! 注入(enigo),会忽略 paste_method 的剪贴板路径——增量上屏本质上必须逐段键入。
//!
//! 线程模型:中间结果在豆包网络线程产出,通过 [`tauri::AppHandle::run_on_main_thread`] marshal 到
//! **主线程**执行键入;松手对齐与取消清理同样在主线程执行。因此 `committed` 状态只在主线程读写,
//! `Mutex` 仅用于跨闭包共享所有权,不存在真正的并发争用。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use enigo::{Direction, Enigo, Key, Keyboard};
use log::warn;
use tauri::{AppHandle, Manager};

use crate::input::EnigoState;
use crate::settings::{get_settings, ClipboardHandling, PasteMethod};

/// 一次「逐字上屏」会话的共享状态:记录当前已键入到目标输入框的文本。
///
/// 由 [`crate::managers::transcription::TranscriptionManager`] 在录音开始时创建并持有,中间结果回调
/// 与松手收尾共享同一份状态(`clone` 只增引用计数)。
#[derive(Clone, Default)]
pub struct StreamingPaste {
    committed: Arc<Mutex<String>>,
}

impl StreamingPaste {
    /// 新建一个空会话(屏幕上尚未键入任何流式文本)。
    pub fn new() -> Self {
        Self::default()
    }

    /// 应用一版新的全量中间结果:把屏幕文本对齐到 `new_text`(退格 + 键入差异)。
    ///
    /// 必须在主线程调用(由中间结果回调 marshal 而来)。失败仅告警、不影响后续中间结果。
    pub fn apply_partial(&self, app: &AppHandle, new_text: &str) {
        if let Err(e) = self.align_to(app, new_text) {
            warn!("streaming paste partial update failed: {e}");
        }
    }

    /// 松手收尾:把屏幕文本对齐到最终文本,并按设置补尾随空格、自动提交、复制到剪贴板。
    ///
    /// 必须在主线程调用。`final_text` 为最终(可能经后处理改写的)文本,内部会自行处理与已键入中间
    /// 文本的差异(整段改写时等价于退格清空 + 重新键入)。返回 `Err` 时由上层记录日志。
    pub fn finalize(&self, app: &AppHandle, final_text: &str) -> Result<(), String> {
        let settings = get_settings(app);
        let target = if settings.append_trailing_space {
            format!("{} ", final_text)
        } else {
            final_text.to_string()
        };

        self.align_to(app, &target)?;

        let enigo_state = app
            .try_state::<EnigoState>()
            .ok_or("Enigo state not initialized")?;

        if settings.auto_submit && settings.paste_method != PasteMethod::None {
            std::thread::sleep(Duration::from_millis(50));
            let mut enigo = enigo_state
                .0
                .lock()
                .map_err(|e| format!("Failed to lock Enigo: {e}"))?;
            crate::clipboard::send_return_key(&mut enigo, settings.auto_submit_key)?;
        }

        if settings.clipboard_handling == ClipboardHandling::CopyToClipboard {
            use tauri_plugin_clipboard_manager::ClipboardExt;
            app.clipboard()
                .write_text(&target)
                .map_err(|e| format!("Failed to copy to clipboard: {e}"))?;
        }

        Ok(())
    }

    /// 取消收尾:把已键入的中间文本全部退格删除,恢复输入框原状。
    ///
    /// 必须在主线程调用。用于取消录音(escape)或最终文本为空等场景,避免遗留半截识别文本。
    pub fn abort(&self, app: &AppHandle) {
        if let Err(e) = self.align_to(app, "") {
            warn!("streaming paste abort cleanup failed: {e}");
        }
    }

    /// 把屏幕文本对齐到 `target`:计算 `committed → target` 的增量,先退格再键入,然后更新 `committed`。
    fn align_to(&self, app: &AppHandle, target: &str) -> Result<(), String> {
        let enigo_state = app
            .try_state::<EnigoState>()
            .ok_or("Enigo state not initialized")?;

        let mut committed = self
            .committed
            .lock()
            .map_err(|e| format!("Failed to lock streaming paste state: {e}"))?;

        let (backspaces, suffix) = compute_delta(&committed, target);
        if backspaces == 0 && suffix.is_empty() {
            return Ok(());
        }

        let mut enigo = enigo_state
            .0
            .lock()
            .map_err(|e| format!("Failed to lock Enigo: {e}"))?;
        type_delta(&mut enigo, backspaces, &suffix)?;

        *committed = target.to_string();
        Ok(())
    }
}

/// 计算把 `old` 变成 `new` 所需的最小键盘操作:先退 `backspaces` 个字符,再键入 `suffix`。
///
/// 以 **Unicode 标量(`char`)** 为单位求最长公共前缀——一次退格删一个字符,中文/英文场景前缀通常
/// 稳定,只需重打发生修正的尾部。返回 `(退格次数, 需键入的后缀)`。
///
/// 注:对包含组合字符(如带变音的字母、部分 emoji 序列)的文本,「一个 `char` = 一次退格」不一定
/// 成立,可能出现少量多删/少删;对豆包中英文识别结果而言可忽略。
pub fn compute_delta(old: &str, new: &str) -> (usize, String) {
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();

    let mut prefix = 0;
    while prefix < old_chars.len()
        && prefix < new_chars.len()
        && old_chars[prefix] == new_chars[prefix]
    {
        prefix += 1;
    }

    let backspaces = old_chars.len() - prefix;
    let suffix: String = new_chars[prefix..].iter().collect();
    (backspaces, suffix)
}

/// 执行增量键盘操作:`backspaces` 次退格 + 直接键入 `suffix`。
fn type_delta(enigo: &mut Enigo, backspaces: usize, suffix: &str) -> Result<(), String> {
    for _ in 0..backspaces {
        enigo
            .key(Key::Backspace, Direction::Click)
            .map_err(|e| format!("Failed to send backspace: {e}"))?;
    }
    if !suffix.is_empty() {
        enigo
            .text(suffix)
            .map_err(|e| format!("Failed to type streaming text: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::compute_delta;

    #[test]
    fn delta_from_empty_types_whole_text() {
        let (bs, suffix) = compute_delta("", "你好");
        assert_eq!(bs, 0);
        assert_eq!(suffix, "你好");
    }

    #[test]
    fn delta_pure_append_no_backspace() {
        // 中间结果逐字增长:只需追加新字,不退格。
        let (bs, suffix) = compute_delta("你好", "你好世界");
        assert_eq!(bs, 0);
        assert_eq!(suffix, "世界");
    }

    #[test]
    fn delta_tail_revision_backspaces_only_diverging_suffix() {
        // 服务端修正了尾字:公共前缀「我想」保留,退 1 字、重打 1 字。
        let (bs, suffix) = compute_delta("我想去", "我想吃");
        assert_eq!(bs, 1);
        assert_eq!(suffix, "吃");
    }

    #[test]
    fn delta_full_rewrite_backspaces_all() {
        let (bs, suffix) = compute_delta("abc", "xyz");
        assert_eq!(bs, 3);
        assert_eq!(suffix, "xyz");
    }

    #[test]
    fn delta_to_empty_backspaces_all_no_suffix() {
        // 取消场景:清空已键入文本。
        let (bs, suffix) = compute_delta("hello", "");
        assert_eq!(bs, 5);
        assert_eq!(suffix, "");
    }

    #[test]
    fn delta_identical_is_noop() {
        let (bs, suffix) = compute_delta("same", "same");
        assert_eq!(bs, 0);
        assert_eq!(suffix, "");
    }

    #[test]
    fn delta_counts_unicode_scalars_not_bytes() {
        // 退格次数应按字符数而非字节数:删掉 2 个中文字(各 3 字节)只退 2 次。
        let (bs, suffix) = compute_delta("中文测试", "中文");
        assert_eq!(bs, 2);
        assert_eq!(suffix, "");
    }
}
