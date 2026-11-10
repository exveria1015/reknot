# ホスト資源と Drain の受付切り替え

[English](../docs/HOST_RESOURCES_AND_ROUTING.md) | 日本語

Handler ABI 5・Services ABI 2 のメッセージ API の契約です。
常駐ワーカーの協調停止と状態移行は [Restart](RESTART_AND_MIGRATION.md) に分けています。

## 役割と範囲

ホストが接続・セッション等の本体を所有し、DLL は版を定義したサービス操作を呼び出す。
新しいリクエストの受付先は `RequestRouter` が管理する。状態へのアクセスは版付きホストサービスで行う。
本書は **Drain** と資源の操作契約を扱う。登録ワーカーの協調 Restart とチェックポイント移行、
[明示登録した子処理の停止・join](MANAGED_ACTIVITIES.md) も提供する。未登録処理は自動発見しない。

| 型 | 責務 |
| --- | --- |
| `HostResources` | ホスト資源の登録・ID 検査・失効。ロジックの世代とは独立した寿命 |
| `Resource<T>` | ホストだけが持つ型付きアクセス。`try_borrow`／`try_borrow_mut` と `id()` |
| `Services` | ホストサービスの所有参照。DLL では clone／drop もホストの関数表へ返す |
| `RequestContext` | DLL が所有する入力のコピーと、保持したサービス参照。対象資源・サービス版・実行世代 ID |
| `RequestRouter` | 新規受付の世代、候補の準備・有効化、受付の一時停止／再開 |
| `RequestTask` | 一つの実行と旧世代の寿命。完了・エラー・キャンセルを個別管理 |

すべて作成スレッドで使う。`Services` とタスクは Send ではない。
サーバーでの並列実行には所有スレッド上のローカル Executor を使える。
複数所有スレッドからのホスト側共有は [非同期サービス・複数スレッド](ASYNC_SERVICES_AND_THREADS.md) を参照。
Waker による別スレッドからの通知は、既存のホスト Waker ABI と DLL 世代保持の契約を使う。

## 資源の登録・失効

ホストは `HostResource::call` を実装し、`register(ServiceKey::new(id, version), value)` で登録する。
サービス ID・版・操作番号・入力と出力のバイト形式はアプリケーションが定義する。
DLL 境界を越えるのは ID、C ABI の関数表、呼び出し中だけ借用するバイト列であり、`T` のレイアウトや Rust のコンテナを共有しない。

`ResourceId` はレジストリ・スロット・スロットの世代からなる。
`revoke(id)` 後に同じスロットを再利用しても、古い ID は新しい資源を指さない。
ID のコピー自体には所有権はない。`Resource<T>` を捨てるだけでは登録は消えず、明示的に revoke する。
失効後もホストが `Resource<T>` を保持していれば本体は生存できるが、DLL の操作は REVOKED を返す。
これらの ID は信頼済みコード間の参照であり、悪意のあるネイティブコードに対する権限制御ではない。

各 `Services` はホスト側レジストリを retain する。元の `HostResources` や Router を破棄した後も、既存のタスクや明示的に clone したサービス参照は有効。
最後の所有参照がなくなったときに、登録中の資源を破棄する。資源が同じレジストリの Services を保持する循環参照は、revoke 等で解消する必要がある。

## サービス操作

```rust,ignore
impl HostResource for Session {
    fn call(&mut self, operation: u32, input: &[u8], output: &mut [u8])
        -> Result<usize, ServiceError>
    {
        // 版を定義した操作を実行し、output に書いた長さを返す。
        // ネットワーク書き込みはホストの I/O ループへキューイングする。
        todo!()
    }
}
```

操作は **短い同期処理** に限定する。実際のソケット I/O、受信バッファ、TLS、接続の再接続判断はホスト／アダプター側の責務。
所有するホスト Future を返す `start_async` と `call_async` は [非同期サービス](ASYNC_SERVICES_AND_THREADS.md) で別途定義する。
ホストの `Resource<T>` が借用中、または同じ資源へ再入した場合は BUSY を返し、同時に可変参照を作らない。
排他性は一つの操作単位であり、await をまたぐ複数操作のトランザクションではない。旧新タスクの操作の整合性や応答の順序はホスト側のプロトコルで定義する。

主なエラーは REVOKED、CONTRACT_MISMATCH、BUSY、BUFFER_TOO_SMALL、INVALID_ARGUMENT、PANICKED。
ホスト操作の unwind は FFI 内で捕捉する。既に起きた部分更新は巻き戻さない。
出力長がバッファを超える実装も拒否する。バッファ不足時の自動再試行はしないため、不可逆な操作を始める前にホスト実装側で必要な容量を確認する。
破棄中の二重 panic や panic=abort、契約に違反する unsafe なネイティブコードからの回復は保証しない。

## ハンドラー

```rust
use reknot::{RequestContext, ServiceError, ServiceKey};

#[reknot::reload_handler(ServiceKey::new(0x4c49_4e45, 1))]
async fn handle(request: RequestContext) -> Result<(), ServiceError> {
    request.call(0, request.input(), &mut [])?;
    Ok(())
}
```

マクロは `hot_handler_api` と実行ごとの opaque Future を生成する。
入力と初期チェックポイントは factory の呼び出し中に DLL 側へコピーし、サービスとタスク制御を retain する。ホストのスタック上の descriptor や入力バッファを Future に保存しない。
ハンドラー本体は最初の poll から実行する。エラーと unwind は RunStatus::Error へ変換し、Future は一度だけ破棄する。
現時点では元の ServiceError の詳細は RequestTask の結果から取得できない。

## 候補の準備と切り替え

```rust,ignore
let resources = HostResources::new();
let session = resources.register(LINE_SESSION, host_owned_session);

// 信頼済み SDK コードの初期化・終了処理と FFI 所有契約を確認する境界。
let mut router = unsafe { RequestRouter::load(old_library, resources.clone()) }?;
let old_task = router.dispatch(session.id(), b"first message")?;

let candidate = unsafe { router.prepare(new_library) }?;
// ここまでは旧世代への受付を継続。
router.activate(candidate)?;
let new_task = router.dispatch(session.id(), b"next message")?;
// old_task は旧コード、new_task は新コードで実行する。
```

prepare は DLL のコピー・ロード、安定したヘッダーの検査、API 表の検査、対象サービスの ID／版の一致確認を行う。
この間は受付先を変更しない。候補の DLL 初期化コードも外部送信やタスク起動などを始めない契約とする。
候補を破棄しても有効化しなければ受付先は変わらない。

activate は Router の所有スレッドで受付先を一度に変更する。実行途中のタスクは変更しない。
別 Router 向けの候補や、別の候補を既に有効化した後の古い候補は拒否する。
`pause()` 中の dispatch は Paused を返す。pause 中に activate しても、自動で受付を再開せず、`resume()?` を明示的に呼ぶ。
入力を内部で無制限に保留したり、失敗したリクエストを自動再実行したりはしない。

`in_flight()` は Router が管理する未完了リクエスト数。ルートと明示登録処理の join・破棄後に減る。
cancel・Drop 後も登録処理が残れば未完了として数え、所有スレッドの終了待ちキューで進める。
残存 Waker・未登録コールバック・保持 DLL 数や、アプリケーションが独自に起動した未登録タスク数を表すものではない。

サービスの版の一致は、同じ版に異なる意味を与えた実装を検出するものではない。
この Router は主サービスと宣言された追加サービス要求を検査する。各操作でも対象資源の版を検査する。
登録ワーカーのチェックポイント移行は Restart が行う。追加サービスの要求表と明示割り当て、
型付き初期化データの検証は [契約と初期化](SERVICE_CONTRACTS_AND_INIT.md) を参照。
全共有状態の互換性検証は行わない。
主サービスの版が変わる候補は Drain・Restart とも拒否する。追加サービスは候補に別版の資源を割り当てられる。
非互換なホスト資源変更には、停止後の `restart_with_commit` でアプリ固有の排他・トランザクションを行う。
[ホスト資源・DB の移行](HOST_RESOURCE_MIGRATION.md) を参照。

## DLL の寿命と ABI

新経路は Handler ABI **5** と Services ABI **2** を個別のヘッダーで検査する。
Rust のアロケーターを共有する必要はなく、各側で確保したコンテナはその側で破棄する。

RequestTask は生成した DLL 世代を保持し、Waker にもコード参照を渡す。
新経路の DLL は現在 KeepLoaded。候補のロード失敗・破棄を含め、通常の切り替えだけで物理アンロードを試みない。
累積ロード試行数は共有 GenerationBudget で制限する。既定32枠で、失敗候補も消費し自動返却しない。
自動回収は行わず、枯渇後は開発プロセスを再起動する。詳細は [保持上限](RESTART_AND_MIGRATION.md#保持上限) を参照。
from_api／prepare_api は静的リンク等の拡張口で、呼び出し元がコールバックとコードの有効期間を保証する unsafe API。

## サンプルと確認結果

```powershell
cargo run -p service_host --locked
```

既定は 127.0.0.1:4000、環境変数 REKNOT_ADDR で変更できる。
改行で区切ったテキストを送信し、`examples/service_logic/src/lib.rs` の応答ラベルを変更して保存する。
同じ接続の次のメッセージが新世代を使う。`slow` と改行を送ると5秒間の処理になり、その間の更新でも旧処理は旧コードで完了する。
このサンプルは単一接続・単一スレッド。ビルド自体は同期的に行い、接続の入力待ち中の変更は次のメッセージ境界で反映する。

接続維持と Drain は `native_routing`、所有と失効は `host_resources`、受付と候補は `request_routing` で検証する。
再実行手順と Miri／実 DLL の範囲は [テスト](TESTING.md) を参照。
