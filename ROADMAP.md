# Ink Mixer Roadmap

[日本語](#ja) | [English](#en)

---

<a id="ja"></a>

## 日本語

このロードマップは [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) の「33. 開発フェーズ」を要約したものです。内容が食い違う場合は ARCHITECTURE.md を正とします。

**現在地: Phase 0 完了、Phase 1 準備中**

### 公開とお披露目の計画

公開とお披露目は、開発フェーズとは別に次の順で進めます。

1. **リポジトリを静かに公開する** — 宣伝はしません。
2. **Phase 1・2** — マイク・BGM・効果音を混ぜて出力できるところまで作ります。
3. **文字起こし（STT）の Spike** — ローカルで動く文字起こしエンジンを試作し、遅延・CPU 使用率・日本語の認識精度と、創作PC（RAM 8GB）でミキサーと同時に動くかを確かめます。あわせて、結果（漢字まじり）を読み（カタカナ）に直して、カタカナで登録したトリガーワードと照らし合わせられるかも確かめます。エンジンは結果を元に ADR で決めます。
4. **ボイストリガーまでの最短の道** — Phase 5・6 のうち、ボイストリガーに必要な最小限だけを先に作ります（マイクの分岐、文字起こしの処理、決めた言葉を認識したら効果音を鳴らす、cooldown）。Phase 3・4 と、Phase 5・6 の残り（OBS 字幕、複数のアクションなど）はその後に作ります。
5. **所属ライバーの先行利用** — Ink Inc. 所属のライバーに実際の配信で使ってもらい、見つかった問題を直します。
6. **お披露目** — 目玉はボイストリガーです（例：「オーマイガー」と言うと効果音が鳴る）。

先に作る場合も、ボイストリガーは Event → Condition → Action の汎用的な仕組みの1つとして作ります。また、MASTER ではなくマイクを分岐した音声を認識します（ARCHITECTURE.md §11）。

Phase 2 の Public Alpha は配布を始める時期の目安で、宣伝はお披露目まで行いません。

### Phase 0 — Repository / Skeleton（完了）

目的: 公開可能な骨格を作る。

- Rust workspace
- アプリの起動
- AudioBackend interface
- config skeleton
- CI
- README / ROADMAP
- logging

**完了条件:** アプリが Windows / Linux で起動し、音声デバイス一覧を取得する土台がある。

### Phase 1 — Minimum Audio Path

```text
MIC -> Gain -> MASTER -> Output
```

- input device / output device
- GainNode / MixerNode
- meter
- master volume

**完了条件:** マイク音声を、選択した出力へリアルタイムに送出できる。

### Phase 2 — BGM / SE（最初の Public Alpha 候補）

- WAV / MP3
- BGM player
- Sound Pad
- Sound Library
- Loop
- volume
- basic UI
- 音源・SE の登録（「音源取り込み」ページ。Ink Mixer のフォルダへコピーする。「権利を確認した音源を入れてください」と表示する）
- 配布前に `cargo-about` で依存crateのライセンス表記を作る

**完了条件:** MIC + BGM + SE を同時にミックスして出力できる。

**このフェーズを最初の Public Alpha 候補とする。**

### Phase 3 — Built-in DSP

- Gate / EQ / Compressor / Limiter / Reverb
- Echo / Delay
- Noise suppression 候補
- Ducking
- 歌枠プリセット（声にだけリバーブ・コンプ等をかけ、音源にはかけない。音源と声の音量バランスの初期値）

**完了条件:** 初心者が外部プラグイン無しで、配信用の基本的な音声処理を行える。

### Phase 4 — Linux / Windows Backend 強化

- WASAPI 固有対応
- PipeWire 固有対応
- reconnect
- device events
- routing
- virtual output 連携
- PC 内のアプリの音を 1 チャンネルとして取り込む（中立な入口。Linux は PipeWire、Windows は Win10 以降の仕組み。作るときに ADR）

### Phase 5 — STT（音声文字起こし）

- MIC branch
- realtime transcription worker
- transcript
- OBS caption output

### Phase 6 — Event / Trigger

- Voice phrase
- cooldown
- PlaySound action / volume action
- multi-action
- Trigger UI

### Phase 7 — Plugin Foundation

- PluginManager
- PluginDescriptor
- ExternalPluginNode interface
- Plugin registry
- scanner boundary

このフェーズではまだ LV2 をロードしなくてもよい。

### Phase 8 — LV2 Host Alpha

- LV2 discovery / metadata parsing
- 対応可能な audio-effect の分類
- instantiate / connect ports / process audio
- generic parameter UI
- bypass
- state / preset の基礎
- compatibility report

最初の目標は、1つの単純な LV2 effect を安全に Graph へ挿入すること。そこから複数のプラグインへ広げる。

### Phase 9 — Plugin Ecosystem

- custom LV2 UI
- crash recovery
- blacklist
- latency handling
- VST3 / LADSPA の調査
- Ink Mixer Plugin SDK
- LV2 プラグインの同梱（プラグインごとのライセンス確認と、ソースの入手方法の提示が前提）
- プラグインをダウンロードできる仕組み（検討）

### 将来の検討事項

- 外部入力チャンネル（音源用の端末を USB アダプタ等でつなぐ。別機器の時計のずれの補正が必要。作るときに ADR）
- 口癖禁止ゲーム（配信者の口癖をトリガーワードにしてハズレ音を鳴らし、回数を数える。Event → Condition → Action の汎用の仕組みに乗る）

### 初期版でやらないこと

v0.0.x / 最初の Alpha では、以下を完成させようとしない。

- 131 LV2 の完全対応
- VST3 Host
- 自作 Virtual Audio Driver
- macOS 正式対応
- DAW 並みの編集機能
- 完全な Plugin sandbox
- MIDI 全面対応
- 高度な録音編集
- 全配信サービス固有の連携（外部のサービスとは連携を作らない方針: [ADR-0008](docs/adr/0008-external-services.md)）
- 外部 Plugin 独自 GUI の完全対応

---

<a id="en"></a>

## English

This roadmap summarizes section 33 ("Development Phases") of [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (in Japanese). If they disagree, ARCHITECTURE.md is the source of truth.

**Current phase: Phase 0 complete, preparing Phase 1**

### Release and Launch Plan

Separately from the development phases, the release and public launch proceed in this order:

1. **Publish the repository quietly** — no promotion.
2. **Phase 1 and 2** — up to mixing and outputting the microphone, BGM, and sound effects.
3. **Speech-to-text (STT) spike** — prototype a local speech-to-text engine and check latency, CPU usage, Japanese recognition accuracy, and whether it runs alongside the mixer on the main development PC (8 GB RAM). It also checks whether the result (which comes back with kanji) can be converted to its reading (katakana) and matched against trigger words registered in katakana. The engine will be chosen in an ADR based on the results.
4. **Shortest path to voice triggers** — build only the minimum parts of Phase 5 and 6 needed for voice triggers first (a microphone branch, speech-to-text processing, playing a sound effect when a chosen phrase is recognized, and a cooldown). Phase 3 and 4, and the rest of Phase 5 and 6 (OBS captions, multiple actions, etc.), come after that.
5. **Early use by Ink Inc.'s affiliated streamers** — they use it in real streams, and the problems they find are fixed.
6. **Public launch** — the headline feature is voice triggers (e.g., saying "Oh my god" plays a sound effect).

Even when built early, voice triggers are implemented as one use of the general Event → Condition → Action system, and they recognize a branch of the microphone audio, not MASTER (ARCHITECTURE.md §11).

The Phase 2 Public Alpha marks roughly when distribution starts; there will be no promotion until the public launch.

### Phase 0 — Repository / Skeleton (done)

Goal: a publishable skeleton.

- Rust workspace
- App startup
- AudioBackend interface
- Config skeleton
- CI
- README / ROADMAP
- Logging

**Done when:** the app starts on Windows / Linux and has the groundwork for listing audio devices.

### Phase 1 — Minimum Audio Path

```text
MIC -> Gain -> MASTER -> Output
```

- Input device / output device
- GainNode / MixerNode
- Meter
- Master volume

**Done when:** microphone audio can be sent to the selected output in real time.

### Phase 2 — BGM / SE (first Public Alpha candidate)

- WAV / MP3
- BGM player
- Sound Pad
- Sound Library
- Loop
- Volume
- Basic UI
- Registering audio and sound effects (an "import audio" page; files are copied into Ink Mixer's folder; a notice asks for audio whose rights have been checked)
- Generate third-party license notices with `cargo-about` before distribution

**Done when:** MIC + BGM + SE can be mixed together and output.

**This phase is the first Public Alpha candidate.**

### Phase 3 — Built-in DSP

- Gate / EQ / Compressor / Limiter / Reverb
- Echo / Delay
- Noise suppression (candidate)
- Ducking
- Singing-stream preset (reverb, compression, etc. only on the voice, not on the audio source; initial volume balance between the audio and the voice)

**Done when:** beginners can do basic streaming audio processing without external plugins.

### Phase 4 — Linux / Windows Backend Improvements

- WASAPI-specific support
- PipeWire-specific support
- Reconnect
- Device events
- Routing
- Virtual output integration
- Take in the sound of applications on the PC as one channel (a neutral input; PipeWire on Linux, the mechanism available since Windows 10 on Windows; an ADR when it is built)

### Phase 5 — STT (Speech-to-Text)

- MIC branch
- Realtime transcription worker
- Transcript
- OBS caption output

### Phase 6 — Event / Trigger

- Voice phrase
- Cooldown
- PlaySound action / volume action
- Multi-action
- Trigger UI

### Phase 7 — Plugin Foundation

- PluginManager
- PluginDescriptor
- ExternalPluginNode interface
- Plugin registry
- Scanner boundary

LV2 loading is not required in this phase.

### Phase 8 — LV2 Host Alpha

- LV2 discovery / metadata parsing
- Filtering compatible audio effects
- Instantiate / connect ports / process audio
- Generic parameter UI
- Bypass
- State / preset basics
- Compatibility report

The first goal is to safely insert one simple LV2 effect into the graph, then expand to more plugins.

### Phase 9 — Plugin Ecosystem

- Custom LV2 UI
- Crash recovery
- Blacklist
- Latency handling
- VST3 / LADSPA investigation
- Ink Mixer Plugin SDK
- Bundling LV2 plugins (requires checking each plugin's license and providing how to get its source)
- A way to download plugins (under consideration)

### Under consideration

- An external input channel (connecting a separate device for audio sources, for example through a USB adapter; needs correction for the clock drift of the other device; an ADR when it is built)
- A "verbal tic game" (the streamer's verbal tic is a trigger word that plays a miss sound and counts how often it happens; built on the general Event → Condition → Action system)

### Out of scope for the first releases

v0.0.x / the first Alpha will not try to complete:

- Full support for all 131 LV2 bundles
- VST3 host
- Custom virtual audio driver
- Official macOS support
- DAW-level editing features
- Full plugin sandboxing
- Full MIDI support
- Advanced recording/editing
- Integrations specific to every streaming service (Ink Mixer does not build integrations with external services: [ADR-0008](docs/adr/0008-external-services.md), in Japanese)
- Full support for external plugins' own GUIs
