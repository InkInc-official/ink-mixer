# Ink Mixer --- AI開発者向け設計図

**文書種別:** Architecture / Product / Implementation Blueprint\
**想定読者:** Claude
Code、OpenCode、Codex、Goose、その他のAIコーディングエージェント、人間の開発者\
**ステータス:** Draft v0.1 --- 実装開始前の基準設計\
**基本方針:**
まず最小構成を公開し、互換性と拡張性を壊さず高速に進化させる

------------------------------------------------------------------------

## 0. この文書の目的

Ink Mixer は、配信者向けのリアルタイム・オーディオミキサーである。

単なる「音量調整アプリ」ではなく、将来的には以下を一つの基盤上で扱う。

-   マイク
-   BGM
-   SE / Sound Pad
-   AUX / 外部音声
-   DSPエフェクト
-   Windows / Linux の音声入出力
-   スマートフォン配信用の完成音声出力
-   OBS等へ渡す仮想出力
-   音声文字起こし
-   音声トリガー
-   自動化
-   外部オーディオプラグイン
-   外部API / Plugin SDK

この文書の最重要目的は、**途中から別のAI開発者が参加しても「何を作っているのか」「なぜこの構造なのか」「今どこまで作るべきか」を推測せず理解できる状態にすること**である。

------------------------------------------------------------------------

# 1. Product Vision

## 1.1 一文で表すと

> **Ink Mixer =
> 配信者向けの、拡張可能なリアルタイム・オーディオコンソール。**

初心者には簡単なミキサーとして見え、上級者にはDSP、Audio
Graph、外部プラグイン、自動化まで開放される構造を目指す。

## 1.2 主な利用シーン

### A. Mobile Streaming Mode

IRIAM / REALITY 等、スマートフォン上で配信を完結するサービス向け。

``` text
Microphone ─┐
BGM ────────┼─> Ink Mixer ─> MASTER ─> OS Output ─> Phoneへの物理的音声経路
SE ─────────┤
AUX ────────┘
```

Ink
Mixerの責任範囲は「選択されたOS出力デバイスへ完成したミックスを出す」ところまで。

USBオーディオインターフェース、TRRS、Bluetooth等の物理接続は機器依存であり、Ink
Mixer本体と分離して扱う。

**注意:**
一般的なBluetooth接続が必ずスマートフォンのマイク入力として利用できるとは仮定しない。

### B. PC / Virtual Output Mode

Twitch / YouTube / OBS 等。

``` text
MIC/BGM/SE/AUX
       │
       v
   Ink Mixer
       │
       v
 Virtual Audio Output
       │
       v
   OBS / App
```

### C. Generic Mixer Mode

特定配信サービスに依存しない通常のリアルタイムミキサー。

------------------------------------------------------------------------

# 2. Product Principles

1.  **OSS / 無料を前提とする。**
2.  **最初から完成品を狙わない。**
3.  **小さく公開し、高頻度で更新する。**
4.  **初心者UIと内部の高度な構造を分離する。**
5.  **Audio Graphを中心にし、固定DSPチェーンへハードコードしない。**
6.  **OS固有処理をCoreから分離する。**
7.  **将来の外部プラグイン追加を妨げない。**
8.  **Built-in
    DSPは基本機能として維持し、外部プラグイン必須にはしない。**
9.  **リアルタイム音声スレッドでは、ブロッキングI/O・重い確保・予測不能な処理を避ける。**
10. **「便利だから今入れる」より「後から安全に追加できる境界を今作る」を優先する。**

------------------------------------------------------------------------

# 3. 初期対応OS

  OS              方針
  --------------- --------------------------------------------
  Windows 10/11   正式対象
  Linux           正式対象
  macOS           初期は非公式・未検証。コミュニティ協力対象

想定バックエンド:

-   Windows: WASAPI
-   Linux: PipeWire
-   macOS: 将来 CoreAudio

初期実装で共通I/Oライブラリを利用する場合でも、上位層は特定ライブラリへ直接依存させない。

------------------------------------------------------------------------

# 4. 全体アーキテクチャ

``` text
┌──────────────────────────────────────────────────────┐
│                    Ink Mixer UI                      │
│ Simple Mode / Advanced / Sound Pad / Trigger / etc. │
└───────────────────────┬──────────────────────────────┘
                        │ Commands / State
┌───────────────────────v──────────────────────────────┐
│                    Ink Mixer Core                    │
│                                                      │
│  Audio Graph    Event Engine    Asset Library        │
│  State/Config   Plugin Layer    Public API           │
└──────────┬──────────────┬──────────────┬──────────────┘
           │              │              │
     ┌─────v─────┐  ┌────v─────┐  ┌────v─────────┐
     │ Built-in  │  │ External │  │ AI / Speech │
     │ DSP Nodes │  │ Plugins  │  │ Components  │
     └───────────┘  └──────────┘  └──────────────┘
           │
┌──────────v───────────────────────────────────────────┐
│                Audio Backend Abstraction             │
├──────────────────────┬───────────────────────────────┤
│ WASAPI / Windows     │ PipeWire / Linux             │
└──────────────────────┴───────────────────────────────┘
```

------------------------------------------------------------------------

# 5. Core Language

## 5.1 基本方針

**RustをInk Mixerのプラットフォーム/Core言語とする。**

PythonをCoreにはしない。

Pythonは将来的に以下へ利用可能。

-   AI処理
-   スクリプティング
-   開発用ツール
-   外部Automation
-   Python Plugin SDK

## 5.2 RustをCoreにする理由

-   リアルタイム処理に適した制御
-   メモリ安全性
-   クロスプラットフォーム
-   ネイティブプラグインとの接続
-   長期的にAudio Graphを成長させやすい

------------------------------------------------------------------------

# 6. Audio Graph

## 6.1 最重要設計

DSPを以下のように固定実装しない。

``` text
MIC -> Gate -> EQ -> Compressor -> Reverb -> Mixer
```

代わりに、すべてをNodeとして扱う。

``` text
AudioSource
    │
    v
AudioNode
    │
    v
AudioNode
    │
    v
MixerNode
    │
    v
AudioSink
```

これにより将来、

``` text
MIC
 │
 ├─ BuiltInNoiseSuppressionNode
 ├─ BuiltInGateNode
 ├─ LV2PluginNode
 ├─ CompressorNode
 └─ MixerNode
```

のように外部プラグインを途中へ挿入できる。

## 6.2 基本概念

### AudioSource

音声をGraphへ供給する。

例:

-   MicrophoneSource
-   FilePlayerSource
-   SoundPadSource
-   AuxSource
-   SystemAudioSource（将来）
-   ApplicationAudioSource（将来）

### AudioNode

音声を受け、処理し、出力する。

例:

-   GainNode
-   EQNode
-   GateNode
-   CompressorNode
-   LimiterNode
-   ReverbNode
-   PanNode
-   ExternalPluginNode

### MixerNode

複数Source/Nodeを合成する。

### AudioSink

Graphの最終出力。

例:

-   PhysicalOutputSink
-   VirtualOutputSink
-   MonitorSink
-   RecorderSink

------------------------------------------------------------------------

# 7. 最初に実装するチャンネル

``` text
MIC
BGM
SE
AUX
MASTER
```

各チャンネルは最低限、

-   Volume / Gain
-   Mute
-   Meter
-   Routing

を持てる構造にする。

Solo / Pan / Insert Effects等は後から追加可能。

------------------------------------------------------------------------

# 8. Built-in DSP

外部プラグイン対応後もBuilt-in DSPは削除しない。

## 初心者向け表示

-   ノイズ除去
-   声を整える
-   低音CUT
-   リバーブ
-   喋ったらBGMを自動で小さくする

## Advancedで露出可能な値

-   Threshold
-   Ratio
-   Attack
-   Release
-   Knee
-   Makeup Gain
-   Frequency
-   Q
-   Wet / Dry
-   その他DSP固有パラメータ

## 将来候補

-   Noise Suppression
-   Noise Gate
-   EQ
-   Compressor
-   Limiter
-   De-esser
-   Reverb
-   Gain
-   Pan
-   BGM Ducking

------------------------------------------------------------------------

# 9. Sound Library / Asset System

BGMやSEの設定にOS上の絶対パスを直接保存し続けない。

ユーザーがファイルを選択したら、Ink Mixer内部でAssetとして登録する。

``` text
Asset
├─ id
├─ type: BGM | SE
├─ display_name
├─ source_path
├─ metadata
└─ availability
```

Trigger等は、

``` text
/home/user/Music/foo.wav
```

ではなく、

``` text
asset_id = "..."
```

を参照する。

これによりファイル管理、移動検出、プリセット、将来の共有が容易になる。

------------------------------------------------------------------------

# 10. BGM Player / Sound Pad

## BGM

最低限:

-   WAV
-   MP3
-   Play / Pause / Stop
-   Volume
-   Loop

将来:

-   Playlist
-   Crossfade
-   Ducking
-   Cloud/streaming source

## Sound Pad

-   SEを複数登録
-   ボタン再生
-   個別音量
-   再生中断/重ね再生ポリシー
-   Hotkey（将来）
-   Voice Triggerから呼び出し可能

------------------------------------------------------------------------

# 11. Event / Condition / Action Engine

Voice Trigger専用機能として作らない。

汎用的な、

> **Event → Condition → Action**

エンジンとして設計する。

## Event候補

-   VoicePhraseDetected
-   WakeWordDetected
-   HotkeyPressed
-   MidiReceived
-   TimerElapsed
-   SilenceDetected
-   VolumeThresholdExceeded
-   BgmFinished
-   StreamDurationReached

## Action候補

-   PlaySound
-   PlayBgm
-   PauseBgm
-   SetVolume
-   FadeVolume
-   ToggleMute
-   SetEffectParameter
-   Wait
-   SendWebhook
-   CallPlugin
-   OBS Control（将来）

## 例

``` text
IF VoicePhraseDetected("オーマイガー")
THEN PlaySound(toccata_se)
COOLDOWN 10 sec
```

複合例:

``` text
IF VoicePhraseDetected("悲しい")
THEN
  FadeVolume(BGM, 0.25)
  PlaySound(sad_se)
  SetEffectParameter(Reverb, wet=0.6)
  Wait(5 sec)
  FadeVolume(BGM, previous)
```

### 必須安全策

Voice Triggerは原則として**MASTERではなく生MIC分岐**を解析する。

理由:

SE自身の音声を認識して再度SEを鳴らすフィードバックループを防ぐため。

Cooldownを持つ。

------------------------------------------------------------------------

# 12. Speech / Realtime Transcription

MIC入力を分岐する。

``` text
                    ┌─> DSP -> Mixer -> MASTER
MIC Capture ────────┤
                    └─> Speech Queue -> STT Engine
```

STTはリアルタイム音声処理スレッドを止めてはいけない。

候補:

-   whisper.cpp等のローカルSTT
-   将来ほかのEngineを差し替え可能

出力候補:

-   Transcript file
-   Timestamped log
-   WebSocket
-   Local API
-   Local browser page
-   OBS Browser Source向け字幕

将来:

-   翻訳
-   二言語字幕
-   配信ログ
-   要約

------------------------------------------------------------------------

# 13. External Audio Plugin System

## 13.1 「プラグイン」とは何か

ここでいうオーディオプラグインは、Ink
Mixer本体とは別に存在する音声処理モジュールである。

概念的には、

``` text
Audio In
   │
   v
[ Plugin ]
   │
   v
Audio Out
```

となる。

プラグインによって、

-   EQ
-   Compressor
-   Reverb
-   Delay
-   Distortion
-   Noise Reduction
-   Synthesizer
-   Meter
-   Analyzer
-   MIDI処理

などを本体へ追加できる。

重要なのは、**Ardour専用の「部品」という意味ではない**こと。

Ardourは複数のプラグイン規格を読み込める「Host」であり、Ink
Mixerも将来Hostになれる。

------------------------------------------------------------------------

# 14. 現在確認できているLV2資産

MODICIA環境で次を確認済み。

``` text
~/.local/lib/lv2/
```

-   LV2 bundle数: **131**
-   合計容量: **約300 MB**

確認できた代表例:

``` text
gx_compressor.lv2
gx_mbcompressor.lv2
gx_reverb.lv2
gx_mbreverb.lv2
gx_graphiceq.lv2
gx_chorus.lv2
gx_flanger.lv2
gx_delay.lv2
gx_phaser.lv2

a-eq.lv2
a-delay.lv2
a-reverb.lv2
a-comp.lv2

nrepellent.lv2

Harrison.lv2
meters.lv2
spectra.lv2
drumgizmo.lv2
yoshimi.lv2
a-fluidsynth.lv2
```

この集合には「エフェクト」だけでなく、シンセ、メーター、MIDI関連、LV2仕様サポート用bundle等が混在している可能性がある。

**131 bundle = 131個すべてがInk
Mixerのマイク用エフェクト、という意味ではない。**

実装時にはPlugin Scannerで分類すること。

------------------------------------------------------------------------

# 15. LV2とは何か

LV2はLinuxオーディオ環境で広く使われるプラグイン規格の一つ。

典型的な `.lv2`
は単一バイナリファイルではなく**bundleディレクトリ**である。

概念例:

``` text
example.lv2/
├─ manifest.ttl
├─ plugin.ttl
├─ plugin.so
└─ ui.so
```

実際の構成はプラグインごとに異なる。

TTL/RDFメタデータから、

-   Plugin URI
-   名前
-   Port
-   Audio Input / Output
-   Control
-   MIDI/Atom
-   Feature requirements
-   UI情報

などを取得する。

したがってInk Mixerは単に `.so` をdlopenするだけでは不十分。

LV2 Hostとして、metadata discovery、feature negotiation、port
connection、instance lifecycle等を扱う必要がある。

------------------------------------------------------------------------

# 16. Plugin Host Architecture

将来:

``` text
PluginManager
├─ PluginScanner
├─ PluginRegistry
├─ PluginDescriptor
├─ PluginInstance
├─ PluginState
└─ PluginUIBridge
```

Audio Graph側:

``` text
AudioNode
├─ BuiltInEffectNode
└─ ExternalPluginNode
      ├─ LV2PluginNode
      ├─ VST3PluginNode   [future]
      └─ OtherPluginNode  [future]
```

重要:

**Audio GraphはLV2そのものへ依存しない。**

LV2はExternalPluginNodeを実装する一方式に過ぎない。

------------------------------------------------------------------------

# 17. Plugin Scanner

Scannerは指定された検索パスからプラグインを発見し、Registryへ登録する。

Linux初期候補:

``` text
~/.lv2
~/.local/lib/lv2
/usr/local/lib/lv2
/usr/lib/lv2
```

環境・ディストリビューションにより場所は異なり得るため、ユーザー追加パスも許可する。

## Registryに保持する情報

``` text
PluginDescriptor
{
    id
    standard
    uri
    name
    vendor
    version
    category
    audio_inputs
    audio_outputs
    control_ports
    midi_support
    has_custom_ui
    required_features
    supported
    compatibility_status
}
```

------------------------------------------------------------------------

# 18. Plugin分類

Scanner後、最低限以下へ分類できるようにする。

``` text
Effect
Instrument
Analyzer
Utility
MIDI
Unsupported
Unknown
```

さらにEffectを可能なら、

``` text
EQ
Dynamics
Reverb
Delay
Modulation
Distortion
Noise
Filter
Other
```

へ分類する。

自動分類を絶対視しない。

------------------------------------------------------------------------

# 19. Plugin UI

外部プラグインには、

1.  独自GUIを持つ
2.  GUIを持たずパラメータだけ公開する
3.  GUIがInk Mixerへ埋め込みにくい

ケースがある。

初期LV2 Hostでは**独自GUI埋め込みを必須にしない**。

まずInk Mixer側でGeneric Parameter UIを生成できることを優先する。

例:

``` text
Plugin: Example Compressor

Threshold   [-18.0 dB] ─────●────
Ratio       [4.0:1]    ───●──────
Attack      [10 ms]     ──●───────
Release     [120 ms]    ─────●────
Bypass      [ ]
```

独自GUI対応は後から追加可能。

------------------------------------------------------------------------

# 20. Plugin Preset / State

PluginInstanceごとに状態を保存できる抽象化を用意する。

``` text
PluginState
├─ plugin_id
├─ parameter_values
├─ opaque_state
└─ preset_name
```

外部プラグイン固有stateがある場合でも、Ink
MixerのConfig全体へ直接混ぜない。

------------------------------------------------------------------------

# 21. Plugin Safety

外部プラグインはInk Mixer本体が管理できないコードである。

想定リスク:

-   クラッシュ
-   ハング
-   異常なCPU使用
-   非Realtime-safe処理
-   巨大レイテンシ
-   不正なmetadata
-   非対応feature要求
-   UIクラッシュ

したがって、

``` text
unknown plugin != trusted built-in DSP
```

として扱う。

将来的にはPlugin Scannerの別プロセス化、blacklist、crash marker、safe
mode等を検討する。

初期Hostで完全なプロセス分離まで実装しなくてもよいが、**PluginManager境界を作り、Core全体へ直接ロード処理を散らさない。**

------------------------------------------------------------------------

# 22. Plugin Compatibility Test

MODICIAに存在する131 LV2 bundlesを実機テストセットとして利用する。

将来的に次のようなレポートを生成できるようにする。

``` text
LV2 Scan Report

Bundles found:       131
Plugins discovered:  xxx
Supported:           xxx
Loaded successfully: xxx
Unsupported:         xxx
Crashed:             xxx
UI available:        xxx
Generic UI only:     xxx
```

bundle数とplugin数は同一とは限らないため、別々に記録する。

------------------------------------------------------------------------

# 23. ライセンス

Ink MixerがOSSであっても、外部プラグインのライセンスはそれぞれ異なる。

区別すること:

1.  Ink Mixerがユーザー環境に既にあるプラグインをHostとしてロードする
2.  Ink Mixer配布物へプラグイン本体を同梱する
3.  外部プロジェクトのソースコードをInk Mixerへコピーする

これらは同じではない。

**同梱・コード流用を行う前に各ライセンスを個別確認する。**

ライセンス不明のプラグインをInk Mixer配布物へ勝手に含めない。

------------------------------------------------------------------------

# 24. VST3 / LADSPA等

現在の131 bundleから明確に確認できている主要資産はLV2。

したがって初期External Plugin研究対象はLV2を優先する。

ただし抽象化は、

``` text
PluginStandard
├─ LV2
├─ VST3
├─ LADSPA
└─ Future
```

を許容する。

**VST3/LADSPA Hostをv0.x初期に実装する必要はない。**

------------------------------------------------------------------------

# 25. Realtime Thread Rules

Audio callback / realtime processing pathでは原則禁止:

-   filesystem access
-   network access
-   sleep
-   mutex待ちの長時間ブロック
-   logging大量出力
-   UI処理
-   STT inference
-   plugin scan
-   設定ファイル保存
-   予測不能なメモリアロケーション

制御/UI側から音声側へ値を渡す場合は、Realtime-safeな通信方式を用いる。

設計候補:

-   lock-free queue
-   atomic parameter
-   preallocated buffer
-   message passing

具体方式は実装時にベンチマークして決定する。

------------------------------------------------------------------------

# 26. Sample Rate / Buffer

48 kHzを主要ターゲットとするが、Coreを48 kHz固定値へハードコードしない。

考慮:

-   44.1 kHz
-   48 kHz
-   device mismatch
-   buffer size
-   resampling
-   channel count
-   mono/stereo
-   device disconnect/reconnect
-   underrun/overrun

これらはAIコード生成より実機検証が重要。

------------------------------------------------------------------------

# 27. Device Abstraction

概念インターフェース:

``` text
AudioBackend
├─ enumerate_input_devices()
├─ enumerate_output_devices()
├─ open_input()
├─ open_output()
├─ default_input()
├─ default_output()
└─ device_events()
```

Windows固有/WASAPI固有コードをUIへ書かない。

PipeWire固有コードも同様。

------------------------------------------------------------------------

# 28. Settings

想定セクション:

``` text
Audio
Streaming
Voice
Trigger
Sound Library
Plugins
Advanced
```

保存対象:

-   selected devices
-   volumes
-   channel state
-   DSP parameters
-   assets
-   triggers
-   plugin search paths
-   plugin chain
-   mode/preset

設定フォーマットはversionを持たせ、migration可能にする。

------------------------------------------------------------------------

# 29. UI方針

## Simple

配信者が音響工学を知らなくても使える。

例:

``` text
MIC      [■■■■■■□□]  🔇  ⚙
BGM      [■■■■□□□□]  🔇  ⚙
SE       [■■■■■■□□]  🔇  ⚙
AUX      [■■■■□□□□]  🔇  ⚙

MASTER   [■■■■■■■□]
```

MIC Settings:

``` text
☑ ノイズ除去
☑ 声を整える
☑ 低音CUT
☐ リバーブ
```

## Advanced

-   Graph/chain
-   詳細DSP
-   External Plugins
-   Routing
-   Buffer/sample rate
-   diagnostics

初心者画面へLV2 URIやAttack/Release等を最初から露出しない。

------------------------------------------------------------------------

# 30. Service Presets

UI/設定をサービス別に簡略化する。

``` text
IRIAM
REALITY
OBS / Twitch
OBS / YouTube
Generic
```

PresetはAudio Engineそのものを別実装しない。

同じCoreに対する初期設定・表示の違いとして扱う。

------------------------------------------------------------------------

# 31. Repository Structure案

``` text
ink-mixer/
├─ crates/
│  ├─ ink-core/
│  ├─ ink-audio/
│  ├─ ink-events/
│  ├─ ink-assets/
│  ├─ ink-dsp/
│  ├─ ink-backend/
│  ├─ ink-plugin-api/
│  └─ ink-plugin-lv2/       # 後期追加
│
├─ app/
│  └─ ink-mixer/
│
├─ assets/
├─ examples/
├─ tests/
├─ docs/
│  ├─ ARCHITECTURE.md
│  ├─ AUDIO_GRAPH.md
│  ├─ PLUGINS.md
│  └─ REALTIME_RULES.md
│
├─ Cargo.toml
├─ README.md
├─ ROADMAP.md
└─ LICENSE
```

実装開始時にcrateを細分化しすぎないこと。

必要になるまで統合していてもよい。

この構造は責務境界を示すための論理設計でもある。

------------------------------------------------------------------------

# 32. Public API / Plugin API

将来的にInk Mixer自体の拡張APIも提供する。

外部Audio Plugin規格とInk Mixer Plugin APIは別物。

## Audio Plugin

音声そのものを処理。

例:

``` text
LV2 Compressor
```

## Ink Mixer Plugin

Ink Mixerのイベント・操作・外部連携を拡張。

例:

``` text
Discord Integration
OBS Integration
MIDI Controller
Auto BGM
Custom Automation
```

将来的な境界:

``` text
Ink Mixer Plugin API
├─ subscribe_event()
├─ get_channel()
├─ set_volume()
├─ play_asset()
├─ execute_action()
└─ register_action()
```

Native API、Python SDK、WebSocket/API等は将来比較する。

------------------------------------------------------------------------

# 33. 開発フェーズ

実装の順番は ROADMAP.md の「公開とお披露目の計画」に従い、Phase 5・6 のうちボイストリガーに必要な最小限を、Phase 3・4 より先に作る。

## Phase 0 --- Repository / Skeleton

目的: 公開可能な骨格。

-   Rust workspace
-   App起動
-   AudioBackend interface
-   config skeleton
-   CI
-   README / ROADMAP
-   logging

### 完了条件

アプリがWindows/Linuxで起動し、音声デバイス一覧を取得する土台がある。

------------------------------------------------------------------------

## Phase 1 --- Minimum Audio Path

``` text
MIC -> Gain -> MASTER -> Output
```

-   input device
-   output device
-   GainNode
-   MixerNode
-   meter
-   master volume

### 完了条件

マイク音声を選択した出力へリアルタイム送出できる。

------------------------------------------------------------------------

## Phase 2 --- BGM / SE

-   WAV/MP3
-   BGM player
-   Sound Pad
-   Sound Library
-   Loop
-   volume
-   basic UI

### 完了条件

MIC + BGM + SEを同時ミックスして出力できる。

ここを最初のPublic Alpha候補とする。

------------------------------------------------------------------------

## Phase 3 --- Built-in DSP

-   Gate
-   EQ
-   Compressor
-   Limiter
-   Reverb
-   Noise suppression候補
-   Ducking

### 完了条件

初心者が外部プラグイン無しで配信用の基本音声処理を行える。

------------------------------------------------------------------------

## Phase 4 --- Linux / Windows Backend強化

-   WASAPI固有対応
-   PipeWire固有対応
-   reconnect
-   device events
-   routing
-   virtual output連携

------------------------------------------------------------------------

## Phase 5 --- STT

-   MIC branch
-   realtime transcription worker
-   transcript
-   OBS caption output

------------------------------------------------------------------------

## Phase 6 --- Event / Trigger

-   Voice phrase
-   cooldown
-   PlaySound action
-   volume action
-   multi-action
-   Trigger UI

------------------------------------------------------------------------

## Phase 7 --- Plugin Foundation

**このPhase以前でもAudio Graph側の抽象化は維持する。**

-   PluginManager
-   PluginDescriptor
-   ExternalPluginNode interface
-   Plugin registry
-   scanner boundary

まだLV2をロードしなくてもよい。

------------------------------------------------------------------------

## Phase 8 --- LV2 Host Alpha

-   LV2 discovery
-   metadata parsing
-   compatible audio-effect filtering
-   instantiate
-   connect ports
-   process audio
-   generic parameter UI
-   bypass
-   state/preset基礎
-   compatibility report

### 最初の目標

131 bundlesすべてを「鳴らす」ことではない。

まず、

``` text
LV2を検出できる
↓
対応可能なEffectを分類できる
↓
1つの単純なLV2 effectを安全にGraphへ挿入できる
↓
複数Pluginへ拡大
```

と進める。

------------------------------------------------------------------------

## Phase 9 --- Plugin Ecosystem

-   custom LV2 UI
-   crash recovery
-   blacklist
-   latency handling
-   VST3 investigation
-   LADSPA investigation
-   Ink Mixer Plugin SDK
-   LV2 プラグインの同梱（プラグインごとのライセンス確認と、ソースの入手方法の提示が前提）
-   プラグインをダウンロードできる仕組み（検討）

------------------------------------------------------------------------

# 34. 初期版で「やらない」こと

v0.0.x / 最初のAlphaで以下を完成させようとしない。

-   131 LV2完全対応
-   VST3 Host
-   自作Virtual Audio Driver
-   macOS正式対応
-   DAW並みの編集機能
-   完全なPlugin sandbox
-   MIDI全面対応
-   高度な録音編集
-   全配信サービス固有連携
-   外部Plugin独自GUI完全対応

これらを最初から実装するとPublic Alphaが遅れる。

------------------------------------------------------------------------

# 35. AI開発者への実装ルール

AIエージェントは変更前に以下を確認すること。

1.  その変更はAudio Graphの抽象化を壊さないか。
2.  OS固有コードがCore/UIへ漏れていないか。
3.  Realtime threadでブロッキング処理をしていないか。
4.  Built-in DSPとExternal Pluginを混同していないか。
5.  UIがEngineの実装詳細へ直接依存していないか。
6.  設定に生の一時パスを乱用していないか。
7.  Event EngineをVoice Trigger専用にしていないか。
8.  まだ必要のない巨大機能を先回り実装していないか。
9.  新規依存ライブラリのライセンスを確認したか。
10. Windows/Linux双方への影響を記録したか。

------------------------------------------------------------------------

# 36. Issue粒度

大きな「Mixerを作る」Issueにしない。

例:

``` text
#1 Create Rust workspace
#2 Define AudioBackend trait
#3 Enumerate input devices
#4 Enumerate output devices
#5 Implement microphone capture
#6 Implement output stream
#7 Implement GainNode
#8 Implement MixerNode
#9 Add master volume
#10 Add WAV playback
#11 Add MP3 playback
#12 Add Sound Pad
#13 Add settings persistence
#14 Add GateNode
...
```

AIエージェントが一回の変更で責務を広げすぎないようにする。

------------------------------------------------------------------------

# 37. テスト戦略

## Unit

-   Gain
-   mixing
-   parameter conversion
-   config migration
-   Event matching
-   Trigger cooldown
-   Asset registry

## Integration

-   Source -\> Graph -\> Sink
-   file playback
-   device switching
-   plugin scan
-   plugin instantiate

## Hardware / Manual

必須。

-   actual microphone
-   USB audio device
-   44.1/48k mismatch
-   unplug/replug
-   Bluetooth where relevant
-   long-running stream
-   CPU load
-   buffer underrun
-   Windows
-   Linux

音声アプリは「コンパイル成功 = 動作成功」ではない。

------------------------------------------------------------------------

# 38. Diagnostics

ユーザーが不具合報告しやすいように診断情報を用意する。

``` text
Ink Mixer Diagnostics
Version:
OS:
Backend:
Input:
Output:
Sample Rate:
Buffer:
XRuns:
Graph:
Loaded Plugins:
Plugin Failures:
```

個人情報や不要なファイルパスを公開ログへ出しすぎない。

------------------------------------------------------------------------

# 39. 重要な未決事項

以下は設計上まだ確定させない。

-   Windows virtual output方式
-   LV2 Host libraryを利用するか独自実装範囲
-   Noise Suppression engine
-   Plugin process isolation方式
-   Python SDK方式
-   VST3対応時期
-   macOS対応時期

AIエージェントはこれらを「決定済み」と扱わないこと。

決定時にはADR（Architecture Decision Record）を残す。

決定済みの事項は `docs/adr/` と AGENTS.md §6 を参照する（音声I/Oライブラリ: ADR-0002、GUI framework: ADR-0004、Audio Graph と値の受け渡し: ADR-0007）。

------------------------------------------------------------------------

# 40. ADR例

``` text
docs/adr/
├─ 0001-rust-core.md
├─ 0002-audio-backend.md
├─ 0003-crate-dependency-direction.md
├─ 0004-gui-framework.md
├─ 0005-license.md
├─ 0006-japanese-font.md
└─ 0007-audio-graph-and-realtime-communication.md
```

今後の候補（番号は起票時に決める）: LV2 Host library、Plugin process isolation など。

各ADR:

``` text
# Decision
# Context
# Options
# Chosen
# Why
# Consequences
```

------------------------------------------------------------------------

# 41. Ink Mixerの「二種類の拡張性」

設計上、ここを混同しない。

## A. Audio Processing Extension

``` text
LV2 / VST3 / LADSPA
```

音そのものを変える。

## B. Application Extension

``` text
Ink Mixer Plugin / API / Automation
```

アプリの行動を増やす。

この二つが共存することで、

``` text
Voice
  ↓
Speech Event
  ↓
Automation
  ↓
Load/Control Audio Effect
  ↓
Mixer
```

のような高度な配信自動化へ発展できる。

------------------------------------------------------------------------

# 42. 最終的な設計イメージ

``` text
                         ┌───────────────┐
                         │     UI        │
                         └───────┬───────┘
                                 │
                ┌────────────────v────────────────┐
                │          Ink Mixer Core          │
                │                                  │
                │ Audio Graph     Event Engine     │
                │ Asset Library   Plugin Manager   │
                │ State           Public API       │
                └───────┬───────────────┬──────────┘
                        │               │
          ┌─────────────v───┐     ┌────v────────────┐
          │ Audio Processing│     │ Automation / AI │
          │                 │     │                 │
          │ Built-in DSP    │     │ STT             │
          │ LV2             │     │ Voice Trigger   │
          │ VST3 (future)   │     │ Webhook         │
          └─────────┬───────┘     └─────────────────┘
                    │
          ┌─────────v──────────────────────────────┐
          │          Audio Backend Layer           │
          │ WASAPI / PipeWire / CoreAudio future  │
          └────────────────────────────────────────┘
```

------------------------------------------------------------------------

# 43. 最重要事項

Ink Mixerの初期成功条件は、

> **「高機能であること」ではなく、「小さく動き、壊さず拡張できること」。**

131個のLV2資産が見つかったことは、131個の機能を今すぐ搭載すべきという意味ではない。

意味するのは、

> **Ink
> Mixer自身がすべての音響処理を再発明しなくても、将来既存のオーディオエコシステムを利用できる可能性が高い**

ということである。

したがって最初の実装では、

``` text
MIC -> Gain -> Mixer -> Output
```

を確実に成立させる。

その時点から、

``` text
AudioNode
ExternalPluginNode
AudioBackend
Event
Action
```

を後から自然に追加できる責務境界を守る。

**LV2 Hostを今すぐ作る必要はない。\
LV2 Hostを後から作れない構造にしてはいけない。**

これをInk Mixerの初期アーキテクチャ原則とする。
