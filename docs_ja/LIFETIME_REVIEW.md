# 非同期実行の所有・借用・破棄順序

[English](../docs/LIFETIME_REVIEW.md) | 日本語

Handler ABI 5 の `RequestTask` を、ネイティブロード・静的リンクで共通に使います。

## メッセージ実行

`RequestTask` は Future、コード世代、ホスト側の停止制御を保持します。
DLL の RequestContext は入力・初期チェックポイントを所有し、サービスとタスク制御の参照を retain／release します。
Router や元のレジストリハンドルを破棄しても、実行に必要な参照は残ります。
RequestTask の完了は外国側ルート Future と登録処理の join・破棄後に未完了数を減らします。
cancel・Drop ではルート Future を破棄し、登録処理の後処理を所有スレッドのキューへ移します。
後処理は `poll_cancelled_requests`、登録ワーカーの poll、Restart で駆動し、世代と必要なデータを保持します。
Restart は管理対象ルートと登録処理の終了確認まで待ってからチェックポイントを移行します。

これらは所有スレッドで扱います。明示登録には所有データを持つ `'static` の処理／アダプターを使い、
ルート Future の借用を外部へ残しません。未登録の独自 spawn・コールバックは自動管理しません。
停止要求・終了確認・コード回収は別の契約です。詳細は [登録処理](MANAGED_ACTIVITIES.md) と [Restart](RESTART_AND_MIGRATION.md) を参照。

## コードの責務

| モジュール | 責務 |
| --- | --- |
| `routing.rs`、`routing/control.rs` | 受付・世代・タスク制御・未完了数と終了待ち |
| `routing/restart.rs` | 協調停止、チェックポイント移行、全 factory の準備、確定フック |
| `task/future.rs` | `TaskHandle` の所有、poll、完了・失敗・キャンセル時の一度だけの破棄 |
| `task/waker.rs` | 所有権を持つ Waker の相互変換とコード参照の保持 |
| `activity.rs` | 明示登録した処理の停止・join・外部コールバック帰還 |
| `generation.rs`、`native.rs` | コード参照、画像の観測、開けた DLL のプロセス終了までの保持 |
| `request.rs`、`reknot_macros/src/handler.rs` | 入力コピー、サービス保持、独立した Future の生成 |

## Future と Waker の寿命

`FfiFuture` は poll が完了・失敗を返した時点で外国側の Future を破棄し、その後に結果を公開します。
再度 poll しても破棄済みのハンドルは呼びません。キャンセル時も同じハンドルを一度だけ破棄します。
Waker のクローンはホストの所有ハンドルとコード参照を保持し、poll のスコープを越えて利用できます。
別スレッドからの wake に対応しますが、Future・Router・資源レジストリは所有スレッドに置きます。

低水準の `unsafe FfiFuture::new` は DLL の参照を自動で追加しません。
呼び出し元はコード・vtable・借用先の寿命と poll/drop の契約を保証してください。
通常のホスト処理では、世代と登録処理の終了待ちも管理する `RequestTask` を使います。

## 破棄と登録処理

ルート Future の終了だけで外部 I/O や登録処理が終了したとは扱いません。
`RequestTask` の Drop 後も登録処理が残る場合は、ホストが所有スレッドで終了待ちを駆動します。
終了確認前にその世代の未完了数を減らさず、Restart の確定にも進みません。
管理外の処理は自動発見しないため、アプリケーションが停止と帰還を確認します。

検証手順は [テストと Miri](TESTING.md)、ネイティブ画像の保持契約は
[DLL の寿命](UNLOAD_SAFETY.md) を参照してください。
