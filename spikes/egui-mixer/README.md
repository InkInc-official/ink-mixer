# egui mixer spike（GitHub #21）

Phase 1 の GUI フレームワークを決めるための試作。**main にはマージしない**（ブランチ `spike/egui` のみ）。

- egui / eframe 0.36.2、描画は glow（OpenGL）を明示指定
- MIC / BGM / SE / AUX / MASTER の5チャンネル。メーター・音量スライダー・ミュート
- メーターはダミーデータで 60fps 更新。「メーター停止」で再描画の要求を止める
- 日本語フォントは OS のものを実行時に読む（Linux: Noto Sans CJK JP、Windows: 游ゴシック → メイリオ）。使ったフォントは起動時に stderr に出る
- ルートの workspace には入っていない（この crate だけの `[workspace]` を持つ）。ink-core / ink-backend には依存しない

Rust 1.95 以上が必要（egui / eframe 0.36.2 の MSRV）。

## 創作PC（Linux）

```bash
cd spikes/egui-mixer
cargo run --release
```

測定（約6分。メモリ・CPU を3回表示する）:

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

```powershell
rustup update
cd C:\Users\loff\ink-mixer
git fetch origin
git switch spike/egui
cd spikes\egui-mixer
cargo run --release
```

確認すること:

- 起動する
- 日本語（「ノイズ除去」「声を整える」「マイク」「効果音」など）が表示される。使ったフォントはコンソールに `using Japanese font: ...` と出る
- 5チャンネルとメーターの表示が崩れない。ウィンドウの大きさを変えても崩れない

メモリ（参考）。起動から10秒後と5分後に、別の PowerShell で:

```powershell
Get-Process egui-mixer | Select-Object Name, @{n='WorkingSet(MB)';e={[math]::Round($_.WorkingSet64/1MB,1)}}, @{n='Private(MB)';e={[math]::Round($_.PrivateMemorySize64/1MB,1)}}, CPU
```

> **注意:** `CPU` 列は CPU 使用率（%）ではなく、起動してから使った CPU 時間の**累計秒数**。
> 使用率の目安にするなら、2回測った差を経過秒数で割る（例: 5分後と5分10秒後の差 ÷ 10秒）。
