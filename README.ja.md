# bambu-rs

[![crates.io](https://img.shields.io/crates/v/bambu-rs.svg)](https://crates.io/crates/bambu-rs)
[![docs.rs](https://img.shields.io/docsrs/bambu-rs)](https://docs.rs/bambu-rs)
[![CI](https://github.com/sksat/bambu-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/sksat/bambu-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English](README.md) · **日本語**

[Bambu Lab](https://bambulab.com/) の 3D プリンターを LAN 経由で監視・操作するための
コマンドラインツールとライブラリです。
人間が手で動かすのにも、AI エージェントに任せるのにも使えます。

既存の Bambu 系ライブラリに依存も参照もせず、プロトコルのドキュメント
（[OpenBambuAPI](https://github.com/Doridian/OpenBambuAPI)）と実機の観測だけから実装した、
**クリーンルーム実装**です。
観測したプロトコルの事実は [docs/protocol.md](docs/protocol.md) にまとめてあります。
自動化を安全にする仕組みは、次の 4 つです。
機械可読な JSON（`--json`）、意味を持つ終了コード、物理動作すべてにかかる
`--confirm`/`--dry-run` のガード、そして、コマンドが通ったことではなく、プリンター自身の
レポートを読み直して意図どおりの状態になったかで成功を判断する仕組み（*verify-by-reread*）。

`bambu` コマンド（CLI）は、AI エージェントから使いやすいインターフェースを意識して設計してあります。

> ⚠️ **手元の A1 mini の LAN mode でしか検証していません**。
> 他の機種や firmware は未検証なので、あくまで best-effort と考えてください。
> プリンターとは直接やり取りし、クラウドは経由しません。
> 印刷の制御には、プリンターを **LAN-only + Developer Mode** にする必要があります。
> 状態を確認するだけなら、この設定は要りません。

## Install

```bash
# ビルド済みバイナリ（dashboard 同梱）
cargo binstall bambu-rs

# crates.io からビルド
cargo install bambu-rs

# dashboard 付き（web UI のビルドに node と pnpm が要る）
cargo install bambu-rs --features dashboard
```

Linux、macOS、Windows 向けのビルド済みバイナリは、各
[リリース](https://github.com/sksat/bambu-rs/releases) にも用意してあります。

## 使い方

```bash
# 初回: プリンターを登録（8 桁の LAN アクセスコードはプリンターの画面に表示される）
bambu config add --printer a1 --ip 192.0.2.50 --serial <SERIAL> \
  --access-code <CODE> --model a1mini
bambu config list                   # 保存済みプロファイル一覧
bambu config show                   # 現在のプロファイル（アクセスコードは伏せて表示）

# 状態を読む。一発の JSON か、--watch で実行中の印刷を終了まで追う
bambu status --json
bambu status --watch
bambu info                          # firmware と、このプリンターの解決済み capability
bambu hms                           # HMS（健全性・保守）アラートをデコード

# プリンター上のファイル（FTPS）。ls / upload / download / rm
bambu file ls
bambu file upload model.gcode.3mf --dest /cache

# キャリブレーション。ルーチン未指定なら全部（bed level, vibration, motor noise）
bambu calibrate --confirm --watch

# 印刷開始。解決後のプランをプレビューし、ガード付きで実行して監視する
bambu job start /cache/model.gcode.3mf --plate 1 --dry-run        # → md5 / plate / AMS マップ
bambu job start /cache/model.gcode.3mf --plate 1 --ams-map 0 \
  --expect-md5 <md5> --expect-plate 1 --confirm --watch

# ローカルファイルをアップロードしてそのまま印刷
bambu job start ./model.gcode.3mf --upload --plate 1 --confirm --watch
```

状態の読み取りは、接続して snapshot を取るだけで、フラグも事前確認も要りません。
一方、物理動作（`job start/pause/resume/stop`、`temp`、`light`、`gcode`、`ams`、`calibrate`）には
`--confirm` が必須で、まずプリンター自身の状態
（idle か、エラーがないか、期待したファイルとプレートか）を確認します。
`--json` を付けると出力は機械可読になり、終了コードが「成功 / 未確認 / 拒否 / busy」を
区別するので、スクリプトやエージェントが分岐できます。

## スライス

`bambu-rs` 自体はスライス機能を提供しません。
**Bambu Studio / OrcaSlicer の CLI に委譲**してスライス済みの `.gcode.3mf` を作り、
それをアップロードして印刷します。

```bash
# 1. モデルを .gcode.3mf にスライス（Bambu Studio / OrcaSlicer CLI）
bambu-studio --slice 1 \
  --load-settings "machine.json;process.json" \
  --load-filaments "filament.json" \
  --allow-newer-file \
  --export-3mf out.gcode.3mf  model.3mf

# 2. アップロードし、プランをプレビューしてから印刷する（検査したものと完全一致を保証）
bambu file upload out.gcode.3mf --dest /cache
bambu job start /cache/out.gcode.3mf --plate 1 --dry-run          # → inspection.gcode_md5
bambu job start /cache/out.gcode.3mf --plate 1 --ams-map 0 \
  --expect-md5 <that-md5> --expect-plate 1 --confirm --watch
```

詳細（フラグ、AMS マッピング、外部スプール、`--dry-run`）は
[docs/slicing.md](docs/slicing.md) にあります。

## Dashboard

`bambu serve` は小さなローカルサーバーを立ち上げます。
dashboard feature が有効なら、CLI に組み込まれた Web dashboard を使えます（無効でも REST API は使えます）。
スマホやブラウザから、プリンターの状態、温度、AMS、ライブカメラ、ワンクリックの
クリーン timelapse 取得、よく使う操作までをライブで扱えます。
すべて同じ 1 本の LAN 接続で動きます（読み取りはオープン、制御は任意のパスワードでゲート）。

<p align="center">
  <img src="assets/dashboard-demo.gif" alt="bambu serve Web dashboard" width="600">
</p>

## Timelapse

内蔵カメラによる公式の timelapse はそのまま扱えます（`bambu timelapse enable/disable` で録画の切り替え、`job start --timelapse` で印刷ごとの指定、`bambu timelapse get` で録画済み動画の取得）。
それに加えて、`bambu-rs` では**外部**カメラを使って独自の timelapse を撮れます。
印刷の layer イベントに合わせて 1 層 1 枚を撮るので、内蔵カメラが無い、または壊れているプリンターでも使えます。

<p align="center">
  <img src="assets/timelapse-demo.gif" alt="外部カメラの timelapse。1 layer につき park フレーム 1 枚" width="480">
</p>

撮り方は 2 通りです。
`bambu timelapse capture` に任意のツールを渡せば、各層で 1 フレーム取得します（`--` の後ろのコマンドはシェルを介さず argv として実行し、`{frame}`/`{layer}`/`{outdir}` が差し込まれます）。

```bash
bambu timelapse capture --out-dir ./tl -- fswebcam -r 1280x720 {frame}

# USB カメラを µStreamer で HTTP 配信している場合（その /snapshot エンドポイント）
bambu timelapse capture --out-dir ./tl -- \
  curl -s -m 15 -o {frame} "http://$USTREAMER_HOST/snapshot"

# IP カメラ（例: atomcam_tools を入れた ATOM Cam）を素の HTTP で
bambu timelapse capture --out-dir ./tl -- \
  curl -s -m 15 -o {frame} "http://$ATOMCAM_HOST/cgi-bin/get_jpeg.cgi"
```

もう 1 つは、上のような滑らかな仕上がりにする方法です。
`bambu timelapse park` はカメラの MJPEG ストリームを読み、各層でヘッドが造形物から退いた **park** フレームをオンデバイスで選ぶので、取得のタイミングを自分で合わせる必要がありません。
`bambu serve` の dashboard は、これをワンクリック取得としてまとめています。

```bash
bambu timelapse park http://<host>/stream --config tuning.json --out ./tl --assemble out.mp4
```

どちらも、取得に失敗したフレームはスキップし、最後にフレームを繋ぐ `ffmpeg` の例を表示します。

## その他のコマンド

```bash
bambu speed standard                 # silent | standard | sport | ludicrous
bambu light on --node chamber        # on | off  ·  --node chamber | work
bambu gcode "G28"                    # 上限超の温度や低温での押し出しは --force なしでは拒否
bambu ams resume                     # resume | reset | pause | change | set-filament | settings
```

物理動作には `--confirm` が必要です。
`ams change`/`set-filament` は `--dry-run` にも対応します。
より深いスライサー統合は今後の予定です。

## ライブラリ

プロトコルや安全機構は、再利用可能な Rust crate として実装してあります。
`bambu` コマンド（CLI）も `bambu serve` の Web dashboard も、どちらもその利用者にすぎません。

```toml
[dependencies]
bambu-rs = { version = "0.1", default-features = false }   # ライブラリのみ。CLI/server の依存は引かない
```

```rust
use bambu_rs::client::LanMqttClient;
use bambu_rs::config::ResolvedTarget;
use bambu_rs::core::command::{Command, ProjectFile};
use bambu_rs::core::model::Model;
use bambu_rs::core::session::CommandOutcome;

let client = LanMqttClient::new(ResolvedTarget {
    ip: "192.0.2.50".into(),
    serial: "<SERIAL>".into(),
    access_code: "<CODE>".into(),
    model: Model::A1Mini,
});

// フル calibration。send_and_verify は、publish の成功ではなく、プリンター自身の
// report を読み直して、実際に始まったことを確認する。
let outcome = client.send_and_verify(&Command::Calibration {
    bed_level: true,
    vibration: true,
    motor_noise: true,
})?;
assert_eq!(outcome, CommandOutcome::Verified);

// プリンターにアップロード済みのスライス済み 3MF を印刷（plate 1）。
let job = ProjectFile::new("ftp:///cache/model.gcode.3mf", 1, "my-print");
client.send_and_verify(&Command::ProjectFile(job))?;
```

プリンターの機種や firmware によって、対応する機能や挙動は異なります。
`bambu-rs` は、その違いの一つ一つを `(model, firmware)` ごとの **capability**（機能や挙動の項目）として持ちます。
たとえば、カメラの転送方式（A1 は生の TCP で JPEG、X 系は RTSP）、印刷中に温度コマンドが効くかどうか（新しい firmware は黙って無視する）、状態が一括 push で来るか差分で来るか、リリースをまたいで綴りが変わったフィールド名、といったものです。
これらは接続時に一度だけ解決され、コマンド生成、report 解析、安全判定はすべてその結果を参照します。
`if firmware >= …` のような分岐をコードのあちこちに散らさずに済みます。
`bambu info` を実行すると、接続中のプリンターについて解決された capability が表示されます。

```
$ bambu info
printer: a1 (a1mini)
firmware: 01.07.02.00
registry: supported
push:     delta_only
camera:   jpeg_tcp_6000
control:  requires_developer_mode — control needs LAN-only + Developer Mode enabled
modules:
  ota        hw OTA       sw 01.07.02.00  Bambu Lab A1 mini
  esp32      hw AP05      sw 01.16.39.58
  mc         hw MC02      sw 00.01.30.10
  th         hw TH03      sw 00.00.07.72
  ams_f1/0   hw AMS_F102  sw 00.00.08.15  AMS Lite
```
