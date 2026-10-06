# ADR-0001: Core言語にRustを採用する

- Status: Accepted
- Date: 2026-10-06
- Decider: 所長（Ink Inc.）

## Decision

Ink MixerのCore（Audio Graph、Audio Backend、Event Engine、Plugin Host）はRustで実装する。
Pythonは、AI処理・スクリプティング・開発ツール・将来のPython Plugin SDKに限定して使用する。

## Context

Ink Mixerはリアルタイム音声処理を行う。音声コールバックは数ミリ秒単位で処理を終える必要があり、
GCの停止や予測できないメモリ確保は音切れ（xrun）の原因になる。

また将来、LV2などのネイティブ（C ABI）プラグインをロードする予定があり、
WindowsとLinuxの両方を正式対象とする。

## Options

1. **Rust**
2. **C++**
3. **Python**（+ ネイティブ拡張）
4. **Go**

## Chosen

Option 1: Rust

## Why

- GCがなく、Realtimeパスでのメモリ確保を制御できる
- メモリ安全性により、AIエージェントが生成したコードでも未定義動作を起こしにくい
- C ABIとの連携が容易で、LV2（lilv等）をFFIで扱える
- `cpal` など、クロスプラットフォームの音声I/O crateが存在する
- Cargo workspaceで責務ごとにcrateを分割しやすい

C++はオーディオ分野の実績が最も多いが、メモリ安全性の面でAI主体の開発と相性が悪い。
Python・GoはGCの影響でRealtimeパスに向かない。

## Consequences

- コンパイル時間が長く、創作PC（RAM 8GB）ではビルド並列数の制限が必要
- 借用チェッカーの制約により、Audio Graphの所有権設計に事前の検討が要る
- Python連携が必要になった時点で、PyO3等の方式を別ADRで決定する
