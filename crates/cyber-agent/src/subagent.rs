//! 子代理转录注册表：每个子代理一次运行（`SubagentRun`）的完整事件转录。
//!
//! `delegate_tasks` / 后台子代理在启动时 `SubagentArchive::start` 建条目，
//! `EventSink` 的 Child 模式把 Reasoning/Token/ToolCall/ToolResult 逐行写入
//! `lines`（超 500 行丢最旧），结束时 `finish` 定稿状态与结果。
//!
//! CLI 子代理面板（Ctrl+G）读 `snapshot()` 渲染列表/详情；进程退出即弃。
//! 条目在会话生命周期内不回删，结束后仍可回看完整转录。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 单次子代理运行的生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Error,
    TimedOut,
    Killed,
}

/// 一次子代理运行的完整转录（会话生命周期内可回看）。
#[derive(Debug, Clone)]
pub struct SubagentRun {
    pub id: u64,
    pub name: String,
    pub status: SubagentStatus,
    /// 事件转录行（thinking:/tool/… 逐行追加，最多保留最近 500 行）。
    pub lines: Vec<String>,
    /// 最终结果文本（Completed 时 Some）。
    pub result: Option<String>,
    /// 错误文本（Error/TimedOut/Killed 时 Some）。
    pub error: Option<String>,
    pub started: Instant,
}

/// 转录行数上限（超限丢最旧）。
const MAX_LINES: usize = 500;

/// 追加一行，超 `MAX_LINES` 丢弃最旧（在行边界）。
fn push_line(lines: &mut Vec<String>, line: String) {
    lines.push(line);
    if lines.len() > MAX_LINES {
        let drop = lines.len() - MAX_LINES;
        lines.drain(..drop);
    }
}

/// 子代理转录注册表。条目以 `Arc<Mutex<SubagentRun>>` 持有：
/// 写方（EventSink / 结束定稿）持句柄直改，读方（CLI 面板）`snapshot()` clone。
#[derive(Default)]
pub struct SubagentArchive {
    runs: Mutex<Vec<Arc<Mutex<SubagentRun>>>>,
    next_id: AtomicU64,
}

impl SubagentArchive {
    /// 新建 Running 条目，返回 run id。
    pub fn start(&self, name: &str) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let run = Arc::new(Mutex::new(SubagentRun {
            id,
            name: name.to_string(),
            status: SubagentStatus::Running,
            lines: Vec::new(),
            result: None,
            error: None,
            started: Instant::now(),
        }));
        if let Ok(mut runs) = self.runs.lock() {
            runs.push(run);
        }
        id
    }

    /// 取运行句柄（供 EventSink 写转录 / 结束定稿）。条目不存在返回 None。
    pub fn run_handle(&self, id: u64) -> Option<Arc<Mutex<SubagentRun>>> {
        self.runs
            .lock()
            .ok()?
            .iter()
            .find(|run| run.lock().map(|r| r.id == id).unwrap_or(false))
            .cloned()
    }

    /// 按 id 追加一行转录；条目不存在则静默忽略。
    pub fn append_line(&self, id: u64, line: String) {
        if let Some(run) = self.run_handle(id) {
            if let Ok(mut run) = run.lock() {
                push_line(&mut run.lines, line);
            }
        }
    }

    /// 定稿一次运行的状态与结果/错误。
    pub fn finish(
        &self,
        id: u64,
        status: SubagentStatus,
        result: Option<String>,
        error: Option<String>,
    ) {
        if let Some(run) = self.run_handle(id) {
            if let Ok(mut run) = run.lock() {
                run.status = status;
                run.result = result;
                run.error = error;
            }
        }
    }

    /// 全部条目快照（clone，供渲染）。
    pub fn snapshot(&self) -> Vec<SubagentRun> {
        self.runs
            .lock()
            .map(|runs| {
                runs.iter()
                    .filter_map(|run| run.lock().ok().map(|guard| guard.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// EventSink Child 模式的转录写句柄。
///
/// `Token` 按行聚合（`\n` 时 flush 完整行）；其余事件逐条成行。
/// 推理流（`reasoning`）同样按缓冲聚合：事件边界可能切在词/数字中间，
/// 逐事件成行会丢空格（"118" + "42" 拼接成 "118 42"）。行内空格原样保留，
/// `\n` 断行转成行尾空格（段落断点保词间空格）。
/// 锁顺序恒为 `reasoning_buf → token_buf → run`，无逆序路径，不会死锁。
#[derive(Clone)]
pub(crate) struct TranscriptWriter {
    run: Arc<Mutex<SubagentRun>>,
    reasoning_buf: Arc<Mutex<String>>,
    token_buf: Arc<Mutex<String>>,
}

/// 连续推理流缓冲阈值：超过即落盘一行，保证长时间纯推理运行中可见。
const REASONING_FLUSH_CHARS: usize = 256;

impl TranscriptWriter {
    pub(crate) fn new(run: Arc<Mutex<SubagentRun>>) -> Self {
        Self {
            run,
            reasoning_buf: Arc::new(Mutex::new(String::new())),
            token_buf: Arc::new(Mutex::new(String::new())),
        }
    }

    /// 追加一整行（落盘前先冲刷推理缓冲尾部，保持事件时序）。
    pub(crate) fn line(&self, text: impl Into<String>) {
        self.flush_reasoning_tail();
        self.push(text.into());
    }

    /// 追加 token 片段；`\n` 时 flush 完整行（未完结行留在缓冲）。
    pub(crate) fn token(&self, text: &str) {
        self.flush_reasoning_tail();
        let mut buf = match self.token_buf.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        buf.push_str(text);
        while let Some(idx) = buf.find('\n') {
            let complete: String = buf.drain(..=idx).collect();
            let line = complete.trim_end_matches(['\n', '\r']);
            if !line.is_empty() {
                self.push(line.to_string());
            }
        }
    }

    /// 追加推理片段：按缓冲聚合，`\n` 断行转为行尾空格；超阈值落盘保持实时可见。
    pub(crate) fn reasoning(&self, text: &str) {
        let mut buf = match self.reasoning_buf.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        buf.push_str(text);
        while let Some(idx) = buf.find('\n') {
            let complete: String = buf.drain(..=idx).collect();
            let line = complete.trim_end_matches(['\n', '\r']).trim_end();
            if !line.is_empty() {
                self.push(format!("thinking: {line} "));
            }
        }
        if buf.len() >= REASONING_FLUSH_CHARS {
            let chunk: String = buf.drain(..).collect();
            let chunk = chunk.trim_end_matches(['\n', '\r']);
            if !chunk.trim().is_empty() {
                self.push(format!("thinking: {chunk}"));
            }
        }
    }

    /// 定稿时 flush 缓冲中剩余的最后一行（token 与推理缓冲都要排空）。
    pub(crate) fn flush(&self) {
        self.flush_reasoning_tail();
        let tail = match self.token_buf.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        let tail = tail.trim_end_matches(['\n', '\r']);
        if !tail.is_empty() {
            self.push(tail.to_string());
        }
    }

    /// 把推理缓冲尾部落盘为一整行（非推理事件到达前调用，保持时序）。
    fn flush_reasoning_tail(&self) {
        let tail = match self.reasoning_buf.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        let tail = tail.trim_end_matches(['\n', '\r']).trim_end();
        if !tail.is_empty() {
            self.push(format!("thinking: {tail}"));
        }
    }

    /// 直接写转录行（不加推理缓冲时序处理；供 reasoning 自身使用）。
    fn push(&self, text: String) {
        if let Ok(mut run) = self.run.lock() {
            push_line(&mut run.lines, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_assigns_unique_ids_and_running_status() {
        let archive = SubagentArchive::default();
        let a = archive.start("alpha");
        let b = archive.start("beta");
        assert_ne!(a, b);
        let snapshot = archive.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.iter().all(|r| r.status == SubagentStatus::Running));
    }

    #[test]
    fn append_line_drops_oldest_beyond_cap() {
        let archive = SubagentArchive::default();
        let id = archive.start("cap");
        for i in 0..(MAX_LINES + 10) {
            archive.append_line(id, format!("line-{i}"));
        }
        let run = &archive.snapshot()[0];
        assert_eq!(run.lines.len(), MAX_LINES);
        assert_eq!(run.lines.first().unwrap(), "line-10");
        assert_eq!(
            run.lines.last().unwrap(),
            &format!("line-{}", MAX_LINES + 9)
        );
    }

    #[test]
    fn finish_updates_status_result_and_error() {
        let archive = SubagentArchive::default();
        let id = archive.start("fin");
        archive.finish(id, SubagentStatus::Completed, Some("done".into()), None);
        let run = &archive.snapshot()[0];
        assert_eq!(run.status, SubagentStatus::Completed);
        assert_eq!(run.result.as_deref(), Some("done"));
        assert!(run.error.is_none());
    }

    #[test]
    fn run_handle_missing_id_returns_none() {
        let archive = SubagentArchive::default();
        assert!(archive.run_handle(999).is_none());
    }

    #[test]
    fn transcript_writer_aggregates_tokens_by_line() {
        let archive = SubagentArchive::default();
        let id = archive.start("tokens");
        let writer = TranscriptWriter::new(archive.run_handle(id).unwrap());
        writer.token("first part");
        writer.token(" of line one\nsecond");
        writer.token(" line\n");
        writer.flush();
        let run = &archive.snapshot()[0];
        assert_eq!(run.lines, vec!["first part of line one", "second line"]);
    }

    #[test]
    fn transcript_writer_reasoning_preserves_stream_spacing() {
        let archive = SubagentArchive::default();
        let id = archive.start("reasoning");
        let writer = TranscriptWriter::new(archive.run_handle(id).unwrap());
        // 事件边界切在词/数字中间（node_ / modules、118 / 42）：不得插入空格；
        // 真正的 \n 断行转成行尾空格（保词间分隔）；无换行尾部在 flush 时落盘。
        writer.reasoning("no node_");
        writer.reasoning("modules, 118");
        writer.reasoning("42 files.\nsecond paragraph\n");
        writer.reasoning("tail without newline");
        writer.flush();
        let run = &archive.snapshot()[0];
        assert_eq!(
            run.lines,
            vec![
                "thinking: no node_modules, 11842 files. ",
                "thinking: second paragraph ",
                "thinking: tail without newline",
            ]
        );
    }

    #[test]
    fn transcript_writer_reasoning_flushes_before_other_events() {
        let archive = SubagentArchive::default();
        let id = archive.start("order");
        let writer = TranscriptWriter::new(archive.run_handle(id).unwrap());
        writer.reasoning("checking dirs");
        // 工具行到达：推理尾部必须先落盘（保持时序）
        writer.line("tool list_dir args: {\"path\":\".\"}");
        writer.line("list_dir => .cyber/");
        writer.token("done\n");
        let run = &archive.snapshot()[0];
        assert_eq!(
            run.lines,
            vec![
                "thinking: checking dirs",
                "tool list_dir args: {\"path\":\".\"}",
                "list_dir => .cyber/",
                "done",
            ]
        );
    }

    #[test]
    fn transcript_writer_reasoning_threshold_flushes_for_live_view() {
        let archive = SubagentArchive::default();
        let id = archive.start("long-reasoning");
        let writer = TranscriptWriter::new(archive.run_handle(id).unwrap());
        writer.reasoning(&"x".repeat(REASONING_FLUSH_CHARS + 10));
        let run = &archive.snapshot()[0];
        assert_eq!(run.lines.len(), 1, "超阈值推理应成行落盘");
        assert!(run.lines[0].starts_with("thinking: xxx"));
    }
}
