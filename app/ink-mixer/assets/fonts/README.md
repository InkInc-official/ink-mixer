# Fonts

Fonts embedded in the Ink Mixer executable (ADR-0006).

## Noto Sans JP Regular

| | |
|---|---|
| File | `NotoSansJP-Regular.otf` |
| Font | Noto Sans JP, Regular, Version 2.004 |
| Copyright | © 2014-2021 Adobe (http://www.adobe.com/). Noto is a trademark of Google Inc. |
| License | SIL Open Font License, Version 1.1 (full text: [`OFL.txt`](OFL.txt)) |
| Source | https://github.com/notofonts/noto-cjk, commit `f8d157532fbfaeda587e826d4cd5b21a49186f7c`, path `Sans/SubsetOTF/JP/NotoSansJP-Regular.otf` |
| Retrieved | 2026-10-12 |
| Size | 4,533,028 bytes |
| SHA-256 | `dff723ba59d57d136764a04b9b2d03205544f7cd785a711442d6d2d085ac5073` |

- The font is used **unmodified**, as distributed by the source above. "SubsetOTF/JP" is the upstream name of the Japanese edition of Noto Sans CJK; Ink Mixer does not remove any characters from it.
- The copyright and trademark lines above are taken from the font's `name` table (IDs 0 and 7), which the unmodified file still carries.
- The font is a separate work under the OFL, not under the GPL of Ink Mixer's code (ADR-0005, ADR-0006). It is embedded with `include_bytes!` and is not sold by itself.

## OFL.txt

| | |
|---|---|
| File | `OFL.txt` |
| Source | https://github.com/notofonts/noto-cjk, commit `f8d157532fbfaeda587e826d4cd5b21a49186f7c`, path `Sans/LICENSE` |
| Size | 4,301 bytes |
| SHA-256 | `6a73f9541c2de74158c0e7cf6b0a58ef774f5a780bf191f2d7ec9cc53efe2bf2` |

Copied unchanged from the source. The upstream file has no copyright line of its own; the copyright notice is the one in the table above.

## Updating

To replace a font, record the new source commit, size, and SHA-256 here, and keep the file unmodified.
