# Ink Mixer Roadmap

[日本語](#ja) | [English](#en)

---

<a id="ja"></a>

## 日本語

このロードマップは [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) の「33. 開発フェーズ」を要約したものです。内容が食い違う場合は ARCHITECTURE.md を正とします。

**現在地: Phase 0**

### Phase 0 — Repository / Skeleton（進行中）

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

**完了条件:** MIC + BGM + SE を同時にミックスして出力できる。

**このフェーズを最初の Public Alpha 候補とする。**

### Phase 3 — Built-in DSP

- Gate / EQ / Compressor / Limiter / Reverb
- Noise suppression 候補
- Ducking

**完了条件:** 初心者が外部プラグイン無しで、配信用の基本的な音声処理を行える。

### Phase 4 — Linux / Windows Backend 強化

- WASAPI 固有対応
- PipeWire 固有対応
- reconnect
- device events
- routing
- virtual output 連携

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
- 全配信サービス固有の連携
- 外部 Plugin 独自 GUI の完全対応

---

<a id="en"></a>

## English

This roadmap summarizes section 33 ("Development Phases") of [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (in Japanese). If they disagree, ARCHITECTURE.md is the source of truth.

**Current phase: Phase 0**

### Phase 0 — Repository / Skeleton (in progress)

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

**Done when:** MIC + BGM + SE can be mixed together and output.

**This phase is the first Public Alpha candidate.**

### Phase 3 — Built-in DSP

- Gate / EQ / Compressor / Limiter / Reverb
- Noise suppression (candidate)
- Ducking

**Done when:** beginners can do basic streaming audio processing without external plugins.

### Phase 4 — Linux / Windows Backend Improvements

- WASAPI-specific support
- PipeWire-specific support
- Reconnect
- Device events
- Routing
- Virtual output integration

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
- Integrations specific to every streaming service
- Full support for external plugins' own GUIs
