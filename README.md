# Ink Mixer

[日本語](#ja) | [English](#en)

---

<a id="ja"></a>

## 日本語

Ink Mixer は、配信者向けの拡張可能なリアルタイム・オーディオコンソールです。

### ステータス

**Pre-alpha（Phase 0）** — リポジトリの骨格を作っている段階です。現在は起動メッセージを表示するだけで、音声処理はまだ実装されていません。

今後の計画は [ROADMAP.md](ROADMAP.md) を参照してください。

### 対応OS

| OS | 方針 |
|---|---|
| Windows 10/11 | 正式対象 |
| Linux | 正式対象 |
| macOS | 非公式・未検証 |

### ビルド方法

**必要なもの**

- Rust（stable、edition 2024 に対応した 1.85 以降）
- Linux のみ: `libasound2-dev` と `pkg-config`（音声I/Oライブラリ `cpal` の ALSA バックエンド用）

Debian / Ubuntu の場合:

```bash
sudo apt install libasound2-dev pkg-config
```

**ビルドと実行**

```bash
cargo build
cargo run -p ink-mixer
```

### ドキュメント

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — 設計の正典（Architecture Blueprint）
- [ROADMAP.md](ROADMAP.md) — 開発フェーズ
- [docs/adr/](docs/adr/) — 設計判断の記録（ADR）
- [AGENTS.md](AGENTS.md) — AIコーディングエージェント向けの共通ルール

### ライセンス

**未定（TBD）**

---

<a id="en"></a>

## English

Ink Mixer is an extensible real-time audio console for streamers.

### Status

**Pre-alpha (Phase 0)** — The repository skeleton is being built. The app currently only prints a startup message; audio processing is not implemented yet.

See [ROADMAP.md](ROADMAP.md) for the plan ahead.

### Supported OS

| OS | Status |
|---|---|
| Windows 10/11 | Officially supported |
| Linux | Officially supported |
| macOS | Unofficial / untested |

### Building

**Requirements**

- Rust (stable, 1.85 or later for edition 2024)
- Linux only: `libasound2-dev` and `pkg-config` (for the ALSA backend of the `cpal` audio I/O library)

On Debian / Ubuntu:

```bash
sudo apt install libasound2-dev pkg-config
```

**Build and run**

```bash
cargo build
cargo run -p ink-mixer
```

### Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — Architecture blueprint (source of truth, in Japanese)
- [ROADMAP.md](ROADMAP.md) — Development phases
- [docs/adr/](docs/adr/) — Architecture Decision Records
- [AGENTS.md](AGENTS.md) — Shared rules for AI coding agents (in Japanese)

### License

**TBD (not yet decided)**
