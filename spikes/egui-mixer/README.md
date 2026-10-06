# egui mixer spike（GitHub #21）

Phase 1 の GUI フレームワークを決めるための試作。**main にはマージしない**（ブランチ `spike/egui` のみ）。

- egui / eframe 0.36.2。描画は既定で glow（OpenGL）。`wgpu` feature を付けてビルドすると wgpu も選べる
- MIC / BGM / SE / AUX / MASTER の5チャンネル。メーター・音量スライダー・ミュート
- メーターはダミーデータ。「メーター停止」で再描画の要求を止める
- 日本語フォントは OS のものを実行時に読む（Linux: Noto Sans CJK JP、Windows: 游ゴシック → メイリオ）
- ルートの workspace には入っていない（この crate だけの `[workspace]` を持つ）。ink-core / ink-backend には依存しない

Rust 1.95 以上が必要（egui / eframe 0.36.2 の MSRV）。

## 起動オプション

```
egui-mixer [--renderer glow|wgpu] [--vsync on|off] [--fps 60|30|max]
既定: --renderer glow --vsync on --fps 60
```

| オプション | 意味 |
|---|---|
| `--renderer glow` | OpenGL で描画 |
| `--renderer wgpu` | wgpu で描画（Windows は DX12 のみ、Linux は Vulkan など wgpu の既定）。`--features wgpu` でビルドしたときだけ使える |
| `--vsync on` / `off` | 画面の更新（垂直同期）を待つかどうか |
| `--fps 60` / `30` | メーター動作中のフレームレートの上限（アプリ側で管理） |
| `--fps max` | 上限なし（描けるだけ描く。vsync on なら画面の更新間隔まで） |

起動するとコンソール（stderr）に次の行が出る:

```
settings: glow / vsync on / fps 60
gpu: glow vendor=... renderer=...          （wgpu のときは gpu: wgpu name=... backend=...）
using Japanese font: ...
```

画面右上にも、実際の fps と設定が出る。

> **fps の上限について:** egui の `request_repaint_after` は、指定した待ち時間から「予測フレーム時間」（約 16.7ms）を差し引く。
> そのため `request_repaint_after(16ms)` は待ち時間 0 になり、上限として働かない。この試作では、次のフレームの時刻をアプリ側で管理して上限をかけている。
> 以前の版（コミット `8a8c7a4`）の「60fps」は実際には上限が無く、今の `--fps max` と同じ動きだった。

## 創作PC（Linux）

```bash
cd spikes/egui-mixer
cargo run --release                          # glow のみ
cargo run --release --features wgpu -- --renderer wgpu
```

測定（約6分。既定の設定で、メモリ・CPU を3回表示する）:

```bash
./measure.sh          # 3回目の前に「メーター停止」を押して Enter
./measure.sh --auto   # xdotool で「メーター停止」を自動で押す
```

- メモリ: `/proc/<pid>/status` の VmRSS
- CPU: 30秒間の utime + stime の平均（1コア = 100%）

バイナリサイズとクリーンビルド時間:

```bash
cargo clean && time cargo build --release
ls -l target/release/egui-mixer
strip -o /tmp/egui-mixer.stripped target/release/egui-mixer && ls -l /tmp/egui-mixer.stripped
```

## Windows（メインPC）

### 準備

```powershell
rustup update
cd C:\Users\loff\ink-mixer
git fetch origin
git switch spike/egui
git pull
cd spikes\egui-mixer
cargo build --release --features wgpu
```

wgpu 入りでビルドした exe 1つで、5パターンすべてを測れる（`--renderer glow` を付ければ glow で動く）。

### 測るときの条件

- **AC 電源につないだ状態で測る**（バッテリー駆動だと省電力設定で CPU / GPU のクロックが変わる）
- 測定中はマウスをウィンドウの外に置く（ホバーで再描画が増える）
- ほかの重いアプリは閉じておく

### 5パターン

| # | 起動コマンド | ねらい |
|---|---|---|
| 1 | `.\target\release\egui-mixer.exe --renderer glow --vsync on --fps 60` | 基準（上限 60fps） |
| 2 | `.\target\release\egui-mixer.exe --renderer glow --vsync off --fps 60` | vsync の待ちが原因かどうか |
| 3 | `.\target\release\egui-mixer.exe --renderer wgpu --vsync on --fps 60` | OpenGL（glow）固有かどうか（DX12） |
| 4 | `.\target\release\egui-mixer.exe --renderer glow --vsync on --fps 30` | フレーム数に比例するかどうか |
| 5 | `.\target\release\egui-mixer.exe --renderer glow --vsync on --fps max` | **前回の 101.5% と同じ条件**（上限なし）。再現の確認 |

### CPU 使用率の測り方（各パターン共通）

起動から10秒以上待って、別の PowerShell で:

```powershell
$p = Get-Process egui-mixer
$c1 = $p.CPU; Start-Sleep -Seconds 30; $p.Refresh(); $c2 = $p.CPU
"{0:N1} %  (WorkingSet {1:N1} MB, Private {2:N1} MB)" -f (($c2 - $c1) / 30 * 100), ($p.WorkingSet64/1MB), ($p.PrivateMemorySize64/1MB)
```

- 1コア = 100%（創作PCの `measure.sh` と同じ基準）
- 参考: `Get-Process` の `CPU` 列は CPU 使用率（%）ではなく、起動してから使った CPU 時間の**累計秒数**。上のスクリプトは30秒間の差を使用率に直している

### 記録するもの

- パターン番号
- CPU %、WorkingSet / Private
- 画面右上の fps
- 起動時にコンソールに出た **`settings:` の行と `gpu:` の行**（ノートPCは GPU が2つある場合がある）

### 結果の見方

- 5 で 101.5% 前後が再現し、1 で大きく下がる → 上限なしで描き続けていたことが原因。fps の上限をアプリ側で管理すれば解決
- 1 でも高く、2 で下がる → vsync の待ちが CPU を回したまま待っている（空回り）
- 3 で下がる → glow（OpenGL ドライバ）固有。wgpu（DX12）なら回避できる
- 4 でほぼ半分になる → フレームごとの処理の重さそのもの

## 確認すること（表示）

- 起動する
- 日本語（「ノイズ除去」「声を整える」「マイク」「効果音」など）が表示される
- 5チャンネルとメーターの表示が崩れない。ウィンドウの大きさを変えても崩れない
