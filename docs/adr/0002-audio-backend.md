# ADR-0002: 音声I/Oライブラリに cpal を採用する

- Status: Accepted
- Date: 2026-10-06
- Decider: 所長（Ink Inc.）

## Decision

初期実装の音声I/Oには `cpal` を使う。
ただし `cpal` を使うのは `ink-backend` の内部だけとし、`ink-core`・アプリ・将来のUIは `cpal` に直接依存しない。
上位層は `ink-backend` が定義する `AudioBackend` trait と自前のデバイス型（`DeviceInfo` 等）だけを使う。

## Context

Ink Mixer は Windows 10/11 と Linux を正式対象とし、Windows では WASAPI、Linux では PipeWire を想定バックエンドとしている（ARCHITECTURE.md §3）。
Phase 0 では、両OSで音声デバイスの一覧を取得できる土台を作る必要がある。

一方で、将来は WASAPI 固有対応・PipeWire 固有対応・仮想出力連携（Phase 4）が予定されており、
初期に選んだライブラリが上位層へ漏れると、後からの差し替えが難しくなる（ARCHITECTURE.md §3, §27）。

## Options

1. **cpal**（Rust製のクロスプラットフォーム音声I/O crate）
2. **PortAudio のバインディング**（C ライブラリ PortAudio を Rust から FFI で使う）
3. **OS API の直接利用**（Windows は WASAPI、Linux は PipeWire / ALSA を各OSごとに直接実装）

## Chosen

Option 1: cpal

## Why

- Pure Rust で、Windows（WASAPI）と Linux（ALSA）を1つのAPIで扱える。Phase 0 の「両OSでデバイス一覧を取得する」を最小の実装量で満たせる
- C ライブラリのビルド・同梱が不要。Linux で必要なのは `libasound2-dev` と `pkg-config` だけ
- ライセンスが Apache-2.0 で、OSS として配布しやすい
- Rust の音声 crate として利用実績が多く、メンテナンスが続いている

PortAudio のバインディングは実績のある C ライブラリを使えるが、Windows でのビルドと同梱の手間が増え、
FFI 境界でのエラー処理も必要になる。cpal に対する利点が Phase 0 の範囲では小さい。

OS API の直接利用は最も細かく制御できるが、OSごとに実装が2倍になり、Phase 0 の範囲を大きく超える。
OS 固有の機能が必要になった時点で、`ink-backend` 内に個別の実装を追加すればよい。

## Consequences

- `cpal` の型（`cpal::Device` 等）は `ink-backend` の外に出さない。trait と自前の型で包む（GitHub Issue #7, #8）
- Linux では cpal が ALSA 経由で PipeWire に接続する（`pipewire-pulse` / `pipewire-alsa` 等のブリッジに依存する）。PipeWire 固有の機能（ルーティング、仮想デバイス）は ALSA 経由では扱えない可能性がある
- PipeWire / WASAPI 固有対応、仮想出力方式が必要になった時点（Phase 4）で、`ink-backend` 内に別実装を追加するかを別ADRで決定する。上位層は変更しない
  - cpal 自体にも `pipewire` / `jack` feature があるため、その時点の選択肢に含める（現時点では有効にしない）
- バッファサイズ・レイテンシ・デバイスの抜き差しなど、cpal の挙動は実機で確認する（ARCHITECTURE.md §26）
