//! Tail-safe SSE decoding for provider byte streams.
//!
//! `eventsource-stream` (the crate this replaces) drops the FINAL event
//! when the connection dies without the blank line that terminates it:
//! its own test asserts `"data: Hello, world!\n"` decodes to NOTHING.
//! When a provider edge flushes the last delta and closes before the
//! terminator — Aliyun compatible-mode does exactly that under load —
//! every response loses its tail, mid-word, in the transcript (two
//! visible clips on 2026-09-08: "apply the three change" and
//! "carves a wr...").
//!
//! This decoder parses the same wire (data/event/id/retry fields,
//! comments, blank-line-terminated events) and, at end-of-stream,
//! promotes whatever unterminated residue the connection died with to
//! one final event instead of dropping it.
//!
//! The provider wires in `providers.rs` and `responses.rs` only ever
//! read `data`, so the other SSE fields are consumed and discarded here.

use std::fmt;
use std::pin::Pin;
use std::str;
use std::task::{Context, Poll};

use futures::Stream;

/// One decoded event. Only `data` is kept — see the module docs.
#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub data: String,
}

/// The underlying byte stream failed. (Invalid UTF-8 bytes are
/// substituted with U+FFFD and decoded, never fatal — corrupt bytes
/// cost a character, not the rest of the stream.)
#[derive(Debug, PartialEq, Eq)]
pub enum SseError<E> {
    Transport(E),
}

impl<E: fmt::Display> fmt::Display for SseError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SseError::Transport(e) => e.fmt(f),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for SseError<E> {}

/// Wrap a byte stream and yield its SSE events — including the last
/// one, even when the stream ends without the blank line an SSE event
/// is supposed to carry.
pub(crate) struct TailSafeSse<S> {
    inner: S,
    /// Decoded text of the in-flight event (consumed through its
    /// terminating blank line; the next poll refills it).
    buffer: String,
    /// Undecodable bytes held across chunk boundaries: the head of a
    /// UTF-8 sequence split mid-character.
    pending: Vec<u8>,
    done: bool,
}

pub(crate) fn tail_safe<S>(inner: S) -> TailSafeSse<S> {
    TailSafeSse {
        inner,
        buffer: String::new(),
        pending: Vec::new(),
        done: false,
    }
}

impl<S, B, E> Stream for TailSafeSse<S>
where
    S: Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
{
    type Item = Result<SseEvent, SseError<E>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            // A complete event is already buffered (or the previous
            // event's boundary left a blank consumer behind).
            if let Some(data) = take_event(&mut this.buffer) {
                return Poll::Ready(Some(Ok(SseEvent { data })));
            }
            if this.done {
                // The connection ended. Whatever is left did not get
                // its blank line — decode it and promote it instead of
                // dropping the tail of the reply.
                let mut rest = std::mem::take(&mut this.buffer);
                if !this.pending.is_empty() {
                    rest.push_str(&String::from_utf8_lossy(&this.pending));
                    this.pending.clear();
                }
                return match extract_data(&rest) {
                    Some(data) => Poll::Ready(Some(Ok(SseEvent { data }))),
                    // Empty residue (clean end) or nothing but id/retry
                    // fields and comments: nothing more to say.
                    None => Poll::Ready(None),
                };
            }
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.pending.extend_from_slice(bytes.as_ref());
                    let decoded = decode_progress(&mut this.pending);
                    this.buffer.push_str(&decoded);
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(SseError::Transport(e))));
                }
                Poll::Ready(None) => this.done = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Decode as much of `pending` as UTF-8 allows, leaving an incomplete
/// trailing sequence in place. A genuinely invalid byte costs one
/// replacement character, not the rest of the stream.
fn decode_progress(pending: &mut Vec<u8>) -> String {
    let mut out = String::new();
    loop {
        if pending.is_empty() {
            return out;
        }
        match str::from_utf8(pending) {
            Ok(_) => {
                let rest = std::mem::take(pending);
                out.push_str(&String::from_utf8(rest).unwrap());
                return out;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    let head: Vec<u8> = pending.drain(..valid).collect();
                    out.push_str(&String::from_utf8(head).unwrap());
                } else {
                    match e.error_len() {
                        // The sequence was cut by a chunk boundary;
                        // hold everything and wait for the next chunk.
                        None => return out,
                        // A real bad byte: substitute and move on.
                        Some(_) => {
                            out.push('\u{FFFD}');
                            pending.remove(0);
                        }
                    }
                }
            }
        }
    }
}

/// Pull one complete event out of `buf`: its data fields, joined per
/// the SSE spec, or None when `buf` holds no complete event yet.
/// Consumes through the terminating blank line. An event with no
/// `data:` field is swallowed (identity with eventsource-stream's
/// empty-data dispatch refusal).
fn take_event(buf: &mut String) -> Option<String> {
    let (end, consume) = match (buf.find("\r\n\r\n"), buf.find("\n\n")) {
        (Some(i), Some(j)) => {
            if i + 2 <= j {
                (i + 2, i + 4)
            } else {
                (j + 1, j + 2)
            }
        }
        (Some(i), None) => (i + 2, i + 4),
        (None, Some(j)) => (j + 1, j + 2),
        (None, None) => return None,
    };
    let event_text: String = buf.drain(..end).collect();
    buf.drain(..consume - end);
    extract_data(&event_text)
}

/// The data payload of an event's text: `data:` lines joined with
/// newlines, the trailing one swallowed per spec. None when the event
/// carries no data field.
fn extract_data(event_text: &str) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    for line in event_text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        lines.push(rest.strip_prefix(' ').unwrap_or(rest));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    async fn collect<'a>(
        chunks: Vec<Result<&'a str, &'a str>>,
    ) -> Vec<Result<SseEvent, SseError<&'a str>>> {
        let s = tail_safe(futures::stream::iter(chunks));
        s.collect().await
    }

    /// THE regression: the old parser's own documented behavior was
    /// vec![] for this input. The tail is the whole point.
    #[tokio::test]
    async fn an_event_terminated_only_by_eof_is_kept() {
        let evs = collect(vec![Ok("data: Hello, world!\n")]).await;
        assert_eq!(
            evs,
            vec![Ok(SseEvent {
                data: "Hello, world!".into()
            })]
        );
    }

    /// No newline AT ALL — the connection died mid-line.
    #[tokio::test]
    async fn an_event_terminated_only_by_eof_without_newline_is_kept() {
        let evs = collect(vec![Ok("data: tail")]).await;
        assert_eq!(
            evs,
            vec![Ok(SseEvent {
                data: "tail".into()
            })]
        );
    }

    /// The final `data: [DONE]` without its blank line must still stop
    /// the loop — and the event before it reads normally.
    #[tokio::test]
    async fn the_done_sentinel_hung_at_eof_is_kept() {
        let evs = collect(vec![Ok("data: {\"a\":1}\n\ndata: [DONE]\n")]).await;
        assert_eq!(
            evs,
            vec![
                Ok(SseEvent {
                    data: "{\"a\":1}".into()
                }),
                Ok(SseEvent {
                    data: "[DONE]".into()
                }),
            ]
        );
    }

    /// Multi-line `data:` fields join with newlines (spec behavior the
    /// provider loop relies on every chunk).
    #[tokio::test]
    async fn multi_line_data_joins() {
        let evs = collect(vec![Ok("data: A\ndata: B\n\n")]).await;
        assert_eq!(
            evs,
            vec![Ok(SseEvent {
                data: "A\nB".into()
            })]
        );
    }

    /// CRLF line endings.
    #[tokio::test]
    async fn crlf_streams_decode() {
        let evs = collect(vec![Ok("data: A\r\n\r\ndata: B\r\n")]).await;
        assert_eq!(
            evs,
            vec![
                Ok(SseEvent { data: "A".into() }),
                Ok(SseEvent { data: "B".into() }),
            ]
        );
    }

    /// Comments and the id/event/retry fields are consumed, not
    /// mistaken for data.
    #[tokio::test]
    async fn comments_and_other_fields_are_ignored() {
        let evs = collect(vec![Ok(": hi\nevent: add\nid: 5\nretry: 9\ndata: x\n\n")]).await;
        assert_eq!(evs, vec![Ok(SseEvent { data: "x".into() })]);
    }

    /// A data-less event (heartbeat) is swallowed.
    #[tokio::test]
    async fn events_without_data_are_swallowed() {
        let evs = collect(vec![Ok("event: ping\n\n")]).await;
        assert_eq!(evs, vec![]);
    }

    /// What EOF sees differs from what was in transit: after a clean
    /// blank-line-terminated event, leftover plain text (id fields,
    /// comments) at EOF yields nothing.
    #[tokio::test]
    async fn non_data_residue_at_eof_yields_nothing() {
        let evs = collect(vec![Ok("data: x\n\n: goodbye\n")]).await;
        assert_eq!(evs, vec![Ok(SseEvent { data: "x".into() })]);
    }

    /// A UTF-8 character genuinely split across two network chunks
    /// reassembles (this one feeds raw bytes mid-sequence — a &str
    /// chunk could never split a character).
    #[tokio::test]
    async fn a_split_multibyte_character_reassembles() {
        let s = tail_safe(futures::stream::iter(vec![
            Ok::<&[u8], &[u8]>(&b"data: \xF0"[..]),
            Ok(&b"\x9F\xA5\x90 rest\n\n"[..]),
        ]));
        let evs: Vec<_> = s.collect().await;
        assert_eq!(
            evs,
            vec![Ok(SseEvent {
                data: "\u{1F950} rest".into()
            })]
        );
    }

    /// A genuinely invalid byte swaps for U+FFFD and the rest survives.
    #[tokio::test]
    async fn a_bad_byte_costs_a_character_not_the_stream() {
        let s = tail_safe(futures::stream::iter(vec![Ok::<&[u8], &[u8]>(
            &b"data: a\xffb\n\n"[..],
        )]));
        let evs: Vec<_> = s.collect().await;
        assert_eq!(
            evs,
            vec![Ok(SseEvent {
                data: "a\u{FFFD}b".into()
            })]
        );
    }

    /// Transport errors pass through.
    #[tokio::test]
    async fn transport_errors_pass_through() {
        let s = tail_safe(futures::stream::iter(vec![
            Ok::<&str, &str>("data: x\n\n"),
            Err("boom"),
        ]));
        let evs: Vec<_> = s.collect().await;
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[1], Err(SseError::Transport("boom"))));
    }

    /// The empty stream yields nothing and terminates.
    #[tokio::test]
    async fn an_empty_stream_yields_nothing() {
        let evs = collect(vec![]).await;
        assert_eq!(evs, vec![]);
    }
}
