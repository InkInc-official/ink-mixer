# AGENTS.md — Ink Mixer

このファイルは、Ink Mixerに関わるすべてのAIコーディングエージェント（Claude Code / OpenCode / その他）の共通ルールである。
作業前に必ず読み、迷ったら `docs/ARCHITECTURE.md` を正とすること。

---

## 1. プロジェクト概要

Ink Mixer は、配信者向けの拡張可能なリアルタイム・オーディオコンソールである。

- Core言語: **Rust**
- 正式対象OS: **Windows 10/11 / Linux**（macOSは非公式）
- ライセンス方針: OSS / 無料
- 運営: Ink Inc.（所長: 黒井葉跡）

設計の正典: `docs/ARCHITECTURE.md`（Ink Mixer Architecture Blueprint）
設計判断の記録: `docs/adr/`

---

## 2. 開発環境

| 環境 | 用途 |
|---|---|
| 創作PC（MODICIA O.S. / Debian系 / X11 / RAM 8GB） | メイン開発機。Linux実機検証。LV2資産（`~/.local/lib/lv2/`）あり |
| Windows機 | Windows / WASAPI 実機検証 |

- 創作PCはRAM 8GBのため、ビルドの並列数を抑える（創作PCのユーザー設定 `~/.cargo/config.toml` で `jobs = 2` を設定する。リポジトリには置かない）。
- 重いビルドや大量の依存追加を行う前に、所長へ確認すること。

---

## 3. 作業の進め方（必須）

1. **1 Issue ＝ 1ブランチ ＝ 1 PR。** Issueの範囲を超えた変更をしない。
2. **変更は1ファイルずつ。** 1ファイル変更したら所長の承認を待ってから次へ進む。
3. **コードは完全な形で出力する。** `// ... existing code` のような省略をしない。
4. **既存ロジックを削除しない。** 機能追加の際に、既存の処理・設定・テストを黙って消さない。削除が必要なら理由を示して確認を取る。
5. **エラーの原因を環境のせいにしない。** まずパス・型定義・依存関係など自分の変更を疑う。
6. **認証操作をしない。** `gh auth`、トークン設定、`git push` の認証、各種ログインは所長が行う。
7. **曖昧なら推測せず質問する。**

---

## 4. 変更前チェックリスト

変更を提案する前に、以下を確認すること。

1. Audio Graphの抽象化（Source / Node / Mixer / Sink）を壊していないか。
2. OS固有コード（WASAPI / PipeWire / cpal固有型）がCore・UIへ漏れていないか。
3. Realtimeスレッドでブロッキング処理をしていないか（§5）。
4. Built-in DSP と External Plugin を混同していないか。
5. UIがEngineの実装詳細へ直接依存していないか。
6. 設定に生のファイルパスを直接保存していないか（Asset IDを使う）。
7. Event EngineをVoice Trigger専用にしていないか。
8. まだ必要のない大きな機能を先回り実装していないか。
9. 新規依存crateのライセンスを確認したか。
   MPL-2.0 などファイル単位の弱いコピーレフトは、改変せずに依存として使う場合は許容する（改変・ソースの取り込みをする場合は所長に確認する）。
10. Windows / Linux 双方への影響を記録したか。

---

## 5. Realtimeスレッドのルール

Audio callback / realtime処理パスでは以下を**禁止**する。

- ファイルI/O、ネットワークI/O
- `sleep`
- Mutexの長時間待ち
- 大量のログ出力
- UI処理
- STT推論
- プラグインスキャン
- 設定ファイルの保存
- 予測できないメモリ確保（`Vec::push` による再確保、`Box::new` 等）

制御側から音声側へ値を渡すときは、lock-free queue / atomic / 事前確保バッファ / message passing を使う。

---

## 6. 決定済み事項と未決事項

### 決定済み（ADRあり、または所長承認済み）

- Core言語はRust（ADR-0001）
- 最初の音声I/Oライブラリは `cpal`（ADR-0002）
- crate の依存の向きは `ink-backend` → `ink-core`。`ink-core` は `ink-backend` に依存しない（`cpal` を core に入れないため）。両方で使う型（`DeviceId` 等）は `ink-core` に置く（ADR-0003）

### 未決（決定済みとして扱わないこと）

- GUIフレームワーク
- リポジトリのライセンス
- Windowsの仮想出力方式
- LV2 Hostライブラリの採用範囲
- Noise Suppressionエンジン
- プラグインのプロセス分離方式
- Python SDKの方式
- VST3 / macOS の対応時期

未決事項に関わる実装が必要になったら、実装せずに所長へ報告し、ADRの起票を提案すること。

---

## 7. 初期版でやらないこと

- LV2 / VST3 Hostの実装（Phase 8まで）
- 自作仮想オーディオドライバ
- macOS正式対応
- DAW並みの編集機能
- 完全なプラグインサンドボックス

---

## 8. コマンド

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

PRを出す前に、上記4つがすべて通ることを確認する。

Linuxでのビルドには `libasound2-dev` と `pkg-config` が必要（cpalのALSAバックエンド用）。

---

## 9. コミット / PR

- コミットメッセージ: `type(scope): 要約`（例: `feat(backend): enumerate output devices`）
- type: `feat` / `fix` / `refactor` / `docs` / `test` / `chore` / `ci`
- PR本文には「対応Issue」「変更内容」「Windows/Linuxへの影響」「確認した方法」を書く。
- ブランチ名: `type/<GitHub Issue番号>-<短い説明>`（例: `feat/5-logging`）。番号は GitHub の Issue 番号を使う。
- PR本文の対応Issueは `Closes #<GitHub Issue番号>` と書く。
- `docs/PHASE0_ISSUES.md` の見出しの番号（Phase 0 番号）と GitHub の Issue 番号は異なる。
  GitHub 上で Phase 0 の項目を参照するときは、裸の `#N` ではなく「Phase 0 #N（#GitHub番号）」と書く（`#N` は GitHub が別の Issue に自動リンクしてしまうため）。
- Windows 実機での確認は、Phase ごとに GitHub Issue「Windows 実機確認（Phase N）」を1つ作り、確認項目をチェックボックスで追記してまとめる。
  PR では、追記した先の Issue を「Windows/Linuxへの影響」に書く。

---

## 10. ログ出力

- ログは `tracing` で出し、出力先は **stderr** にする。
- **stdout はコマンドの出力（デバイス一覧など）専用**とし、ログを混ぜない。
- ログの色付け（ANSIエスケープ）は、stderr が端末のときだけ行う。ファイルや pipe に出すときは色を付けない。
- `RUST_LOG` でレベルを変更できるようにし、未設定のときは `info` を既定にする。
- Realtimeパスではログを出さない（§5）。
