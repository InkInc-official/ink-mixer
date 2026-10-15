# Ink Mixer

[日本語](#ja) | [English](#en)

---

<a id="ja"></a>

## 日本語

Ink Mixer は、配信者向けの拡張可能なリアルタイム・オーディオコンソールです。

### ステータス

**Pre-alpha（Phase 0 完了、Phase 1 準備中）** — `ink-mixer devices` で音声デバイスの一覧を表示できます。音声処理（マイクの入力や出力）はまだ実装されていません。

今後の計画は [ROADMAP.md](ROADMAP.md) を参照してください。

### 対応OS

| OS | 方針 |
|---|---|
| Windows 10/11 | 正式対象 |
| Linux | 正式対象 |
| macOS | 非公式・未検証 |

### ビルド方法

**必要なもの**

- Rust（stable、1.95 以降。GUI の egui / eframe 0.36 が必要とするため）
- Linux のみ: `libasound2-dev` と `pkg-config`（音声I/Oライブラリ `cpal` の ALSA バックエンド用）
- Linux で窓（GUI）を開くには、X11 または Wayland、OpenGL（`libGL` / `libEGL`）、`libxkbcommon` が要ります（デスクトップ環境にはたいてい入っています。ビルドには要りません）

Debian / Ubuntu の場合:

```bash
sudo apt install libasound2-dev pkg-config
```

**ビルドと実行**

```bash
cargo build
cargo run -p ink-mixer
cargo run -p ink-mixer -- devices
```

引数なしの `cargo run -p ink-mixer` は窓（GUI）を開きます。`devices` は、音声の入力・出力デバイスの一覧を表示します（既定のデバイスには `[default]` が付きます）。

### 音源について

Ink Mixer は、特定のサービス（YouTube や Spotify など）から音を取り込む機能を作りません。BGM や効果音は、権利を確認した音源ファイル（フリー BGM など）を Ink Mixer のフォルダに入れて使うのが基本です。入力端子のように、つながれた音を混ぜるだけの入口（中身を知らない入口）は作ることがあります。

流す音の権利の確認は、使う人の責任です（配信サービスの規約にも従ってください）。

配信の許諾がある音源の例（機能ではなく、情報です）:

- 自作の音源
- 配信での利用が許諾されているものとして、購入・依頼した音源

フリー BGM も、配布元の利用規約（クレジット表記、配信での利用の可否など）を確認してください。

これは一般的な案内で、法的な助言ではありません。方針の理由は [ADR-0008](docs/adr/0008-external-services.md) を参照してください。

### ドキュメント

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — 設計の正典（Architecture Blueprint）
- [ROADMAP.md](ROADMAP.md) — 開発フェーズ
- [docs/adr/](docs/adr/) — 設計判断の記録（ADR）
- [AGENTS.md](AGENTS.md) — AIコーディングエージェント向けの共通ルール

### ライセンス

GPL-3.0-or-later。全文は [LICENSE](LICENSE)、理由は [ADR-0005](docs/adr/0005-license.md) を参照。

---

<a id="en"></a>

## English

Ink Mixer is an extensible real-time audio console for streamers.

### Status

**Pre-alpha (Phase 0 complete, preparing Phase 1)** — `ink-mixer devices` lists audio devices. Audio processing (microphone input and output) is not implemented yet.

See [ROADMAP.md](ROADMAP.md) for the plan ahead.

### Supported OS

| OS | Status |
|---|---|
| Windows 10/11 | Officially supported |
| Linux | Officially supported |
| macOS | Unofficial / untested |

### Building

**Requirements**

- Rust (stable, 1.95 or later, required by egui / eframe 0.36 for the GUI)
- Linux only: `libasound2-dev` and `pkg-config` (for the ALSA backend of the `cpal` audio I/O library)
- To open the window (GUI) on Linux: X11 or Wayland, OpenGL (`libGL` / `libEGL`), and `libxkbcommon` (usually present on desktop systems; not needed to build)

On Debian / Ubuntu:

```bash
sudo apt install libasound2-dev pkg-config
```

**Build and run**

```bash
cargo build
cargo run -p ink-mixer
cargo run -p ink-mixer -- devices
```

Without arguments, `cargo run -p ink-mixer` opens the window (GUI). `devices` lists audio input and output devices (the default device is marked `[default]`).

### About audio sources

Ink Mixer does not build features that take in audio from a specific service (such as YouTube or Spotify). For BGM and sound effects, the basic way is to put audio files whose rights you have checked (royalty-free BGM, for example) into Ink Mixer's folder. It may provide neutral inputs: inputs that know nothing about what is connected and only mix the sound that comes in, like an input jack.

Checking the rights to the audio you play is your responsibility (and follow the terms of your streaming service as well).

Examples of audio that is permitted for streaming (information, not a feature):

- Audio you made yourself
- Audio you bought or commissioned with permission to use it in streams

For royalty-free BGM too, check the terms of use of where it is distributed (credit notices, whether it may be used in streams, and so on).

This is general information, not legal advice. For the reasoning, see [ADR-0008](docs/adr/0008-external-services.md) (in Japanese).

### Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — Architecture blueprint (source of truth, in Japanese)
- [ROADMAP.md](ROADMAP.md) — Development phases
- [docs/adr/](docs/adr/) — Architecture Decision Records
- [AGENTS.md](AGENTS.md) — Shared rules for AI coding agents (in Japanese)

### License

GPL-3.0-or-later. See [LICENSE](LICENSE) for the full text and [ADR-0005](docs/adr/0005-license.md) for the rationale (in Japanese).
