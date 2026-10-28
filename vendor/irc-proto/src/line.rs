//! Implementation of line-delimiting codec for Tokio.

use std::io;

use bytes::BytesMut;
#[cfg(feature = "encoding")]
use encoding::label::encoding_from_whatwg_label;
#[cfg(feature = "encoding")]
use encoding::{DecoderTrap, EncoderTrap, EncodingRef};
use tokio_util::codec::{Decoder, Encoder};

use crate::error;

// ── veles patch: bound an unterminated line ──────────────────────────
//
// UPSTREAM BUG. `decode` below reads until it finds a `\n` and has no
// ceiling, so a server that opens a line and never terminates it grows
// `src` until the process dies. Nothing above this layer can bound it:
// `Client::from_config` owns the socket and `ClientStream` only ever
// yields COMPLETE messages, so the partial line is never visible to a
// consumer of the crate. The cap has to live here.
//
// This is WeeChat WSA-2026-3 — "missing limit on unterminated IRC
// messages causes memory exhaustion", affecting 0.3.3 through 4.9.1 —
// in a different codebase. It is worth being precise that this is not
// speculative: it is the same bug, found by reading their advisory
// register and then checking whether we had it.
//
// RFC 1459 caps a line at 512 bytes including CRLF; IRCv3 message tags
// add up to 8191 more. 16 KiB is comfortably past anything legitimate,
// so exceeding it means the peer is hostile or the stream has desynced.
// Erroring (rather than truncating) drops the connection, which is the
// correct response to both.
//
// Reported upstream: see vendor/irc-proto/UPSTREAM.md.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// A line-based codec parameterized by an encoding.
pub struct LineCodec {
    #[cfg(feature = "encoding")]
    encoding: EncodingRef,
    next_index: usize,
}

impl LineCodec {
    /// Creates a new instance of LineCodec from the specified encoding.
    pub fn new(#[allow(unused_variables)] label: &str) -> error::Result<LineCodec> {
        Ok(LineCodec {
            #[cfg(feature = "encoding")]
            encoding: match encoding_from_whatwg_label(label) {
                Some(x) => x,
                None => {
                    return Err(error::ProtocolError::Io(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        &format!("Attempted to use unknown codec {}.", label)[..],
                    )));
                }
            },
            next_index: 0,
        })
    }
}

impl Decoder for LineCodec {
    type Item = String;
    type Error = error::ProtocolError;

    fn decode(&mut self, src: &mut BytesMut) -> error::Result<Option<String>> {
        if let Some(offset) = src[self.next_index..].iter().position(|b| *b == b'\n') {
            // Remove the next frame from the buffer.
            let line = src.split_to(self.next_index + offset + 1);

            // Set the search start index back to 0 since we found a newline.
            self.next_index = 0;

            #[cfg(feature = "encoding")]
            {
                // Decode the line using the codec's encoding.
                match self.encoding.decode(line.as_ref(), DecoderTrap::Replace) {
                    Ok(data) => Ok(Some(data)),
                    Err(data) => Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        &format!("Failed to decode {} as {}.", data, self.encoding.name())[..],
                    )
                    .into()),
                }
            }

            #[cfg(not(feature = "encoding"))]
            {
                match String::from_utf8(line.to_vec()) {
                    Ok(data) => Ok(Some(data)),
                    Err(data) => Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        &format!("Failed to decode {} as UTF-8.", data)[..],
                    )
                    .into()),
                }
            }
        } else {
            // veles patch: refuse an unterminated line past the cap
            // rather than buffering it forever. See MAX_LINE_BYTES.
            if src.len() > MAX_LINE_BYTES {
                return Err(error::ProtocolError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "IRC line exceeded {} bytes with no terminator — refusing to buffer more",
                        MAX_LINE_BYTES
                    ),
                )));
            }
            // Set the search start index to the current length since we know that none of the
            // characters we've already looked at are newlines.
            self.next_index = src.len();
            Ok(None)
        }
    }
}

impl Encoder<String> for LineCodec {
    type Error = error::ProtocolError;

    fn encode(&mut self, msg: String, dst: &mut BytesMut) -> error::Result<()> {
        #[cfg(feature = "encoding")]
        {
            // Encode the message using the codec's encoding.
            let data: error::Result<Vec<u8>> = self
                .encoding
                .encode(&msg, EncoderTrap::Replace)
                .map_err(|data| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        &format!("Failed to encode {} as {}.", data, self.encoding.name())[..],
                    )
                    .into()
                });
            // Write the encoded message to the output buffer.
            dst.extend(&data?);
        }

        #[cfg(not(feature = "encoding"))]
        {
            dst.extend(msg.into_bytes());
        }

        Ok(())
    }
}

#[cfg(test)]
mod veles_line_cap_tests {
    use super::*;
    use bytes::BufMut;

    /// The bug, stated as a test: bytes with no terminator must not be
    /// buffered without limit. Before the cap this loop ran until the
    /// allocator gave up.
    #[test]
    fn an_unterminated_line_is_refused_rather_than_buffered() {
        let mut codec = LineCodec::new("utf-8").expect("codec");
        let mut buf = BytesMut::new();

        // Well under the cap: still waiting, which is correct — a
        // partial line is normal, it is an ENDLESS one that is not.
        buf.put_slice(&vec![b'a'; 1024]);
        assert!(codec.decode(&mut buf).expect("under the cap is fine").is_none());

        // Past it: an error, which drops the connection.
        buf.put_slice(&vec![b'a'; MAX_LINE_BYTES]);
        let err = codec.decode(&mut buf).expect_err("past the cap must error");

        // NOTE for anyone reading the logs: `ProtocolError::Io`'s own
        // Display is the fixed string "an io error occurred" and the
        // reason hangs off it as a `#[source]`. So assert on the source,
        // and — more importantly — anything that reports this error to a
        // human has to walk the chain or it prints nothing useful. This
        // test found that; client.rs now uses `.context()` for it.
        let reason = std::error::Error::source(&err)
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert!(
            reason.contains("no terminator"),
            "the source must say what happened, got: {}",
            reason
        );
    }

    /// The cap must not break a legitimate long line. IRCv3 message
    /// tags allow 8191 bytes on top of the 512-byte RFC 1459 line, and
    /// a client that rejects those is broken in a subtler way than one
    /// that buffers forever.
    #[test]
    fn a_long_but_terminated_line_still_decodes() {
        let mut codec = LineCodec::new("utf-8").expect("codec");
        let mut buf = BytesMut::new();
        let long = "a".repeat(8703); // 512 + 8191, the IRCv3 maximum
        buf.put_slice(long.as_bytes());
        buf.put_slice(b"\r\n");
        let got = codec.decode(&mut buf).expect("decodes").expect("a line");
        assert_eq!(got.trim_end(), long);
    }

    /// Several lines in one read still come out one at a time, with the
    /// cap in place — the check must look at the UNCONSUMED remainder,
    /// not at everything that ever arrived.
    #[test]
    fn the_cap_applies_per_line_not_per_connection() {
        let mut codec = LineCodec::new("utf-8").expect("codec");
        let mut buf = BytesMut::new();
        for _ in 0..8 {
            buf.put_slice(&vec![b'x'; 4096]);
            buf.put_slice(b"\r\n");
        }
        // 32 KiB total, twice the cap, but no single line is over it.
        for i in 0..8 {
            let got = codec.decode(&mut buf).expect("decodes").expect("a line");
            assert_eq!(got.trim_end().len(), 4096, "line {i}");
        }
    }
}
