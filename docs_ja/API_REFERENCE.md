# API リファレンス

[English](../docs/API_REFERENCE.md) | 日本語

## 型付きアプリケーションAPI

通常の関数をホットリロードの入口にする API です。使い始める手順は [型付きハンドラー入門](TYPED_HANDLERS.md) を参照してください。
ここでは、各 API の契約と上限をまとめます。型付き API は既存の ABI を使い、`typed` feature で有効になります。

### 定義と初期設定

| API | 役割 |
| --- | --- |
| `Handler<Input, Output>::new(name, version)` | 安定したハンドラー名と入出力契約の版を定義する |
| `Module::new(name, version).with(handler)` | 1つの crate／DLL に含めるハンドラーを宣言する |
| `#[reknot::handler(CONTRACT)]` | 同期・非同期関数から呼び出し口を生成する。必要なら先頭引数に `RequestContext` を取る |
| `export_handlers!(module(), [handlers])` | crate 内のハンドラーをまとめて公開する。1 crate に1回記述する |
| `module().builder()` | ホスト資源、サービスの割り当て、同時要求数、Router の設定を組み立てる |
| `builder.load(path)` / `builder.from_api(api)` | ネイティブ DLL／常駐する関数表から `ModuleHost` を作る。`unsafe` が必要 |
| `DevModule::start(builder, options)` | 初回ビルド・ロードとソース監視を始める。`unsafe` が必要で、今後の編集と依存先も信頼の対象 |
| `DevModule::update()` | 変更検出、背景ビルドの結果適用、接続したブリッジ、キャンセル後の終了処理を進める |
| `reknot-new PATH` | host・logic・protocol のひな形を新しいディレクトリへ生成する CLI。feature `typed` が必要 |

protocol と logic の依存指定は `default-features = false, features = ["typed"]` です。
logic の `[lib] crate-type` はネイティブなら `["cdylib"]`、静的リンクも使うなら `["cdylib", "rlib"]` にします。
開発ホストでは既定 feature、または `typed`・`native`・`dev-tools` を有効にします。

### 契約の照合と入出力

候補の検査では、モジュール名・版、ハンドラー名・版、入出力の Rust 型名、Serde 両方向の生成スキーマ、状態の契約、実際の export 一覧を照合します。
一覧の順序は問いません。重複キーと検出できるハッシュ衝突は登録エラーになります。
識別名は Rust の関数名から自動生成せず、権限の付与にも使いません。

プロトコルの型に `#[reknot::schema]` を付けます。構造が異なる場合、`Error::Contract { report, source }` に期待値・実値、JSON ポインター、ハンドラーのソース位置を記録します。
スキーマの一致は意味の互換性を保証しません。[契約エラー](CONTRACTS_AND_SCHEMAS.md) を参照してください。
それらを変更したら対応する契約の版を上げ、ホストも再ビルド・再起動します。
ハンドラーの追加・削除も、ホストとロジックで共有する宣言の更新が必要です。

入出力は `Serialize + DeserializeOwned + JsonSchema + 'static` を満たす、所有する値です。
複数の引数は構造体やタプルにまとめ、値が不要な場合は `()` を使います。
借用した SDK オブジェクト、`&self`、ジェネリック関数は対象外です。

境界を渡るのはコピーした JSON バイト列です。Rust オブジェクトやそのデストラクタを DLL 間で共有しません。
JSON で表現できない値や変換失敗はエラーになります。
戻り値を `Result<Reply, AppError>` にした場合、呼び出しの外側の `Result` が SDK 側の失敗、内側が業務上の結果です。
ビルド失敗・候補拒否では受付先を維持し、実行時の panic や業務エラーでもイベントを自動再実行しません。

### 呼び出しとスレッド

| API | スレッドと受付の契約 |
| --- | --- |
| `ModuleHost::handler(contract)` | 所有スレッドで使う `HandlerHandle` を返す。clone して保持でき、更新時の再登録は不要 |
| `HandlerHandle::call(input)` | 同期に受付を試み、`Call<Output>` を返す。受付に成功すると、最初の poll より前に世代が確定する |
| `ModuleHost::bridge(options)` | `ModuleClient` と `BridgeDriver` の組を返す。driver を所有スレッドで poll する |
| `DevModule::bridge(options)` | driver を `update()` に組み込み、クライアントを返す |
| `ModuleClient::handler(contract)` | `Send + Sync` の `HandlerClient` を返す。別スレッドのイベント処理へ clone できる |
| `HandlerClient::call(input)` | 即時にキューへ入れ、`Send` の `RemoteCall<Output>` を返す。世代は所有側の受付時に確定する |
| `RemoteCall::generation()` | 受付前は `None`。受付後、要求が未完了の間は選ばれた世代を返す |

`ModuleClient` は `Send + Sync` です。`ModuleHost`・`HandlerHandle`・`Call`・`BridgeDriver` は `Send` ではありません。
Tokio では所有側を `current_thread` または `LocalSet` で動かします。
フレームワーク固有のイベント変換と送信はホストが担当し、専用アダプターは含みません。

### 容量と受付の上限

| 対象 | 上限・既定値 | 設定 |
| --- | --- | --- |
| JSON 入力・出力 | それぞれ1 MiB | `MAX_MESSAGE_BYTES` |
| モジュール内のハンドラー | 1～64個 | `MAX_HANDLERS` |
| モジュールの同時要求 | 既定256件 | `module().builder().max_in_flight(n)` |
| ブリッジの要求枠 | 既定256件。待機・実行・結果未受領の合計 | `BridgeOptions::capacity` |
| ブリッジの実行中要求 | 既定64件 | `BridgeOptions::max_in_flight` |

ブリッジの上限は正の値とし、`max_in_flight <= capacity` にします。モジュール側の上限も別に適用されます。
満杯や競合は `Error::Busy` です。自動再試行は行いません。
受付順は FIFO ですが、完了順は同じとは限りません。実行中のキャンセルでも、登録した子処理が終了するまでは枠を保持します。

`outstanding()` はブリッジの要求枠の使用数です。0でも、DLL を回収できることの証明にはなりません。

### キャンセルと終了

`Call::cancel` / `RemoteCall::cancel` の後も要求を await すると、登録した子処理の終了を待てます。
結果の完了がキャンセルより先なら、その結果が返る場合があります。Future の Drop もキャンセルを要求します。

| 操作 | 終了時の契約 |
| --- | --- |
| `client.close().await` | 新規受付を即時に閉じ、受け付けた処理を完了まで待つ。`HandlerClient` の clone を含め、最後のクライアントの Drop でも受付を閉じる |
| `client.shutdown().await` | 受付を閉じてキャンセルし、登録した子処理の終了を待つ。対象はそのブリッジの要求だけ |
| `BridgeDriver` の Drop | 残る要求と終了待ちに `Error::BridgeClosed` を通知する。正常な終了確認にはならず、後処理は `host.poll_cleanup` へ移る |
| ローカルの `Call` の Drop | 登録した子処理の後処理を、所有スレッドの `poll_cleanup` で続ける |

終了待ち中も driver を poll してください。`DevModule::update()` は接続したブリッジと後処理を駆動します。
独自・静的ホストでは、Drop 後の処理を `std::future::poll_fn(|cx| host.poll_cleanup(cx)).await` で待てます。
この `poll_cleanup` は、同じスレッド上のキャンセル済み要求全体を進めます。

停止に応じない子処理があれば終了待ちは続きます。管理外の処理の強制停止や、外部副作用の巻き戻しは行いません。

### 型付きハンドラーからのホストサービス

先頭引数に `RequestContext` を追加し、`ctx.call_service` / `ctx.call_service_async` でサービスを呼び出します。
ホストは `builder.resources(resources).bind(binding, resource.id())` で割り当て、
ロジックは `export_handlers!(..., requires = REQUIRED_SERVICES)` で必要なサービスを宣言します。
組み合わせは [型付きサービスのテスト](../reknot/tests/typed_services.rs) を参照してください。

`ctx.resource()` と `ctx.call()` は型付きの結果受け取りに予約されているため、アプリケーション I/O には使いません。
初期化データ領域もモジュール契約に使用します。設定は入力またはホストサービスで渡してください。
子処理は `spawn_child` / `register_activity` の既存契約に従って登録します。

監視とビルドの条件は [開発時の運用](DEVELOPMENT.md)、ネイティブコードの寿命は [DLL の寿命](UNLOAD_SAFETY.md) を参照してください。
型付き API は単一モジュールの Drain と [型付き Restart](TYPED_RESTART.md) に対応します。チェックポイントは `WorkerState<S>` で定義し、
`start_worker`、`poll_workers`、`stop_worker`、`remove_worker`、`worker_status`、`worker_error`、`worker_checkpoint` で登録ルートを管理します。
`restart` / `restart_api` は状態を維持し、`restart_with` / `restart_api_with` は明示的な `StateMigration` を受け取ります。
`ModuleRestart` は所有スレッドで進める Future です。失敗・中断時は受付停止を維持し、`resume()` で明示的に復旧します。
`DevOptions::with_reload_mode(ReloadMode::Restart)` で、状態契約を変えない自動 Restart を選べます。
`Module::schema_json()` は契約全体を出力します。複数 DLL の一括更新は対象外です。

## メッセージ API

Handler ABI 5、Services ABI 2、タスク制御 ABI 2 を使います。

`retention_snapshot()` は SDK 内のネイティブ参照数・コピー時のファイル長・状態・検証結果と
直近のコピー削除エラーを値として返し、画像参照を追加保持しません。メモリ量・予算使用数とは別です。
`inspect_cache(&CacheOptions)`／`clean_cache(&CacheOptions)` はコピー専用ルートを検査し、
clean は `apply=true` の場合だけ適格な終了済みセッションを削除します。CLI は `reknot-cache` です。
[管理単位・排他・限界](RETENTION_AND_CLEANUP.md) を参照してください。

既定の KeepLoaded に加え、終了条件を確認できるホストは `unsafe reclaim_native_image(ArtifactId)` で
退役画像を明示回収できます。`artifact_id()` は Router／候補の画像 ID を返し、
`release_retired_generations()` は Router が持つ完了済み旧世代の参照を解放します。
SDK は退役・コード参照・呼び出しの帰還・所有スレッドを再検査します。
外部 Waker の DLL 側処理全体の帰還、TLS、依存先の終了条件は呼び出し元の `unsafe` 契約です。
保持理由、`ReclaimResult`、close とコピー削除の区別は [DLL の寿命](UNLOAD_SAFETY.md) を参照してください。
Handler ABI と累積ロード予算の意味は変わりません。

`GenerationLocal<T>` は世代・所有スレッドごとの登録型TLSで、`with` / `with_mut` からアクセスします。
`RequestContext::host_notification()` は要求終了後もCハンドルのclone・wake・dropがホストで完結する通知です。
`call_async` は対応するホストでこの通知を自動利用します。
任意のTLSや依存先固有の終了は対象外です。[責任範囲と使い方](MANAGED_LOCALS_AND_NOTIFICATIONS.md) を参照してください。

メッセージ API は、資源の所有・処理の受付・世代付き実行を分離します。

### 資源とロジック

| API | 契約 |
| --- | --- |
| `HostResource::call(operation, input, output)` | 短い同期操作。書き込んだ長さまたは ServiceError を返し、借用バッファを保持しない |
| `HostResources::register(key, value)` | 非ゼロのサービス ID・版で登録し、型付き Resource<T> を返す |
| `Resource<T>::id()` / `try_borrow*()` | DLL には ID、ホスト内部だけ型付きアクセス |
| `HostResources::revoke(id)` | 以降のアクセスを失効。古い ID は再利用後のスロットを指さない |
| `HostResources::services()` | レジストリを保持する所有 Services を作る |
| `Services::call(...)` | レジストリ、ID 世代、サービス版、出力長を検査 |

Resource<T> の Drop だけでは登録を消しません。失効後も型付き所有者は本体を保持できますが、DLL 操作は REVOKED になります。
元のレジストリや Router のハンドルを捨てても、Services は所有参照を維持します。
同じレジストリを資源から保持する循環参照は revoke 等で解消してください。
所有スレッド上の信頼済みコード間のハンドルであり、悪意のある DLL の権限隔離ではありません。

```rust
use reknot::{RequestContext, ServiceError, ServiceKey};
const SERVICE: ServiceKey = ServiceKey::new(1, 1);
const STATE: ServiceKey = ServiceKey::new(2, 1);

#[reknot::reload_handler(SERVICE, state = STATE)]
async fn worker(ctx: RequestContext) -> Result<(), ServiceError> {
    let saved = ctx.checkpoint().ok_or(ServiceError::INVALID_ARGUMENT)?;
    ctx.cancelled().await;
    ctx.save_checkpoint(STATE, &saved.bytes)?;
    Ok(())
}
```

状態を使わない `reload_handler(SERVICE)` 形式もあります。
`async fn(RequestContext) -> Result<(), ServiceError>` から hot_handler_api と実行ごとの opaque Future を生成します。
factory 内で入力・seed をコピーし、サービスと制御表を retain します。
本体の開始は最初の poll です。未 poll の生成・破棄で I/O や共有状態の変更を始めません。

RequestContext は `input`、`generation`、`resource`、`service`、`services`、`call`、
`stop_requested`、`cancelled`、`checkpoint`、`save_checkpoint`、`spawn_child`、`register_activity` を公開します。
checkpoint() は初期 seed、save_checkpoint はホストへの確定データのコピーです。
`requires = &[ServiceRequirement::new(binding, key), ...]` と `init = Config` もマクロに指定できます。
Config は `InitPayload`（SCHEMA・encode・decode）を実装します。
`HandlerEnvironment::bind` と `with_initial(InitData::from_value(&config)?)` で設定を作り、
RouterOptions とともに `load_with_environment` / `from_api_with_environment` へ渡します。
候補の設定変更は `prepare_with_environment` / `prepare_api_with_environment` を使い、通常の prepare は現在の設定を再利用します。
要求本体・factory の実行前に追加サービスと純粋な初期化デコードを検査し、受付・切り替え時も資源の失効を検査します。
`initial::<Config>()`、`binding`、`call_service`、`call_service_async` で世代ごとのコピーと割り当てを使います。
追加要求32件・初期化バイト列1 MiBが上限です。通常の input・Checkpoint とは別です。
詳細は [複数サービスと初期化の契約](SERVICE_CONTRACTS_AND_INIT.md) を参照してください。
`call_async(operation, input, max_output_bytes)` は所有スレッドで待つ `ServiceCall` を返します。
入力をコピーし、ホスト I/O のキャンセル終了待ちを登録処理へ含めます。
ホストは `HostResource::start_async` と所有 `HostServiceFuture` を実装し、`CancellationToken` で停止を確認します。
`HostResources::with_async_limits` が操作・バッファ上限を指定します。
`ThreadService::spawn` の `ServiceClient` だけをホストスレッド間で共有し、各ロジックスレッドの
SDK ハンドルは移動しません。`ServiceWorker::shutdown()` と await で I/O スレッドを終了・join します。
詳細は [非同期サービスと複数スレッド](ASYNC_SERVICES_AND_THREADS.md) を参照してください。
ServiceError と捕捉した unwind は RunStatus::Error へ変換し、元の詳細エラーは現在の RequestTask から取得できません。
デストラクタの panic や panic=abort からの復旧は対象外です。

### 受付と実行

| API | 挙動 |
| --- | --- |
| `unsafe RequestRouter::load(path, resources)` | 信頼済み SDK ネイティブコードをロード |
| `unsafe load_with_options(path, resources, options)` | 共有予算とチェックポイント上限を指定 |
| `unsafe from_api` / `from_api_with_options` | 静的・独自 API。コードと ABI の寿命は呼び出し元が保証 |
| `dispatch(resource_id, input)` | 入力をコピーし、Future<Output = RunStatus> の所有 RequestTask を返す |
| `unsafe prepare(path)` / `prepare_api(api)` | 受付を変えず、不活性な候補を準備 |
| `activate(candidate)` | Drain。サービスと状態スキーマの一致が必要 |
| `pause()` / `resume() -> Result` | 受付を停止・再開。キューイングと再実行はしない |
| `in_flight()` | 登録ワーカーと旧 Drain 世代を含む管理対象ルートの未完了数 |
| `generation()` / `is_accepting()` / `options()` | 受付世代・状態・上限を確認 |

ネイティブロードは今後の置換候補も含め、初期化・終了・コールバックの契約を守ることが unsafe の条件です。
メタデータ検査はネイティブコードの安全性を検証しません。別 Router・古い基準世代の候補は拒否し、Drain 後も pause は維持します。

RequestTask は Future・世代・制御を所有し、Router より長く生存できます。
ホストが所有スレッド上で poll します。generation・resource・status で状態を確認できます。
`cancel(self)` と Drop はルート Future を同期的に破棄します。登録処理の停止・join は
所有スレッドの終了待ちキューで継続し、それまで未完了数を減らしません。
ホストは `poll_cancelled_requests(cx)` を進めます。`poll_workers` と Restart もこのキューを進めます。
副作用の取り消しや未登録の独立した子処理の自動停止は行いません。
外部 Waker はコード参照を保持し、ネイティブの既定は KeepLoaded です。

### 登録した子処理と外部コールバック

`spawn_child(|stop| async move { ... })` は所有スレッドで poll する `'static` な子 Future を登録します。
`ChildStop::cancelled()` / `is_requested()` で停止を確認します。生成クロージャは登録成功後の初回 poll で呼びます。
スレッドや外部ランタイムへの spawn ではありません。
`register_activity(adapter)` は外部処理の JoinHandle／購読を所有する `unsafe impl ManagedActivity` を受け取ります。
`request_stop` のエラーを記録・再試行し、`poll_join` は join・解除・実行中コールバックの帰還を確認してから
`Ready(Ok/Err)` を返します。確認できなければ `Pending` です。登録拒否時の初回 poll 前の Drop も安全に実装します。

`activity_count()` / `activity_error()`、ワーカーでは `worker_activity_count(id)` / `worker_activity_error(id)` が状態を返します。
要求あたり同時64件、満杯は `BUSY`、停止・終了後の追加は `REVOKED` です。
終了確認後も活動の失敗があれば Restart は `ActivityFailed` とし、対象世代の未完了数が0になってから
`acknowledge_activity_failure(generation)` でアプリケーションが確認済みにできます。
受付再開・ワーカー再登録・副作用の復旧は別操作です。
詳細は [管理の契約と検証](MANAGED_ACTIVITIES.md) を参照してください。

### 登録ワーカーと Restart

- `start_worker(WorkerId, resource, input, Checkpoint)`: 再生成する処理を明示登録。
- `poll_workers(cx)`、`worker_status(id)`、`worker_checkpoint(id)`: 駆動と観測。
- `stop_worker(id)`: 協調停止の要求。`remove_worker(id)`: キャンセルと登録解除。
- `restart(candidate, migrate) -> Result<Restart<'_>, RouteError>`: 受付停止、旧 Drain 世代を含む全管理ルートの停止待ち、
  全チェックポイントの移行と factory 準備後に切り替え。Restart が登録ワーカーを駆動し、外部所有の RequestTask はホストが並行して進める。
- `restart_with_commit(candidate, migrate, commit)`: 同じ停止・準備後、世代の切り替え直前に
  同期の `FnOnce() -> Result<(), ServiceError>` を一度呼ぶ。それ以前の失敗・中断では呼ばない。
  Err／unwind は `RouteError::HostCommit` として受付を停止。排他・トランザクション・復旧は
  アプリの責務で、SDK は副作用を巻き戻さない。準備済みの割り当てを失効させてはいけない。
  [SQLite とホスト内資源の例](HOST_RESOURCE_MIGRATION.md) を参照。

再生成は登録ワーカーだけです。失敗・中断後は旧チェックポイントを維持し、受付を閉じたままにします。
resume()? はワーカーの再生成ではありません。Restart は状態スキーマを変更できますが、サービス契約は一致が必要です。
詳細は [Restart の契約](RESTART_AND_MIGRATION.md) を参照してください。

Checkpoint は ServiceKey スキーマと所有 Vec<u8> です。
RouterOptions の既定は共有 GenerationBudget の累積ロード32枠、チェックポイント1 MiB。
ネイティブ枠はロード前に予約し、ファイル不存在・検査失敗・破棄候補も返却しません。
Router 再作成時も同じ予算を共有でき、used()・limit() で確認します。プロセス全体のメモリ上限ではありません。

## ビルド構成

既定で `typed`・`native`・`dev-tools` を有効にします。`typed` はSerde JSONによる型付きハンドラー、`native` は DLL ロード・保持量観測・コピー掃除、
`dev-tools` は `reknot::development` のソース監視・Cargo ビルド補助を提供します。
`native` だけを指定した構成には監視機能を含めません。`reknot-cache` は `native` が必要です。
ロジック crate と静的リンクのホストでは `default-features = false` を指定してください。

| 構成 | 依存の指定 | 含まれる機能 |
| --- | --- | --- |
| 開発ホスト | 既定 feature | DLL ロード・保持量観測・コピー掃除、ビルド・監視補助 |
| 外部で成果物をビルドするホスト | `default-features = false, features = ["native"]` | DLL ロード・保持量観測・コピー掃除 |
| ロジック crate・静的ホスト | `default-features = false` | サービス・タスク・ルーティング・ハンドラー生成 |
| 型付きロジック・静的ホスト | `default-features = false, features = ["typed"]` | 型付き入出力・モジュール契約。ネイティブ依存なし |

ロード方式と Debug／Release は独立しています。

`reload_handler` は静的リンク用の `handler_api() -> HandlerApi` も生成します。
これを `unsafe RequestRouter::from_api` へ渡します。呼び出し元はコールバックの寿命を保証します。
同じロジックとホスト資源の契約を使う静的サンプルは次のコマンドで実行できます。

```sh
cargo run -p service_host --release --no-default-features --features static-logic --locked
```

## 低水準タスク ABI

`TaskHandle` / `TaskVTable` は所有する外国側 Future、`FfiFuture` はその poll と一度だけの破棄を扱います。
低水準の `unsafe FfiFuture::new` ではコードと借用先の寿命を呼び出し元が保証します。
通常の受付処理には世代と終了待ちを管理する `RequestTask` を使ってください。
`native` 有効時は、I/O・ライブラリ・シンボル取得の失敗を `LoadError` と `RouteError::Load` で返します。
現在の境界は Handler ABI 5、Services ABI 2、タスク制御 ABI 2 です。
ホストとロジックを同じ SDK でビルドしてください。

実行時の契約は [互換性と対象範囲](COMPATIBILITY.md)、API の変更点は [変更履歴](../CHANGELOG.md) を参照してください。
