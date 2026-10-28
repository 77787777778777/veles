# jsonkit

Five interlocking pieces. `tests/visible.rs` covers part of this; the
spec below is the authority for everything, including cases the visible
tests do not reach.

## 1. `parse(&str) -> Result<Json, JsonError>`
RFC 8259. Objects, arrays, strings, numbers, `true`/`false`/`null`.
- String escapes: `\" \\ \/ \b \f \n \r \t` and `\uXXXX`, including
  surrogate PAIRS (`😀` is one emoji).
- Numbers: `-`, integers, fractions, exponents (`1e3`, `-2.5E-2`).
  Leading zeros (`01`) and a bare `.5` or `5.` are errors.
- Duplicate object keys: the LAST wins.
- Rejects trailing commas, comments, single quotes, `NaN`, `Infinity`.
- Trailing non-whitespace after the value is an error.
- Errors carry the 0-indexed byte position where the problem starts.

## 2. `serialize(&Json) -> String`
Compact (no spaces). Object keys sorted (`Json::Obj` is a `BTreeMap`,
so iteration order is already sorted). Escapes `"` `\` and control
characters below 0x20 using the short forms where they exist and
`\u00XX` otherwise. Non-ASCII is emitted literally, not escaped.
Numbers round-trip: integral values print without a trailing `.0`.

## 3. `pointer(&Json, &str) -> Option<&Json>`
RFC 6901. `""` is the whole document. `/foo/0/bar` walks in. The
escapes are `~1` for `/` and `~0` for `~`, decoded in that order.
Array indices are decimal with no leading zeros; `-` is not a valid
index for lookup. A missing key or out-of-range index is `None`.

## 4. `patch(&Json, &Json) -> Result<Json, JsonError>`
RFC 6902. `ops` is an array of objects. Support `add`, `remove`,
`replace`, `move`, `copy`, `test`. Applied in order to a COPY; any
failing op fails the whole patch and the input is unchanged.
- `add` to an array index inserts; `-` appends. `add` to an object
  key replaces if present.
- `remove`/`replace` require the target to exist.
- `test` compares by deep equality and fails the patch when unequal.
- `move` must not move a location into its own child.

## 5. `merge_patch(&Json, &Json) -> Json`
RFC 7386. Recursive merge. A `null` value in the patch DELETES the key.
A non-object patch replaces the target outright.
