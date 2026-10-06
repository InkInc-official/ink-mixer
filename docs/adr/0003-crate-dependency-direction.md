# ADR-0003: crate の依存の向きを ink-backend → ink-core にする

- Status: Accepted
- Date: 2026-10-06
- Decider: 所長（Ink Inc.）

## Decision

`ink-backend` は `ink-core` に依存してよい。`ink-core` は `ink-backend` に依存しない。
`ink-core` と `ink-backend` の両方で使う型（現時点では `DeviceId`）は `ink-core` に置き、`ink-backend` は必要に応じてそれを再公開する。

## Context

ADR-0002 により、音声I/Oの `cpal` は `ink-backend` の内部に閉じ、上位層は `cpal` に直接依存しない。
Phase 0 #5（GitHub #7）で、デバイスの識別子 `DeviceId` を `ink-backend` に定義した。

Phase 0 #7（GitHub #9）で、選択したデバイスの ID を設定ファイルに保存するため、`ink-core` の設定型が `DeviceId` を扱う必要が出た。
しかし `ink-core` が `ink-backend` に依存すると、`ink-backend` が依存する `cpal` が推移的に `ink-core` へ入り、ADR-0002 の方針に反する。

## Options

1. **案A: `DeviceId` を `ink-core` に移し、`ink-backend` が `ink-core` に依存する**
2. **案B: 共有型だけの新しい crate（例 `ink-types`）を作り、`ink-core` と `ink-backend` の両方がそれに依存する**
3. **案C: 設定には `String` で保存し、`DeviceId` との変換はアプリ側で行う（依存は変えない）**

## Chosen

Option 1: 案A

## Why

- 依存の向きが一方向（`ink-backend` → `ink-core`）に揃い、`cpal` が `ink-core` に入らない
- 型が1つで済み、設定にも `Option<DeviceId>` として型付きで保存できる
- crate を増やさずに済む（ARCHITECTURE.md §31「実装開始時にcrateを細分化しすぎないこと」、Phase 0 #1 の「初期crateは3つに絞る」と整合する）
- 将来、Audio Graph（`ink-core`）が backend と共有する型を増やすときも、同じ向きで足せる

案Bは最も疎な構成だが、Phase 0 時点では中身が `DeviceId` 1つだけの crate になり過剰である。
案Cは変更が最小だが、設定側では型の保証が無く、変換コードがアプリに散らばる。将来 `ink-core` が `DeviceId` を扱う時点で、結局A・Bへの移行が必要になる。

## Consequences

- `DeviceId` を `ink-backend` から `ink-core` に移し、`ink-backend` は `pub use ink_core::DeviceId;` で再公開する。`ink_backend::DeviceId` を使う既存コードは変更不要
- ADR-0002 の「`ink-backend` が定義する `AudioBackend` trait と自前のデバイス型」のうち、`DeviceId` はこの ADR で `ink-core` に移った。`AudioBackend` trait・`DeviceInfo`・`BackendError` は引き続き `ink-backend` にある
- `ink-core` に `serde` が入る（`DeviceId` の保存のため）。`ink-core` は引き続き OS 固有 API と `cpal` に依存しない
- Phase 1 以降、`AudioBackend` trait など他の抽象を `ink-core` に移すかどうかは、必要になった時点で改めて決める
