# 再現可能な比較

[English](../docs/BENCHMARKS.md) | 日本語

同じハンドラー本体を使い、オブジェクトのロードだけでなく、変更後の正しい応答までを測ります。
リポジトリの[ベンチマーク](../reknot/benches/development.rs)は、JSONと任意のMessagePackで
型付きの呼び出し全体も比較します。

## 2026-11-17の実測

以下は、同一試行内のmetadata・コンパイラー情報共有、Cargo出力先統一、容量走査の効率化、
古いビルドの段階別打ち切り、support最適化、通常DLLの背景準備より前の初回測定です。
下記のソースハッシュに対応する過去の比較基準であり、現在の実装の測定値ではありません。
当時はlogic・supportの両方をdev `opt-level=0`でコンパイルしました。

同じRyzen 7 5700X（8コア/16スレッド）で、Windows 11 Home 10.0.26300 / Rust 1.99.1と、
Ubuntu 24.04 / WSL2 / Rust 1.100.0を順に測定しました。各OS内の2方式は同じツールチェインです。

| 環境 | 通常DLL 中央値 / p95 (ms) | 二層JIT 中央値 / p95 (ms) |
| --- | --- | --- |
| Windows | 655.29 / 705.93 | 801.52 / 861.07 |
| Linux (WSL2) | 236.49 / 249.27 | 352.28 / 379.77 |

**この初回測定の小さなfixtureでは、JITは保存から応答までの全体時間を短縮しませんでした。**
JITの時間の多くは入力探索、Cargo、基盤検査を含むworker側です。所有スレッドのロード・適用時間だけから
全体の高速化は判断できません。この測定だけではworker内の各処理の因果的な寄与も分離できません。

型付き呼び出しの結果は次のとおりです。編集・ロード時間を含む上の表とは別の測定です。

| 環境 | f32要素数 | JSON 中央値 (µs) | MessagePack 中央値 (µs) | JSON / MessagePack bytes |
| --- | --- | --- | --- | --- |
| Windows | 1 | 2.54 | 2.31 | 29 / 21 |
| Windows | 64 | 8.06 | 5.20 | 431 / 338 |
| Windows | 4096 | 302.64 | 142.89 | 32447 / 20498 |
| Linux (WSL2) | 1 | 1.84 | 1.65 | 29 / 21 |
| Linux (WSL2) | 64 | 6.93 | 4.11 | 431 / 338 |
| Linux (WSL2) | 4096 | 293.12 | 137.13 | 32447 / 20498 |

[Windows生データ](../benchmarks/windows-x64.json)・[Linux生データ](../benchmarks/linux-x64.json)には全標本、
初回起動、JITの段階別時間、p95を保存しています。[測定ソースのSHA-256](../benchmarks/source-sha256.json)も付属します。
この一覧は各ファイルの同一性を記録するもので、ソース本体のスナップショットは含みません。
現在のcheckoutだけで過去の実装を復元できるわけではありません。

## 再実行する

Windows x64 MSVCまたはLinux x86_64 GNUの対応環境で、ほかの重い処理を止めて単独実行します。

```sh
cargo bench -p reknot --bench development --features jit-dev,typed-binary --locked -- --output results.json
```

rustup、Cargo、OSのリンカー、キャッシュ済みの依存が必要です。Rust製ビルダーを直接使うため、
PowerShell/Pythonのビルダーは不要です。Cargoは`reknot/` crateのディレクトリーで実行するため、別の場所へ保存する場合は絶対パスを指定します。出力には未使用のパスを指定します。独立した一時workspaceを
作り、診断用に残します。既存のプロジェクトを上書き・削除しません。

現在のベンチマークは組み込みビルダーの既定値を使い、JITのlogicは`opt-level=0`、
常駐support・依存は`opt-level=1`です。通常DLLは`opt-level=0`です。
これは異なる開発構成の比較であり、最適化条件をそろえたコンパイラーの比較ではありません。
supportの最適化だけを比較する場合は、ベンチマーク内の`LayeredDevOptions::support_optimization`を
明示し、ソースの版と設定を結果とともに記録してください。[ビルド設定](LAYERED_DEVELOPMENT.md#入力土台キャッシュ)も参照してください。

## 測定するもの

| 比較 | 測定範囲と条件 |
| --- | --- |
| 通常DLLとJIT | ソース書き込みから監視、50 msのdebounce、ビルド、検証、適用、変更後の正しい応答まで。所有スレッドは1 msごとにpoll |
| 処理内容 | 本体が同じ型付きStringハンドラー1つ。通常DLLはSDKを`cdylib`へリンクし、JITはSDKを常駐support `dylib`に置く |
| コンパイル | 同じOS・ツールチェインでdebug=0、codegen-units=1。通常DLL・logicはdev opt=0、現在のJIT support・依存はopt=1（初回測定は0）。両方のビルダーが`CARGO_INCREMENTAL=0`を指定し、二層オブジェクトは動的supportを優先 |
| 標本 | 通常DLL/JIT/JIT/通常DLLのプロセス順。各プロセスで2回の編集をウォームアップし、9回測定。方式ごとに18標本。初回起動は別記録 |
| JSONとMessagePack | 常駐する型付きechoの全体。入力clone、エンコード、ルーティング、デコード、結果確認、後始末を含み、DLLロードや外部I/Oは含まない |
| データ | tickと1・64・4096個の`f32`を持つ構造体。両形式で同じ値を使い、MessagePackもフィールド名を保持する |
| 呼び出しの標本 | 2バッチをウォームアップし、順番を交互にして15バッチ測定。小さいデータは1000回/バッチ、4096要素は100回。時間はバッチ内の1呼び出し平均 |

パーセンタイルはnearest-rankの標本を使い、偶数個の中央値は中央2標本の上側です。
生の標本と段階別の時間をJSONに保持します。どちらかが速いことをCIの合格条件にはしません。

## 結果の読み方

編集の比較は、実際に使える2つの開発構成を測っています。リンカーだけの時間ではありません。
JITは基盤の同一性検査と不変成果物の取得も行い、通常DLLも独自のビルド・監視処理を使います。
意図的に小さくしたfixtureなので、大規模ゲーム、エディター、本番サービスの性能を示すものではありません。

呼び出し時間は所有権のある直列化の測定で、ゼロコピーの結果ではありません。扱う大きさで
バイト数と応答時間の両方を比較してください。方式や形式を選ぶ際は、自分の環境とアプリでも再測定します。

## 更新中の応答遅延を測る

別の `reload_latency` ベンチマークでは、要求を処理しながらDrain、Managedの回収、Restart、
3回の連続保存を測ります。通常DLLのDrainを対照とし、64 bytesと256 KiBの状態、
即時の子タスク終了と25 msの協調的な後始末待ちも比較します。

```sh
cargo bench -p reknot --bench reload_latency --features jit-dev --locked -- --output /absolute/path/reload-results
python benchmarks/analyze_reload_latency.py /absolute/path/reload-results
```

未使用の絶対パスを指定します。Windowsなら `C:/benchmarks/reload-results` などです。
標準では全ケースを順方向と逆方向に、新しいプロセスで実行します。各プロセスで2回の
ウォームアップと9回の本測定を行います。短い動作確認には
`--case jit-restart-state --samples 1 --warmups 0 --rounds 1` を指定できます。
`--interval-us` と `--cpu-us` で要求負荷を変更できます。

実際のtyped bridgeへ毎秒1000要求を予定し、各要求で100 µsのbusy waitと5 msの協調的待機を行います。
予定到着時刻から完了までを遅延とし、所有スレッド停止中の送信の遅れや、受付停止中の待ちも含めます。
各保存前に500 msの無編集対照を置きます。所有ループは反復間に1 ms休止するため、処理時間も周期に加わります。

生JSONはウォームアップを含めた全要求、owner更新、bridge poll、受付の遷移、更新イベント、JITメトリクスを保存します。
ownerの計測はワーカーのpollと回収を含む `DevModule::update` 全体で、bridgeは別にpollします。
受付の変化はupdateの前後で観測するため、正確な停止時刻と断定せず、時間の下限・上限も記録します。
状態の継続、応答の一致、予定要求の全件完了、imageの実際の解放を検査します。
合成負荷による観測であり、速度の合格条件や本番SLAではありません。ケースごとに2プロセスで18回の更新を測ります。
要求をプールしたp99は、相関のある多数の要求から求めた値であり、更新の観測数が増えたことを意味しません。

[型付きJIT](TYPED_JIT.md) · [バイナリメッセージ](BINARY_MESSAGES.md)
