# The glyph table

`unifont.bin.z` is GNU Unifont 18.0.01 (`VERSION`), planes 0 and 1,
packed by `examples/pack_font.rs` from Unifont's own precompiled
`unifont-18.0.01.hex` and `unifont_upper-18.0.01.hex`, then deflated.
Regenerate it with `scripts/textart-font.sh`, which takes the source
tarball from the flake's nixpkgs.

Unifont is by Roman Czyborra, Paul Hardy and contributors. It is
dual-licensed under the GNU GPL version 2 or later with the GNU font
embedding exception, and under the SIL Open Font License 1.1
(`OFL-1.1.txt`, copied from the tarball). This crate redistributes it
under the OFL.
