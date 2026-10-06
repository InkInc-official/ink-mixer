# ADR-0004: GUI フレームワークに egui（glow）を採用する

- Status: Accepted
- Date: 2026-10-06
- Decider: 所長（Ink Inc.）

## Decision

Phase 1 の GUI は egui / eframe で作る。描画の既定は glow（OpenGL）とする。
wgpu は代わりの描画方式として、cargo feature で切り替えられる構成にしておく。

## Context

Ink Mixer は配信者向けのアプリで、低スペックの PC（創作PC）でも軽く動くことを方針としている。
UI には日本語を表示し、Windows 10/11 と Linux を正式対象とする（ARCHITECTURE.md §3、§29）。
Phase 0 では GUI フレームワークを未決としていた（ARCHITECTURE.md §39、AGENTS.md §6）。

Spike #21 で egui の試作（5チャンネルのメーター・スライダー・ミュート、日本語ラベル、ダミーデータ）を作り、
創作PC（Linux / Intel UHD）と Windows のメインPC（Intel Iris Xe）で、メモリ・CPU・ビルドを実測した。
試作のコードはブランチ `spike/egui`（コミット `8a8c7a4`、`c652669`）にあり、main にはマージしない。

## Options

1. **egui + glow**
2. **egui + wgpu**
3. **iced**
4. **その他（Slint、Tauri）**

2〜4 は実機で測定していない（2 のみ #21 で参考値を取った）。

## Chosen

Option 1: egui + glow

## Why

- 日本語を OS のフォントで表示できた（Linux: Noto Sans CJK JP、Windows: 游ゴシック）
- 創作PC（Linux）: メモリ約 125 MiB、60fps 時の CPU 約 15%、30fps 時の CPU 約 7%、メーター停止時の CPU 0%
- Windows（メインPC）: fps に上限をかければ創作PCとほぼ同じ（60fps で 15.4%、30fps で 7.7%）
- glow と wgpu の CPU の差は小さく、glow のほうが依存・バイナリが小さく、ビルドも短い
- リリースビルドは 12.9 MiB、クリーンビルドは 3分44秒（創作PC、jobs = 2）
- 依存にコピーレフトだけのライセンスは無い

採用しなかった理由（いずれも測定はしていない）:

- egui + wgpu: CPU は glow とほぼ同じで、依存・バイナリ・ビルド時間が増える（代わりの描画方式としては残す）
- iced: 作者が実験的なソフトウェアとしていて、ドキュメントが少ない
- Slint: 無償利用は GPLv3 か表記付きライセンスになり、リポジトリのライセンスに制約が出る
- Tauri: WebView のメモリが増え、メーター値の頻繁な受け渡しが必要。低スペック方針と合わない

## Consequences

- fps の上限はアプリ側で管理する。egui の `request_repaint_after` は予測フレーム時間（約 16.7ms）を差し引くため、上限として働かない。上限が無いと、Windows（Intel の OpenGL ドライバ）では 1コアを使い切った（101.3%）
- メーターが止まっているときは再描画を要求しない（停止時の CPU は 0〜1.7%）
- 低スペック向けに 30fps の設定を用意できる（#21 で CPU がほぼ半分になった）
- egui はマイナー版ごとに API が変わる（0.36 で `App::update` が `App::ui` に、`TopBottomPanel` が `Panel` に変わった）。バージョンは固定し、上げるときは別 PR にする
- egui 0.36 は Rust 1.95 以上が必要
- GPU の無い環境（llvmpipe などのソフトウェア描画）は未測定
- LV2 プラグインの独自 GUI の埋め込み（Phase 9）は未検証
- 配布するときは、egui に同梱されている欧文フォント（OFL-1.1 と Ubuntu Font License）の表記が必要（ROADMAP Phase 2 の cargo-about）
- 日本語フォントを同梱するか、OS のフォントを読むかは未決。Phase 1 の GUI 実装を始める前に決める → ADR-0006 で決定（Noto Sans JP を同梱）
- GUI は Engine の実装詳細に依存しない（AGENTS.md §4 の5）
