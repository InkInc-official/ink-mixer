# ADR-0005: リポジトリのライセンスを GPL-3.0-or-later にする

- Status: Accepted
- Date: 2026-10-06
- Decider: 所長（Ink Inc.）

## Decision

Ink Mixer のリポジトリ（コード・ドキュメント）のライセンスを GPL-3.0-or-later とする。
全文はリポジトリ直下の `LICENSE` に置き、各 crate は `Cargo.toml` の `license = "GPL-3.0-or-later"` をワークスペースから継承する。

この ADR の判断は一般的な考え方に基づくもので、法的な助言ではない。

## Context

Ink Mixer は OSS・無料で公開する方針で（AGENTS.md §1）、ライセンスは未決だった（AGENTS.md §6、ARCHITECTURE.md §39）。
リポジトリを公開する前に決める必要がある。

将来、LV2 プラグインを Ink Mixer と一緒に配布（同梱）したい。創作PCの LV2 資産（ARCHITECTURE.md §14 時点で 131 bundle）には GPL のものが多いと見られる。
ARCHITECTURE.md §23 のとおり、ユーザー環境のプラグインをロードすることと、配布物に同梱することは別の問題である。

## Options

1. **GPL-3.0-or-later**
2. **MIT OR Apache-2.0**（Rust で一般的な許容的ライセンス）
3. **MPL-2.0**（ファイル単位の弱いコピーレフト）

## Chosen

Option 1: GPL-3.0-or-later

## Why

- 本体も GPL にしておくと、GPL のプラグインを同梱するときの判断が明確になる
- Linux のオーディオアプリでは GPL が一般的（Ardour、Carla など）
- 現在の依存（MIT / Apache-2.0 / Unlicense / Unicode-3.0 / MPL-2.0）は GPL-3.0 と組み合わせられる（Issue #24 の PR で一覧を確認）
- Apache-2.0 の依存（cpal など）は GPL-3.0 とは組み合わせられるが、GPL-2.0 とはできない。そのため「GPL-2.0」ではなく「3.0 以降」とする

採用しなかった案:

- MIT OR Apache-2.0: 派生物を自由に作れるが、GPL のプラグインを同梱すると配布物全体の扱いが複雑になる
- MPL-2.0: ファイル単位のコピーレフトで、GPL のプラグインの同梱の判断は GPL ほど明確にならない

## Consequences

- Ink Mixer を元にした非公開の派生アプリは作れなくなる（派生物を配布する場合はソースを GPL で公開する必要がある）
- 依存を追加するときは、GPL-3.0 と組み合わせられるかを確認する（AGENTS.md §4 の9）
- 同梱するプラグインは1つずつライセンスを確認する。再配布できないものは同梱しない（ARCHITECTURE.md §23）
- GPL のプラグインを同梱する場合は、そのソースの入手方法を示す
- 131 は bundle 数であり、同梱できるプラグイン数とは限らない。Windows 版は別に数える
- 配布物には、依存 crate のライセンス表記を添える（ROADMAP Phase 2 の cargo-about）
- フォント（egui 同梱の欧文フォント、ADR-0006 の日本語フォント）は OFL などで配布される。GPL の配布物に埋め込むときの扱いは、組み込みの実装時に確認する
