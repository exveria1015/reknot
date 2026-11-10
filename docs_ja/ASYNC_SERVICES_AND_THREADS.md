# 非同期ホストサービスと複数スレッド

[English](../docs/ASYNC_SERVICES_AND_THREADS.md) | 日本語

ホストが I/O と実行スレッドを所有し、ロジック DLL はコピーしたバイト列を送って結果を待ちます。
現行は **Handler ABI 5 / Services ABI 2 / タスク制御 ABI 2 / Activity ABI 1** です。
ホストと DLL を一緒に再ビルドしてください。旧 Handler ABI 2～4 はロード時に拒否します。

## ロジックからの呼び出し

```rust
let reply = ctx.call_async(1, b"ping", 64)?.await?;
ctx.call(2, &reply, &mut [])?;
```

`call_async(operation, input, max_output_bytes)` は入力をコピーし、`ServiceCall` を返します。
実際の開始は、登録成功後の所有スレッドでの poll まで遅延します。
ホストは `HostResource::start_async` から、所有する `HostServiceFuture` を返します。
同期の `HostResource::call` も併用できます。既定実装はそれぞれ `UNSUPPORTED_OPERATION` です。

開始関数は短い同期処理とし、戻り値の Future が I/O と必要なデータを所有します。
開始に失敗した場合は進行中の操作を残さず、Future の poll はブロックさせません。
Future の `Ready` は I/O が終了していることを表します。単にキャンセル要求を送った時点で
`Ready` を返したり、終了待ちが必要な I/O を detach したりしないでください。

`call_async` は前段の [登録処理の管理](MANAGED_ACTIVITIES.md) に参加します。
ルートが戻った場合、`ServiceCall` を Drop した場合、ルートを cancel / Drop した場合にも
停止を要求し、実 I/O の終了応答まで登録処理と未完了数を維持します。
Drop 後は `poll_cancelled_requests`、`poll_workers` または Restart の polling を続けます。
資源の revoke は既存操作に停止を伝えますが、終了待ち用の操作 ID を早期には無効化しません。
終了後の結果は `REVOKED` とし、古い結果を新しい資源へ適用しません。

キャンセルの結果は `CANCELLED`、資源失効は `REVOKED`、満杯は `BUSY`、
サービス終了後の受付は `CLOSED` です。通常の I/O エラーは呼び出し結果として返します。
I/O エラーと「終了確認ができなかった」という登録処理の失敗は区別します。
常駐ワーカーで停止時の `CANCELLED` を正常停止にしたい場合は、ワーカー自身で判定して `Ok(())` を返します。
`?` でそのまま返したエラーはルートの Error になります。

## 上限と結果の受け渡し

| 制約 | 既定・挙動 |
| --- | --- |
| レジストリの操作枠 | `AsyncLimits::default()` は64件。完了済みで未回収の結果も数える |
| 入出力 | 既定各64 KiB。`HostResources::with_async_limits` で指定。各バッファの上限は1 MiB |
| 要求ごとの返却長 | `call_async` の第3引数で指定。ホスト側上限より大きい指定は開始前に拒否 |
| 超過 | 入力・出力の超過は `INVALID_ARGUMENT`。結果を切り詰めて成功にしない |
| バックプレッシャー | 満杯は `BUSY`。受付枠の自動待機、自動再試行、通常要求のリプレイはしない |
| 操作 ID | レジストリ ID と非再利用の通し番号。別レジストリ・回収済み ID は `REVOKED` |

DLL 境界では Rust の `Vec` / Future / 接続を共有せず、C ABI の ID・状態・バッファを使います。
ホストに戻った結果はコピーして受け取り、操作を回収します。低水準 ABI の出力バッファが
小さい場合は必要長を返し、完了結果を保持します。操作をもう一度実行するわけではありません。
終端結果の回収後に ID を解放します。保留中 ID の早期 release では停止を要求して保持します。

ホスト Future の poll が panic した場合は終了確認を推定せず、Future を保持して以降 Pending にします。
その状態からの一般的な自動復旧は提供しません。上限は SDK・キューが保持するデータに対するもので、
アプリケーションが別途確保するメモリや、I/O ハンドラー内部の最大使用量を制限するものではありません。

## ホストの I/O スレッド

`ThreadService::spawn(limits, handler)` が、`ServiceClient` と `ServiceWorker` を返します。
新規の必須ランタイム依存は加えず、標準ライブラリのスレッド・Mutex・Condvar を使います。

- `ServiceClient` はホスト内の安全な `Send + Sync` の接続口です。コピーした入力・操作番号・同期した返却状態だけを渡します。
- `ServiceWorker` は作成スレッドに所属します。`shutdown()` が待機中／実行中の処理に停止を要求します。
- `ServiceWorker` を await すると実際にスレッドを join します。Drop は shutdown の要求であり、join 完了ではありません。
- I/O ハンドラーのクロージャと I/O 資源はワーカースレッドで使います。クロージャは信頼済みのホスト側コードに置きます。
- ハンドラーは `CancellationToken::is_cancelled()` を確認し、ソケットの timeout 等で待機時間を制限してください。

ThreadService の容量は待機中・実行中・未回収の返答を含めた数で、全クライアントで共有します。
完了結果を消費して必要な所有者が解放されるまで枠を戻しません。
入力サイズと返却サイズにも独立した上限を設けます。
I/O ハンドラーの panic はその操作の `PANICKED` に変換します。デストラクタの panic、
FFI を越える unwind、panic=abort、任意のホスト Future の故障からの自動復旧は保証しません。

## 公平性とスレッドの所属

1個の ThreadService には1個の I/O スレッドがあります。受理した要求はキューの登録順に
FIFO で1件ずつ処理します。同時に到着した別スレッド間の順序、クライアントごとの枠の予約、
処理時間の上限までは保証しません。先頭の I/O が停止要求を無視すれば、後続も待ちます。
長い処理と短い処理を分ける場合は、別の ThreadService に分ける等の方針をアプリケーションで決めます。

各ロジックスレッドでは、そのスレッド上で HostResources・RequestRouter・RequestTask を作成・poll・破棄します。
SDK ハンドルや生ポインタを別スレッドへ渡して Send とみなす実装は追加していません。
各スレッドで ServiceClient のクローンをローカルなホスト資源へ登録します。
レジストリが違えば資源 ID も操作 ID も独立しています。

対応ホストでの通知には、ホスト所有のC通知ハンドルとホスト内のRust Wakerを使い、元の所有スレッドを起こします。
要求の登録処理まで終了すると通知先を切り離すため、ホストI/Oに残った通知のclone・wake・dropはDLLを呼びません。
古いホストでは従来のFFIブリッジへ戻ります。[登録型TLSと通知](MANAGED_LOCALS_AND_NOTIFICATIONS.md) を参照してください。
Waker が別スレッドから通知できることと、タスク本体を移動できることは別です。
入力はホストの所有する値として I/O スレッドへ移り、結果はロジックの所有スレッドで回収します。
外部スレッド上の Waker 解放も世代保持の契約に従い、自動アンロードは追加していません。

## 実行・検証

```powershell
cargo run -p threaded_io_host --locked
cargo test -p reknot --test async_services --test threaded_services --locked
cargo test -p threaded_io_host --test native_threaded_io --locked -- --ignored --test-threads=1 --nocapture
./scripts/check-miri.ps1 -Toolchain nightly-2026-11-02
```

サンプルはソースを書き換えず、ロジック DLL のv1／v2を別々に作り、有限回の検証後に終了します。
ビルド出力は `target/threaded-io-build/<PID>/` に分離します。
実 TCP の応答をホスト I/O スレッドで受信し、2本のロジック所有スレッドが未完了の旧要求を
残したまま Drain して、新旧それぞれの世代の結果を受け取ります。
最後にロジック資源を所有スレッドで破棄し、作成スレッドで I/O ワーカーを join します。

ネイティブ試験では実 DLL＋TCP、読み取り待ち中のキャンセルとソケット解放、送信を再実行しないこと、
キュー満杯・未回収結果の容量維持・FIFO・キャンセル済み待機要求の I/O 非実行・所有スレッドを検査します。
Miri は実 TCP を除く非同期サービスと複数所有スレッドの同期・通知・破棄を両借用モデルで検査します。
Miri の Linux インタープリターターゲットでの成功を、Linux の実 DLL／実ネットワーク実行の証拠とは扱いません。
実行方法と検証範囲の区別は [テスト](TESTING.md) を参照してください。

この段階では任意の Future のスレッド間移動、スレッドプールへの DLL タスクの移管、
I/O のロールバックや強制停止は提供しません。複数サービスの明示的な契約検査は
[複数サービス・初期化データの契約](SERVICE_CONTRACTS_AND_INIT.md) を参照してください。
