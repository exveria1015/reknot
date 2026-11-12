# 子タスクと外部コールバックの管理

[English](../docs/MANAGED_ACTIVITIES.md) | 日本語

メッセージ API に明示登録した処理を、ルート要求の終了待ちに含めます。
現行は Handler ABI **5**、Services ABI **2**、タスク制御 ABI **2**、Activity ABI **1** です。
ホストとロジック DLL を一緒に再ビルドしてください。旧 Handler ABI 2～4 の DLL は拒否します。

## 登録と進行

```rust
ctx.spawn_child(|stop| async move {
    // 所有スレッドで poll される、所有データだけを持つ子 Future。
    stop.cancelled().await;
    // 必要な非同期後処理をここで待つ。
    Ok(())
})?;
```

`spawn_child` は新しいスレッドや外部ランタイムへ spawn しません。
ホストが `RequestTask` を poll するたびに、ルートと登録済み処理を進めます。
生成クロージャは登録成功後の初回 poll まで呼びません。子は `'static` に限定し、
ルートのスタックや Future への借用は持てません。
実行できる例は `cargo run -p reknot --example managed_children --locked` です。

`RequestContext::cancelled()` と `ChildStop::cancelled()` は、待機Futureごとに
通知先を登録します。`FuturesUnordered` のように通知された要素だけをpollする組み合わせや、
同じcontext・複製した子トークンからの複数待機にも対応します。再pollではその待機のWakerを更新し、
Dropでその登録だけを解除します。停止待機は管理対象の64枠を消費せず、新しい処理もspawnしません。

リクエストの停止通知は、既存のタスク制御・世代サービスの先頭レイアウトに続く、
任意の cancellation ABI 1 拡張を使います。ホストとロジックを一緒に再ビルドしてください。
この拡張のない旧ホストで停止前に `cancelled()` を待つと明示的にpanicし、生成ハンドラーはErrorを返します。
未提供の領域を読んだり、通知なしに待ち続けたりしません。同期的な `stop_requested()` は引き続き使用できます。

独立したランタイムの JoinHandle やコールバック登録には、
`ctx.register_activity(adapter)` と `unsafe impl ManagedActivity` を使います。
アダプターは次の2操作を実装します。

| 操作 | 契約 |
| --- | --- |
| `request_stop(Pin<&mut Self>)` | 停止・購読解除を要求。`Ok` は要求の受理であり、終了確認ではない。`Err` は記録して次の poll で再試行 |
| `poll_join(Pin<&mut Self>, cx)` | join・解除と実行中コールバックの帰還を確認。確認前は `Pending`。`Ready(Ok/Err)` は両方とも終了確認を表す |

解除 API が失敗し、まだコールバックが走る可能性がある場合は、`Ready(Err)` を返してはいけません。
停止要求のエラーを返し、join 側は `Pending` に保ちます。
`Ready(Err)` は、例えば子処理が失敗したが join 自体は完了した場合に使います。
アダプターが終了確認を偽って返せば安全性を失うため、この trait の実装は unsafe です。

アダプターは外部処理が使うデータを所有し、ルート本体の破棄後も有効に保ちます。
登録拒否では初回 poll 前のアダプターを破棄するため、この時点の Drop も安全である必要があります。
外部登録の開始を初回 poll に置く設計が扱いやすくなります。
登録・停止・join・破棄は所有スレッドで行い、外部スレッドから thread-bound な
`RequestContext` やサービス表へ直接アクセスしません。外部の実行元には別途同期を用意します。

## 何を終了として数えるか

- 通常終了・ルートのエラー／panic：ルート Future を破棄し、登録処理の停止・join を待つ。
- Restart：過去の Drain 世代も含めて停止を要求し、ルートと登録処理の終了確認後に移行する。
- 子処理の失敗：ルートと他の登録処理にも停止を要求し、全件の終了確認後に要求を Error とする。
- `cancel(self)`／Drop／`remove_worker`：ルート Future は同期的に破棄する。残る登録処理は所有スレッドの終了待ちキューへ移す。

`RequestTask::status()` が終端値になること、`in_flight()` が減ることは、
登録処理の join・解除後のアダプター破棄まで待ちます。
ルートの async 関数が戻った時点とは区別してください。
`activity_count()` / `activity_error()`、ワーカーでは
`worker_activity_count(id)` / `worker_activity_error(id)` で途中の状態を確認できます。
停止エラーの最初のコードを保持します。join の終端エラーは共通の `FAILED` として記録します。
チェックポイントの内容・保存時点はアプリケーションが決めます。子の最終結果が必要なら、
停止要求後にその結果を受け取ってから保存してください。終了待ちが状態を自動採取するわけではありません。

登録枠は要求ごとに同時64件です。満杯は `BUSY`、停止要求後・ルート終了後の登録は `REVOKED` です。
完了済み処理は破棄して枠を戻します。未登録の独自 spawn を自動発見する機能ではありません。

## Drop 後の終了待ち

```rust,ignore
drop(task); // ルートは破棄。登録した後処理は未完了のまま残り得る。
// フレーム／ホストの executor から、同じ所有スレッドで繰り返し呼ぶ。
let pending = reknot::poll_cancelled_requests(&mut cx).is_pending();
```

`poll_workers` と Restart もこのキューを進めます。キューは所有スレッド単位で、
Router の Drop 後にも `poll_cancelled_requests` で進められます。
1回の呼び出しでは開始時点の要求をpollします。その終了処理中に別の要求がDropされた場合は、
キューへ所有を戻してから次のpollを通知します。既にpollしたI/O待ちは自身の通知を使うため、
未完了の要求があるだけでは自己通知を繰り返しません。
他の Router の未完了処理も進めますが、`poll_workers` の返値は対象 Router の登録ワーカーについての値です。
停止後も待つ処理があれば、ホストはこの polling を継続してください。
DLL 世代・アダプター・必要な所有データはその間保持します。

終了待ちを進めない場合や、終了確認前に所有スレッドが終わる場合、未終了のアダプターと世代参照を
意図的に保持してメモリを解放しません。非同期の join を Drop で済ませたことにはしません。
既定の KeepLoaded と同じく、データの早期解放を避けるための保持であり、停止の保証ではありません。
unsafe 外部アダプターのメソッドが panic した場合も終了確認を推定せず、poison 状態で保持します。
安全なローカル子 Future の panic は、Future 破棄後の終端 Error に変換します。
デストラクタの panic や panic=abort からの回復は対象外です。

## 失敗後の Restart

join まで完了しても、登録処理の停止・終了で失敗があれば
`RouteError::ActivityFailed { generation }` として切り替えを拒否し、受付を停止したままにします。
失敗は旧 Drain 世代や Drop 済み要求についても残します。
終了が確認できていなければ、Restart は引き続き Pending です。必要な待ち時間の上限はホストで設けます。

アプリケーションが失敗と副作用を確認した後、`acknowledge_activity_failure(generation)` で
記録を確認済みにできます。対象世代の `in_flight` が0でなければ拒否します。
これは受付再開・失敗ワーカーの再登録・外部副作用の巻き戻しではありません。
失敗ワーカーは必要に応じて解除・再登録し、Restart の再試行と `resume()` を明示的に行ってください。
登録処理の失敗のない通常のルートエラーについては、既存の Restart 契約を維持しています。

## 検証

```powershell
cargo test -p reknot --test managed_activities --test native_routing --locked
cargo run -p reknot --example managed_children --locked
./scripts/check-miri.ps1 -Toolchain nightly-2026-11-02
```

`managed_activities` は遅延する子の終了、通常終了・panic、上限、停止後の登録拒否、
Drop 後の終了待ち、解除失敗・終端エラー・確認後の再試行を検査します。
Miri では同じ管理経路を Linux インタープリターターゲット、Stacked / Tree 両借用モデルで検査します。
実 DLL の試験はネイティブで別に実行します。ツールチェーンとログの扱いは [テスト](TESTING.md) を参照してください。

`native_routing` はマクロ非依存の実 DLL で、別スレッドから DLL のコールバックを実行します。
ホストが待機を解除するまでコールバックを実行中に保ち、ルート／Router の Drop 後も
状態を保持し、コールバック帰還・スレッドjoin・アダプター破棄の順を確認します。
解除要求の失敗が終了確認や切り替え成功と誤認されないこと、旧 Handler ABI 2～4 の拒否も検査します。

任意のランタイムや OS の購読解除 API への完成済みアダプター、強制停止、
外部 I/O のロールバック、自動アンロードは提供しません。
コールバック元の解除・実行中呼び出しの同期は、各 `ManagedActivity` 実装の責任です。
