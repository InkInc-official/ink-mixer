# CLAUDE.md — Ink Mixer

@AGENTS.md

上記の共通ルールに加え、Claude Code固有の運用を以下に定める。

---

## 作業開始時

1. 対象のIssueを読み、範囲を一文で要約して所長に確認する。
2. 複数ファイルに触れる作業は、Plan Modeで計画を出してから着手する。
3. 計画には「触るファイル」「触らないファイル」「完了条件」を明記する。

## 実装中

- 1ファイル変更するごとに `cargo check` を実行し、結果を報告してから所長の承認を待つ。
- 依存crateを追加するときは、crate名・バージョン・ライセンス・追加理由を先に提示する。
- `docs/ARCHITECTURE.md` と矛盾する実装が必要になったら、手を止めて報告する。

## 実行してはいけないコマンド

- `git push`、`gh auth`、トークンやAPIキーの設定
- `cargo publish`
- `rm -rf` 等、リポジトリ外へ影響する削除
- システムの音声設定（PipeWire / ALSA の設定ファイル等）の変更

## 完了時

- `cargo build` / `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt --check` の結果を報告する。
- 実機で確認すべき項目（デバイス、OS）があれば一覧にして所長へ渡す。
- コミットまでは行い、pushは所長が行う。
