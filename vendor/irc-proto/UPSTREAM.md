# irc-proto: `LineCodec::decode` has no maximum line length

This directory is `irc-proto` 1.1.0 with a single behavioural change and
tests for it. It exists because the bug below cannot be fixed from
outside the crate.

**Not yet reported upstream** — filing an issue is an outward-facing
action and is the repository owner's to take. The text below is ready to
paste at <https://github.com/aatxe/irc>.

---

## Report

**Title:** `LineCodec::decode` buffers an unterminated line without limit

**Affects:** `irc-proto` (all versions through 1.1.0), and therefore
every user of `irc`.

### What happens

`irc-proto/src/line.rs`:

```rust
fn decode(&mut self, src: &mut BytesMut) -> error::Result<Option<String>> {
    if let Some(offset) = src[self.next_index..].iter().position(|b| *b == b'\n') {
        ...
    } else {
        // Set the search start index to the current length since we know that none of the
        // characters we've already looked at are newlines.
        self.next_index = src.len();
        Ok(None)
    }
}
```

There is no ceiling on `src`. A server that opens a line and never sends
`\n` makes `Framed` read forever, and `BytesMut` grows until allocation
fails. RFC 1459 caps a line at 512 bytes and IRCv3 message tags add at
most 8191 more, so any line past a few tens of kilobytes is already
protocol-invalid — the codec just never says so.

### Why a downstream user cannot fix it

The buffer is private to the codec, and `Client` owns its transport:

- `Client::new(path)` and `Client::from_config(config)` are the only
  constructors, and both open the connection themselves. There is no way
  to supply a pre-wrapped stream.
- `Client::stream()` yields `ClientStream`, which produces only
  *complete* `Message`s. A partial line is never observable.

So a consumer can neither see the growth nor bound it. A read timeout is
not equivalent: bandwidth × timeout is still unbounded memory.

### Precedent

WeeChat shipped this same bug from 0.3.3 (2010) to 4.9.1 (2026) and
fixed it as **WSA-2026-3**, "missing limit on unterminated IRC messages
causes memory exhaustion". It is a known and durable shape for an IRC
client, not a theoretical one.

### Suggested fix

A constant and four lines in the `else` branch:

```rust
const MAX_LINE_BYTES: usize = 16 * 1024;

// ... in the else branch, before updating next_index:
if src.len() > MAX_LINE_BYTES {
    return Err(error::ProtocolError::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("IRC line exceeded {MAX_LINE_BYTES} bytes with no terminator"),
    )));
}
```

Erroring rather than truncating is deliberate: a stream that has produced
an over-length line has either desynced or is hostile, and continuing to
parse it as if it were framed correctly is worse than dropping the
connection.

Making the cap a field on `LineCodec` with a `with_max_line_length`
constructor would also work and would let a caller relax it; the constant
is the smaller change.

### Tests

Three, in `veles_line_cap_tests` in this copy of `line.rs`:

- an unterminated line past the cap errors, and a partial line under it
  still returns `Ok(None)`;
- a 8703-byte terminated line (512 + the IRCv3 tag maximum) still
  decodes, so the cap does not break legitimate traffic;
- eight 4 KiB lines totalling 32 KiB all decode, so the cap is per line
  and not per connection.

---

## Maintaining this copy

Re-check on every `irc` / `irc-proto` bump. If upstream takes the fix,
delete this directory and the `[patch.crates-io]` block in the workspace
`Cargo.toml`. The only difference from the published crate is
`MAX_LINE_BYTES` and its check, plus the test module; everything else is
byte-identical to `irc-proto` 1.1.0 so the diff stays reviewable.
