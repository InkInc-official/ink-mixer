# Ink Mixer — Phase 0 Issues

Phase 0の目的: **公開可能な骨格を作る。**
完了条件: アプリがWindows / Linuxで起動し、音声デバイス一覧を取得できる。

GUIフレームワークは未決のため、Phase 0はCLIで動作確認する。

---

## #1 Rust workspaceを作成する

**内容**
- ルートに `Cargo.toml`（workspace）を作成
- 初期crateは3つに絞る（細分化しすぎない）
  - `crates/ink-core` — Audio Graphの型、設定など
  - `crates/ink-backend` — 音声I/Oの抽象と実装
  - `app/ink-mixer` — 実行ファイル
- ビルド並列数の制限（`jobs = 2`）は創作PCのユーザー設定 `~/.cargo/config.toml` で行う（リポジトリには置かない。創作PC 8GB対策）
- `.gitignore`

**完了条件**
- `cargo build` と `cargo test` が通る
- `cargo run -p ink-mixer` で起動メッセージが出る

---

## #2 ドキュメントを配置する

**内容**
- Blueprintを `docs/ARCHITECTURE.md` として配置
- `README.md`（概要、対応OS、ビルド方法、ステータス: Pre-alpha）
- `ROADMAP.md`（Phase 0〜9の一覧）
- `docs/adr/0001-rust-core.md` を配置
- `AGENTS.md` / `CLAUDE.md` を配置

**完了条件**
- 上記ファイルがリポジトリに存在する

---

## #3 ロギング基盤を入れる

**内容**
- `tracing` + `tracing-subscriber` を導入
- ログレベルを環境変数（`RUST_LOG`）で変更可能にする
- Realtimeパスでログを出さない方針をコメントで明記

**完了条件**
- 起動時にバージョンとOSがログに出る

---

## #4 ADR-0002: 音声I/Oライブラリ

**内容**
- `docs/adr/0002-audio-backend.md` を作成
- 結論: 初期実装は `cpal` を使う。ただし上位層は `cpal` に直接依存しない
- 比較対象: cpal / PortAudio(binding) / OS API直接

**完了条件**
- ADRがマージされている

---

## #5 AudioBackend traitを定義する

**内容**
- `ink-backend` に `AudioBackend` traitを定義
  - `enumerate_input_devices()`
  - `enumerate_output_devices()`
  - `default_input()`
  - `default_output()`
- デバイス情報は自前の型（`DeviceInfo`: id, name, channels, sample_rates）で返す
- `open_input()` / `open_output()` / `device_events()` はPhase 1以降。今回はシグネチャも入れない

**完了条件**
- traitと型が定義され、`cpal` の型がtraitの外に漏れていない

---

## #6 cpalでデバイス列挙を実装する

**内容**
- `CpalBackend` を実装し、#5のtraitを満たす
- Linux（ALSA経由、PipeWire環境で確認）とWindows（WASAPI）で動作

**完了条件**
- 創作PCとWindows機の両方で、入出力デバイスの一覧が取得できる

---

## #7 Config skeletonを作る

**内容**
- `ink-core` に設定の型を定義（`version` フィールド必須）
- `serde` + `toml` で読み書き
- 保存先はOS標準の設定ディレクトリ（`directories` crate等）
- 中身は選択デバイスIDのみでよい

**完了条件**
- 設定ファイルが存在しなければデフォルト値で生成される
- `version` が読み取れる

---

## #8 CLIでデバイス一覧を表示する

**内容**
- `ink-mixer devices` で入出力デバイスとデフォルトデバイスを表示
- 引数パースは `clap`

**完了条件**
- 創作PCとWindows機で実行し、結果を所長が確認済み

---

## #9 CIを作る

**内容**
- GitHub Actions: `ubuntu-latest` と `windows-latest`
- `cargo build` / `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check`
- Ubuntuでは `libasound2-dev` と `pkg-config` をインストール

**完了条件**
- PRでCIが両OSとも通る

---

## 推奨順序

```
#1 → #2 → #3 → #4 → #5 → #6 → #7 → #8 → #9
```

#9（CI）は#1の直後に前倒ししてもよい。
