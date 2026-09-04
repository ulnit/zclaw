//! Streaming-TTS consumer — port of hermes `gateway/streaming_tts_consumer.py`.
//!
//! Bridges LLM text deltas to a voice-capable platform sink's streaming
//! audio contract so playback begins while the model is still generating.
//!
//! Lifecycle (hermes)::text
//!
//!     let consumer = StreamingTtsConsumer::new(sink, chat_id, format, metadata, active);
//!     // wire consumer.on_delta into the stream-delta callback (sync, non-blocking)
//!     let task = consumer.start();
//!     ... agent runs ...
//!     consumer.finish();                 // signal end-of-text
//!     let success = consumer.wait_complete(Duration::from_secs(10)).await;
//!     if consumer.suppress_whole_file() { /* skip whole-file auto-TTS */ }
//!     consumer.abort("cancelled");       // idempotent cancellation
//!
//! Design (verbatim from hermes):
//! - `on_delta` is synchronous and never blocks the agent thread. It feeds
//!   deltas into a [`SentenceChunker`] and queues completed clauses onto a
//!   bounded (256) channel.
//! - An async drain task ([`StreamingTtsConsumer::run`]) drains the queue,
//!   synthesises each clause via the sink and writes PCM chunks.
//! - Per-turn state is isolated: each consumer owns its chunker, queue and
//!   flags. Concurrent chats cannot cross-contaminate.
//! - On successful completion (all clauses synthesised and written) the
//!   consumer reports `completed=true` so the gateway can suppress the
//!   duplicate whole-file auto-TTS.
//! - On failure before any audible output it reports `completed=false` and
//!   clears `suppress_whole_file` so the gateway falls back to whole-file
//!   TTS.
//! - On failure after partial audible output it reports `completed=false`
//!   but keeps `suppress_whole_file=true` so the gateway does NOT replay
//!   the whole response from the beginning.
//! - Cancellation/abort is idempotent: late chunks are silently dropped.
//!
//! The `SentenceChunker` source is not present in the hermes reference
//! checkout (it imports a `tools.tts_streaming` module that is not
//! published), so the chunker here is a clean-room clause splitter with
//! the same incremental `feed`/`flush` contract.

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const QUEUE_CAPACITY: usize = 256;

/// Declared PCM format for a streaming-TTS session (hermes `AudioFormat`).
///
/// All chunks delivered via [`StreamingTtsSink::write_streaming_tts`] must
/// conform: raw little-endian PCM at the declared sample rate, channels
/// and sample width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    /// Bytes per sample (int16 = 2).
    pub sample_width: u8,
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self {
            sample_rate: 24000,
            channels: 1,
            sample_width: 2,
        }
    }
}

/// A streaming-TTS session handle owned by the consumer (hermes
/// `StreamingTTSHandle`).
#[derive(Debug, Default)]
pub struct StreamingTtsHandle {
    /// True once the first PCM chunk has been written.
    pub audible: bool,
    /// True once the stream has been aborted.
    pub aborted: bool,
}

/// Voice-capable sink contract (hermes adapter streaming-TTS methods).
#[async_trait::async_trait]
pub trait StreamingTtsSink: Send + Sync {
    /// Whether this sink can play streaming PCM for `chat_id`.
    fn supports_streaming_tts(&self, chat_id: &str, format: &AudioFormat) -> bool;

    /// Open a streaming session (hermes `begin_streaming_tts`).
    async fn begin_streaming_tts(
        &self,
        chat_id: &str,
        format: &AudioFormat,
        metadata: &Value,
    ) -> Result<StreamingTtsHandle, String>;

    /// Write one PCM chunk (hermes `write_streaming_tts`).
    async fn write_streaming_tts(
        &self,
        handle: &mut StreamingTtsHandle,
        chunk: &[u8],
    ) -> Result<(), String>;

    /// Close the session cleanly (hermes `finish_streaming_tts`).
    async fn finish_streaming_tts(
        &self,
        handle: &mut StreamingTtsHandle,
        interrupted: bool,
    ) -> Result<(), String>;

    /// Abort the session, swallowing errors — idempotent (hermes
    /// `abort_streaming_tts`).
    async fn abort_streaming_tts(&self, handle: &mut StreamingTtsHandle, error: &str);

    /// Synthesise one clause into PCM chunks (hermes streaming provider
    /// `stream(text)` iteration).
    async fn synthesize_chunks(&self, text: &str) -> Result<Vec<Vec<u8>>, String>;
}

/// Incremental clause splitter with the hermes `feed`/`flush` contract.
///
/// Clean-room implementation (the hermes original is unpublished): text
/// is buffered; a clause is emitted when a sentence-terminating boundary
/// is followed by whitespace or a closing quote/bracket. Over-long
/// buffers are force-split at the last whitespace so synthesis never
/// starves while the model emits one endless clause.
#[derive(Debug)]
pub struct SentenceChunker {
    buffer: String,
    /// Force a split once the buffer exceeds this many characters.
    max_chunk: usize,
}

impl Default for SentenceChunker {
    fn default() -> Self {
        Self::new()
    }
}

impl SentenceChunker {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            max_chunk: 400,
        }
    }

    fn is_boundary(c: char) -> bool {
        matches!(
            c,
            '.' | '!' | '?' | '…' | '。' | '！' | '？' | '；' | ';' | '\n'
        )
    }

    fn is_closer(c: char) -> bool {
        matches!(c, '"' | '\'' | ')' | ']' | '”' | '’' | '」' | '』')
    }

    /// Feed a delta; returns zero or more completed clauses.
    pub fn feed(&mut self, text: &str) -> Vec<String> {
        self.buffer.push_str(text);
        let mut clauses = Vec::new();
        loop {
            match self.find_split() {
                Some(idx) => {
                    let clause = self.buffer[..idx].trim().to_string();
                    self.buffer = self.buffer[idx..].trim_start().to_string();
                    if !clause.is_empty() {
                        clauses.push(clause);
                    }
                }
                None => {
                    if self.buffer.chars().count() > self.max_chunk {
                        if let Some(idx) = self.force_split() {
                            let clause = self.buffer[..idx].trim().to_string();
                            self.buffer = self.buffer[idx..].trim_start().to_string();
                            if !clause.is_empty() {
                                clauses.push(clause);
                            }
                            continue;
                        }
                    }
                    break;
                }
            }
        }
        clauses
    }

    /// Flush the tail at end-of-text; returns the remaining buffer if
    /// non-empty.
    pub fn flush(&mut self) -> Vec<String> {
        let tail = self.buffer.trim().to_string();
        self.buffer.clear();
        if tail.is_empty() {
            Vec::new()
        } else {
            vec![tail]
        }
    }

    /// The not-yet-emitted buffer (hermes `chunker.buf` peek) — the
    /// speak-stream idle flush uses it to decide when a trailing clause
    /// has waited long enough to be spoken.
    pub fn pending(&self) -> &str {
        &self.buffer
    }

    /// Find the byte index just past a boundary (+ optional closers)
    /// that terminates a clause, if any.
    fn find_split(&self) -> Option<usize> {
        let chars: Vec<char> = self.buffer.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            if !Self::is_boundary(c) {
                continue;
            }
            // CJK terminators split immediately (no trailing whitespace
            // in CJK text, and no abbreviation problem); ASCII
            // boundaries need a short-prefix guard so "Dr. Smith" is
            // not split after the abbreviation.
            let cjk_boundary = matches!(c, '\u{3002}' | '\u{ff01}' | '\u{ff1f}' | '\u{ff1b}');
            if !cjk_boundary && i < 3 {
                continue;
            }
            // Optional closing quote/bracket right after the boundary.
            let mut j = i + 1;
            while j < chars.len() && Self::is_closer(chars[j]) {
                j += 1;
            }
            // Boundary at the buffer end: wait for more text (the flush
            // path handles the true end-of-text), unless this is a hard
            // newline which always terminates a clause.
            if j >= chars.len() {
                if c == '\n' {
                    return Some(self.byte_index(i + 1));
                }
                return None;
            }
            if cjk_boundary || chars[j].is_whitespace() || c == '\n' {
                return Some(self.byte_index(j));
            }
        }
        None
    }

    /// Force-split position for an over-long buffer: the byte index of
    /// the last whitespace, or the hard limit when none exists.
    fn force_split(&self) -> Option<usize> {
        let chars: Vec<char> = self.buffer.chars().collect();
        let limit = chars.len().min(self.max_chunk);
        for i in (0..limit).rev() {
            if chars[i].is_whitespace() {
                return Some(self.byte_index(i + 1));
            }
        }
        Some(self.byte_index(limit))
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.buffer.len())
    }
}

enum QueueItem {
    Clause(String),
    Done,
    Abort,
}

#[derive(Debug, Default, Clone)]
struct Outcome {
    started: bool,
    completed: bool,
    partial: bool,
    suppress_whole_file: bool,
}

/// Consumes LLM text deltas and produces streaming PCM audio for a sink
/// (hermes `StreamingTTSConsumer`).
pub struct StreamingTtsConsumer<S: StreamingTtsSink> {
    sink: Arc<S>,
    chat_id: String,
    format: AudioFormat,
    metadata: Value,
    /// True when a streaming provider was resolved (hermes `active`).
    active: bool,
    chunker: Mutex<SentenceChunker>,
    tx: mpsc::Sender<QueueItem>,
    rx: Mutex<Option<mpsc::Receiver<QueueItem>>>,
    aborted: AtomicBool,
    finished: AtomicBool,
    dropped: AtomicBool,
    outcome: Mutex<Outcome>,
    handle: Mutex<Option<StreamingTtsHandle>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl<S: StreamingTtsSink + 'static> StreamingTtsConsumer<S> {
    pub fn new(
        sink: Arc<S>,
        chat_id: impl Into<String>,
        format: AudioFormat,
        metadata: Value,
        active: bool,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        Arc::new(Self {
            sink,
            chat_id: chat_id.into(),
            format,
            metadata,
            active,
            chunker: Mutex::new(SentenceChunker::new()),
            tx,
            rx: Mutex::new(Some(rx)),
            aborted: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            outcome: Mutex::new(Outcome::default()),
            handle: Mutex::new(None),
            task: Mutex::new(None),
        })
    }

    // ------------------------------------------------------------------
    // Public properties (hermes @property surface)
    // ------------------------------------------------------------------

    /// True when this consumer has a usable streaming provider.
    pub fn active(&self) -> bool {
        self.active
    }

    /// True when streaming audio was fully delivered.
    pub fn completed(&self) -> bool {
        self.outcome.lock().unwrap().completed
    }

    /// True when some audio was audible before a failure or drop.
    pub fn partial(&self) -> bool {
        self.outcome.lock().unwrap().partial
    }

    /// True when the sink accepted the streaming session.
    pub fn started(&self) -> bool {
        self.outcome.lock().unwrap().started
    }

    /// True once the first PCM chunk has been written.
    pub fn audible(&self) -> bool {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .map(|h| h.audible)
            .unwrap_or(false)
    }

    /// True when queue saturation dropped at least one clause.
    pub fn dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }

    /// True when the gateway should skip the legacy whole-file TTS
    /// fallback.
    pub fn suppress_whole_file(&self) -> bool {
        self.outcome.lock().unwrap().suppress_whole_file
    }

    /// True once the async drain task has terminated.
    pub fn done(&self) -> bool {
        self.task
            .lock()
            .unwrap()
            .as_ref()
            .map(|t| t.is_finished())
            .unwrap_or(false)
    }

    // ------------------------------------------------------------------
    // Sync callback (agent worker thread)
    // ------------------------------------------------------------------

    /// Receive a text delta from the agent. Non-blocking.
    pub fn on_delta(self: &Arc<Self>, text: &str) {
        if self.aborted.load(Ordering::SeqCst)
            || !self.active
            || self.finished.load(Ordering::SeqCst)
        {
            return;
        }
        let clauses = self.chunker.lock().unwrap().feed(text);
        for clause in clauses {
            if self.tx.try_send(QueueItem::Clause(clause)).is_err() {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Signal end-of-text and flush the chunker tail.
    ///
    /// Enqueues a `Done` sentinel after all flushed clauses so the drain
    /// loop has a deterministic termination signal that cannot race with
    /// a late `on_delta` or be lost when the queue is full.
    pub fn finish(self: &Arc<Self>) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.aborted.load(Ordering::SeqCst) || !self.active {
            return;
        }
        for clause in self.chunker.lock().unwrap().flush() {
            if self.tx.try_send(QueueItem::Clause(clause)).is_err() {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }
        self.enqueue_done();
    }

    /// Enqueue the `Done` sentinel, evicting a queued clause if
    /// necessary — the sentinel is load-bearing and must not be lost
    /// (hermes #60671 hardening).
    fn enqueue_done(&self) {
        if self.tx.try_send(QueueItem::Done).is_ok() {
            return;
        }
        if let Some(rx) = self.rx.lock().unwrap().as_mut() {
            let _ = rx.try_recv();
            self.dropped.store(true, Ordering::SeqCst);
        }
        let _ = self.tx.try_send(QueueItem::Done);
    }

    // ------------------------------------------------------------------
    // Async lifecycle (gateway loop)
    // ------------------------------------------------------------------

    /// Create the async drain task (hermes `start`). Idempotent: a
    /// second call keeps the original task. The task is awaited via
    /// [`StreamingTtsConsumer::wait_complete`].
    pub fn start(self: &Arc<Self>) {
        let mut slot = self.task.lock().unwrap();
        if slot.is_some() {
            return;
        }
        let this = Arc::clone(self);
        *slot = Some(tokio::spawn(async move {
            this.run().await;
        }));
    }

    /// Drain clauses from the queue, synthesise, and write to the sink.
    async fn run(self: &Arc<Self>) {
        if !self.active {
            return;
        }
        let rx = match self.rx.lock().unwrap().take() {
            Some(rx) => rx,
            None => return,
        };
        if !self
            .sink
            .supports_streaming_tts(&self.chat_id, &self.format)
        {
            return;
        }
        let mut handle = match self
            .sink
            .begin_streaming_tts(&self.chat_id, &self.format, &self.metadata)
            .await
        {
            Ok(h) => h,
            Err(_) => return,
        };
        *self.handle.lock().unwrap() = Some(StreamingTtsHandle {
            audible: false,
            aborted: false,
        });
        {
            let mut outcome = self.outcome.lock().unwrap();
            outcome.started = true;
            outcome.suppress_whole_file = false;
        }
        let mut rx = rx;
        loop {
            if self.aborted.load(Ordering::SeqCst) {
                break;
            }
            let item = match rx.recv().await {
                Some(item) => item,
                None => break,
            };
            match item {
                QueueItem::Abort | QueueItem::Done => break,
                QueueItem::Clause(clause) => {
                    if self.aborted.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Err(_exc) = self.synthesise_and_write(&mut handle, &clause).await {
                        let audible = handle.audible;
                        {
                            let mut outcome = self.outcome.lock().unwrap();
                            if audible {
                                outcome.partial = true;
                                outcome.suppress_whole_file = true;
                            } else {
                                outcome.suppress_whole_file = false;
                            }
                            outcome.completed = false;
                        }
                        self.safe_abort(&mut handle, "clause failed").await;
                        *self.handle.lock().unwrap() = Some(handle);
                        return;
                    }
                }
            }
        }

        if !self.aborted.load(Ordering::SeqCst) {
            let finish_failed = self
                .sink
                .finish_streaming_tts(&mut handle, false)
                .await
                .is_err();
            if finish_failed {
                // finish_streaming_tts() raised — never report full
                // completion. Audible → partial + preserve suppression;
                // silent → permit whole-file fallback.
                {
                    let mut outcome = self.outcome.lock().unwrap();
                    if handle.audible {
                        outcome.partial = true;
                        outcome.completed = false;
                        outcome.suppress_whole_file = true;
                    } else {
                        outcome.completed = false;
                        outcome.suppress_whole_file = false;
                    }
                }
                self.safe_abort(&mut handle, "finish_streaming_tts failed")
                    .await;
            } else {
                let mut outcome = self.outcome.lock().unwrap();
                if handle.audible && !self.dropped.load(Ordering::SeqCst) {
                    outcome.completed = true;
                    outcome.suppress_whole_file = true;
                } else if handle.audible && self.dropped.load(Ordering::SeqCst) {
                    outcome.partial = true;
                    outcome.completed = false;
                    outcome.suppress_whole_file = true;
                } else {
                    outcome.completed = false;
                    outcome.suppress_whole_file = false;
                }
            }
        }
        *self.handle.lock().unwrap() = Some(handle);
    }

    /// Synthesise one clause via the sink and write PCM chunks.
    async fn synthesise_and_write(
        self: &Arc<Self>,
        handle: &mut StreamingTtsHandle,
        clause: &str,
    ) -> Result<(), String> {
        if handle.aborted {
            return Ok(());
        }
        let cleaned = strip_markdown_for_tts(clause);
        if cleaned.trim().is_empty() {
            return Ok(());
        }
        let chunks = self.sink.synthesize_chunks(&cleaned).await?;
        for chunk in chunks {
            if self.aborted.load(Ordering::SeqCst) || handle.aborted {
                return Ok(());
            }
            if chunk.is_empty() {
                continue;
            }
            let was_audible = handle.audible;
            self.sink.write_streaming_tts(handle, &chunk).await?;
            if !was_audible {
                handle.audible = true;
                self.outcome.lock().unwrap().suppress_whole_file = true;
            }
        }
        Ok(())
    }

    /// Abort the sink stream, swallowing errors (idempotent).
    async fn safe_abort(&self, handle: &mut StreamingTtsHandle, reason: &str) {
        self.sink.abort_streaming_tts(handle, reason).await;
        handle.aborted = true;
    }

    // ------------------------------------------------------------------
    // Cancellation and completion
    // ------------------------------------------------------------------

    /// Idempotent cancellation from any thread (hermes `abort`).
    pub fn abort(&self, _reason: &str) {
        if self.aborted.swap(true, Ordering::SeqCst) {
            return;
        }
        // Guarantee the Abort sentinel reaches the queue; evict one
        // queued clause if the bounded channel is full (hermes #60671).
        if self.tx.try_send(QueueItem::Abort).is_err() {
            if let Some(rx) = self.rx.lock().unwrap().as_mut() {
                let _ = rx.try_recv();
                self.dropped.store(true, Ordering::SeqCst);
            }
            let _ = self.tx.try_send(QueueItem::Abort);
        }
    }

    /// Wait for the drain task to finish. Returns true only on full
    /// success (hermes `wait_complete`).
    pub async fn wait_complete(self: &Arc<Self>, timeout: Duration) -> bool {
        let handle = self.task.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = tokio::time::timeout(timeout, handle).await;
        }
        self.completed()
    }
}

/// TTS-friendly markdown stripper (hermes `tools.tts_tool
/// ._strip_markdown_for_tts`, which is unpublished in the reference
/// checkout — clean-room minimal version): strips fences, images, link
/// URLs, emphasis markers and list/heading decoration so the voice
/// provider never reads markup aloud.
pub fn strip_markdown_for_tts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let mut cleaned = trimmed
            .trim_start_matches(['#', '>', '-', '*', '+'])
            .trim_start()
            .to_string();
        // Images drop entirely; links keep their label.
        cleaned = strip_pattern(&cleaned, "![");
        cleaned = keep_label(&cleaned, '[');
        cleaned = cleaned.replace(['*', '_', '~', '`'], "");
        cleaned = cleaned.replace('|', " ");
        if !cleaned.is_empty() {
            out.push_str(&cleaned);
            out.push(' ');
        }
    }
    out.trim().to_string()
}

fn strip_pattern(text: &str, marker: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(marker) {
        out.push_str(&rest[..start]);
        let after = &rest[start + marker.len()..];
        // Skip to the closing paren of the image URL.
        rest = match after.find(')') {
            Some(idx) => &after[idx + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn keep_label(text: &str, marker: char) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(marker) {
        out.push_str(&rest[..start]);
        let after = &rest[start + marker.len_utf8()..];
        match after.find(']') {
            Some(end) => {
                out.push_str(&after[..end]);
                let tail = &after[end + 1..];
                rest = if let Some(paren_end) = tail.find(')') {
                    if tail.starts_with('(') {
                        &tail[paren_end + 1..]
                    } else {
                        tail
                    }
                } else {
                    tail
                };
            }
            None => {
                out.push(marker);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Test sink recording every write; failure points injectable.
    struct MockSink {
        supported: bool,
        begin_fails: bool,
        /// Fail synthesis on the Nth clause (1-based), 0 = never.
        fail_synthesis_on: AtomicUsize,
        /// Fail finish.
        fail_finish: bool,
        /// Fail the first write after N successful writes (0 = never).
        fail_write_after: AtomicUsize,
        writes: Mutex<Vec<Vec<u8>>>,
        clauses: Mutex<Vec<String>>,
        finished_flag: AtomicBool,
        aborted_flag: AtomicBool,
    }

    impl MockSink {
        fn new() -> Self {
            Self {
                supported: true,
                begin_fails: false,
                fail_synthesis_on: AtomicUsize::new(0),
                fail_finish: false,
                fail_write_after: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
                clauses: Mutex::new(Vec::new()),
                finished_flag: AtomicBool::new(false),
                aborted_flag: AtomicBool::new(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl StreamingTtsSink for MockSink {
        fn supports_streaming_tts(&self, _chat_id: &str, _format: &AudioFormat) -> bool {
            self.supported
        }

        async fn begin_streaming_tts(
            &self,
            _chat_id: &str,
            _format: &AudioFormat,
            _metadata: &Value,
        ) -> Result<StreamingTtsHandle, String> {
            if self.begin_fails {
                return Err("begin failed".into());
            }
            Ok(StreamingTtsHandle::default())
        }

        async fn write_streaming_tts(
            &self,
            _handle: &mut StreamingTtsHandle,
            chunk: &[u8],
        ) -> Result<(), String> {
            let limit = self.fail_write_after.load(Ordering::SeqCst);
            let mut writes = self.writes.lock().unwrap();
            if limit > 0 && writes.len() >= limit {
                return Err("write failed".into());
            }
            writes.push(chunk.to_vec());
            Ok(())
        }

        async fn finish_streaming_tts(
            &self,
            _handle: &mut StreamingTtsHandle,
            _interrupted: bool,
        ) -> Result<(), String> {
            if self.fail_finish {
                return Err("finish failed".into());
            }
            self.finished_flag.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn abort_streaming_tts(&self, _handle: &mut StreamingTtsHandle, _error: &str) {
            self.aborted_flag.store(true, Ordering::SeqCst);
        }

        async fn synthesize_chunks(&self, text: &str) -> Result<Vec<Vec<u8>>, String> {
            let n = self.clauses.lock().unwrap().len() + 1;
            self.clauses.lock().unwrap().push(text.to_string());
            let target = self.fail_synthesis_on.load(Ordering::SeqCst);
            if target > 0 && n == target {
                return Err("synthesis failed".into());
            }
            Ok(vec![text.as_bytes().to_vec()])
        }
    }

    #[test]
    fn test_chunker_basic_sentences() {
        let mut chunker = SentenceChunker::new();
        let mut out = chunker.feed("Hello there. How are you? I am ");
        assert_eq!(out, vec!["Hello there.", "How are you?"]);
        out = chunker.feed("fine!");
        assert!(out.is_empty());
        out = chunker.feed(" Thanks.");
        // A boundary at the very end of the buffer is held until more
        // text arrives or flush() signals end-of-text.
        assert_eq!(out, vec!["I am fine!"]);
        assert_eq!(chunker.flush(), vec!["Thanks."]);
    }

    #[test]
    fn test_chunker_cjk_boundaries() {
        let mut chunker = SentenceChunker::new();
        let out = chunker.feed("你好。今天天气不错！走吧？");
        // The final clause is held until flush (streaming contract).
        assert_eq!(out, vec!["你好。", "今天天气不错！"]);
        assert_eq!(chunker.flush(), vec!["走吧？"]);
    }

    #[test]
    fn test_chunker_holds_partial_tail() {
        let mut chunker = SentenceChunker::new();
        assert!(chunker.feed("Wait for it").is_empty());
        let tail = chunker.flush();
        assert_eq!(tail, vec!["Wait for it"]);
        assert!(chunker.flush().is_empty());
    }

    #[test]
    fn test_chunker_closing_quote_after_boundary() {
        let mut chunker = SentenceChunker::new();
        let out = chunker.feed("He said \"stop.\" Then he left. ");
        assert_eq!(out, vec!["He said \"stop.\"", "Then he left."]);
    }

    #[test]
    fn test_chunker_newline_terminates() {
        let mut chunker = SentenceChunker::new();
        let out = chunker.feed("line one\nline two\n");
        assert_eq!(out, vec!["line one", "line two"]);
    }

    #[test]
    fn test_chunker_abbreviation_no_split() {
        // Boundary not followed by whitespace stays buffered.
        let mut chunker = SentenceChunker::new();
        let out = chunker.feed("Dr. Smith is here. ");
        assert_eq!(out, vec!["Dr. Smith is here."]);
    }

    #[test]
    fn test_chunker_force_split_long_buffer() {
        let mut chunker = SentenceChunker {
            buffer: String::new(),
            max_chunk: 20,
        };
        let text = "word ".repeat(20); // 100 chars, no boundary
        let out = chunker.feed(&text);
        assert!(!out.is_empty());
        assert!(out.iter().all(|c| c.chars().count() <= 20));
        let tail = chunker.flush();
        let total: usize = out
            .iter()
            .chain(tail.iter())
            .map(|c| c.chars().count())
            .sum();
        // All 80 non-whitespace chars survive; inter-word spaces may be
        // trimmed at split boundaries.
        assert!(total >= 80, "total {total}");
    }

    #[test]
    fn test_strip_markdown_for_tts() {
        let md = "# Title\n\nSome **bold** and _italics_ with a [link](https://x.y) and ![img](https://i.png).\n\n```\ncode();\n```\n> quoted";
        let out = strip_markdown_for_tts(md);
        assert!(out.contains("Some bold and italics with a link and ."));
        assert!(!out.contains("code()"));
        assert!(out.contains("quoted"));
        assert!(!out.contains("https://"));
    }

    #[tokio::test]
    async fn test_consumer_full_success() {
        let sink = Arc::new(MockSink::new());
        let consumer = StreamingTtsConsumer::new(
            sink.clone(),
            "chat-1",
            AudioFormat::default(),
            Value::Null,
            true,
        );
        consumer.start();
        consumer.on_delta("Hello there. ");
        consumer.on_delta("All done now!");
        consumer.finish();
        let ok = consumer.wait_complete(Duration::from_secs(5)).await;
        assert!(ok);
        assert!(consumer.completed());
        assert!(consumer.suppress_whole_file());
        assert!(consumer.started());
        assert!(!consumer.partial());
        assert!(!consumer.dropped());
        assert_eq!(sink.clauses.lock().unwrap().len(), 2);
        assert!(sink.finished_flag.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_consumer_inactive_passthrough() {
        let sink = Arc::new(MockSink::new());
        let consumer = StreamingTtsConsumer::new(
            sink.clone(),
            "chat-1",
            AudioFormat::default(),
            Value::Null,
            false,
        );
        consumer.on_delta("text. ");
        consumer.finish();
        assert!(!consumer.wait_complete(Duration::from_millis(100)).await);
        assert!(!consumer.suppress_whole_file());
        assert!(sink.clauses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_consumer_unsupported_sink() {
        let mut sink = MockSink::new();
        sink.supported = false;
        let sink = Arc::new(sink);
        let consumer = StreamingTtsConsumer::new(
            sink.clone(),
            "chat-1",
            AudioFormat::default(),
            Value::Null,
            true,
        );
        consumer.start();
        consumer.on_delta("Hello. ");
        consumer.finish();
        assert!(!consumer.wait_complete(Duration::from_secs(5)).await);
        assert!(!consumer.started());
        assert!(!consumer.suppress_whole_file());
    }

    #[tokio::test]
    async fn test_consumer_begin_failure_allows_fallback() {
        let mut sink = MockSink::new();
        sink.begin_fails = true;
        let sink = Arc::new(sink);
        let consumer = StreamingTtsConsumer::new(
            sink.clone(),
            "chat-1",
            AudioFormat::default(),
            Value::Null,
            true,
        );
        consumer.start();
        consumer.on_delta("Hello. ");
        consumer.finish();
        assert!(!consumer.wait_complete(Duration::from_secs(5)).await);
        // No audible output → whole-file fallback permitted.
        assert!(!consumer.suppress_whole_file());
    }

    #[tokio::test]
    async fn test_consumer_synthesis_failure_before_audible() {
        let sink = Arc::new(MockSink::new());
        sink.fail_synthesis_on.store(1, Ordering::SeqCst);
        let consumer = StreamingTtsConsumer::new(
            sink.clone(),
            "chat-1",
            AudioFormat::default(),
            Value::Null,
            true,
        );
        consumer.start();
        consumer.on_delta("Hello. ");
        consumer.finish();
        assert!(!consumer.wait_complete(Duration::from_secs(5)).await);
        assert!(!consumer.completed());
        assert!(!consumer.partial());
        assert!(!consumer.suppress_whole_file());
        assert!(sink.aborted_flag.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_consumer_write_failure_after_audible_keeps_suppression() {
        let sink = Arc::new(MockSink::new());
        sink.fail_write_after.store(1, Ordering::SeqCst);
        let consumer = StreamingTtsConsumer::new(
            sink.clone(),
            "chat-1",
            AudioFormat::default(),
            Value::Null,
            true,
        );
        consumer.start();
        consumer.on_delta("First clause. ");
        consumer.on_delta("Second clause. ");
        consumer.finish();
        assert!(!consumer.wait_complete(Duration::from_secs(5)).await);
        // Audible before the failure → partial, suppression preserved.
        assert!(consumer.partial());
        assert!(!consumer.completed());
        assert!(consumer.suppress_whole_file());
    }

    #[tokio::test]
    async fn test_consumer_finish_failure_after_audible() {
        let mut sink = MockSink::new();
        sink.fail_finish = true;
        let sink = Arc::new(sink);
        let consumer = StreamingTtsConsumer::new(
            sink.clone(),
            "chat-1",
            AudioFormat::default(),
            Value::Null,
            true,
        );
        consumer.start();
        consumer.on_delta("Hello there. ");
        consumer.finish();
        assert!(!consumer.wait_complete(Duration::from_secs(5)).await);
        assert!(consumer.partial());
        assert!(consumer.suppress_whole_file());
    }

    #[tokio::test]
    async fn test_consumer_abort_idempotent() {
        let sink = Arc::new(MockSink::new());
        let consumer = StreamingTtsConsumer::new(
            sink.clone(),
            "chat-1",
            AudioFormat::default(),
            Value::Null,
            true,
        );
        consumer.start();
        consumer.on_delta("Hello there. ");
        consumer.abort("cancelled");
        consumer.abort("again");
        assert!(!consumer.wait_complete(Duration::from_secs(5)).await);
        assert!(!consumer.completed());
        // Late deltas are dropped silently after abort.
        consumer.on_delta("More text. ");
        assert!(!consumer.suppress_whole_file());
    }

    #[tokio::test]
    async fn test_consumer_empty_clauses_skipped() {
        let sink = Arc::new(MockSink::new());
        let consumer = StreamingTtsConsumer::new(
            sink.clone(),
            "chat-1",
            AudioFormat::default(),
            Value::Null,
            true,
        );
        consumer.start();
        consumer.on_delta("![img](https://example.com/i.png) ");
        consumer.finish();
        assert!(!consumer.wait_complete(Duration::from_secs(5)).await);
        // Image-only clause strips to nothing → nothing audible → no
        // suppression, whole-file fallback permitted.
        assert!(!consumer.completed());
        assert!(!consumer.suppress_whole_file());
        assert!(sink.clauses.lock().unwrap().is_empty());
    }
}
