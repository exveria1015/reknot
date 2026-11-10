# DLL の寿命と管理外タスク

[English](../docs/UNLOAD_SAFETY.md) | 日本語

## ネイティブ画像の保持

`RequestRouter` が開いた DLL は、既定ではプロセス終了まで保持します。
採用しなかった候補、ABI・シンボル・サービス検査で拒否した候補も対象です。
Router の Drop、ルート要求の完了、Waker の破棄から物理アンロードは実行しません。
外部処理と TLS を含む終了条件を確認できるホストには、低水準の
`unsafe reclaim_native_image(ArtifactId)` を提供します。Handler ABI 5 は変更していません。

ロード直後からホストの保持一覧に所有参照を保存するため、検査失敗や後続の unwind でもコードは残ります。
`RequestTask`、登録処理、外部へ渡した Waker も実行世代への参照を保持します。
SDKのホストI/Oには、要求終了後に無効化できるホスト所有の通知口を使います。
この通知の外部クローンはDLLを保持せず、終了後のwake・dropもホスト側で完結します。
`GenerationLocal<T>` に登録したTLSは世代の所有終了時に破棄します。
対象範囲と依存先の責任は [登録型TLSと通知](MANAGED_LOCALS_AND_NOTIFICATIONS.md) を参照してください。
一方、静的リンクではコードが実行ファイルに含まれ、DLL のコピーは作りません。

## ホストから明示的に回収する

回収する画像の ID は `RequestRouter::artifact_id()` または `PreparedHandler::artifact_id()` で取得します。
ID と `retention_snapshot()` の戻り値は画像を保持しません。
ホストは次の順序で、画像をロードしたスレッドから回収を呼び出します。

1. 新世代へ切り替えるか Router を破棄し、その画像への受付を終了します。
2. 旧要求・登録処理を完了させ、キャンセル後の処理も `poll_cancelled_requests` で終了まで進めます。
3. 旧タスク・候補を破棄し、`release_retired_generations()` で Router が持つ完了済み旧世代を解放します。
   登録処理の失敗を記録した世代は、既存の `acknowledge_activity_failure` による確認も必要です。
4. 外部スレッドの join、購読解除とコールバックの帰還、TLS・依存ライブラリの終了条件を確認します。
5. その確認を `unsafe` 契約として引き受け、`reclaim_native_image(id)` を呼び出します。

SDK は画像が退役済みで、コード参照が残らず、ホストから開始した DLL 呼び出しもすべて戻っていることを検査します。
参照の再取得を防ぐため、保持一覧の排他中に再検査して所有参照を取り出します。
実際の `Library::close` は SDK のロックを解放してから実行します。

ただし、外部スレッド上の DLL 側 Waker は、最後のホスト参照を解放したあとも
DLL 内で破棄や帰還を続けることがあります。そのため、参照ゼロや次の poll を終了確認には使えません。
DLL 側の wake・drop・clone 全体が戻り、再び呼ばれないことを、join 等の別の手段で確認してください。
共有スレッド上の TLS、OS 登録、依存ライブラリの後始末を確認できない画像は、そのまま保持します。
SDK は回収の安全性を自動判定する DLL 用終了フックや、世代専用スレッドを提供しません。

`retention_snapshot().images` の各画像は `code_leases`、`active_calls` と `retention_reasons` を返します。
保持理由は次のとおりです。終了確認はホスト側の責任なので、スナップショットには
`ExternalQuiescenceUnconfirmed` が常に残ります。

| 理由 | 必要な対応 |
| --- | --- |
| `NotRetired` | 現用・準備中の画像の利用を終了する |
| `CodeLeases { count }` | タスク・候補・Waker・登録処理等の所有を終了する。件数は要求数ではなくコード参照数 |
| `HostCalls { count }` | factory・validator・poll・drop・登録処理の呼び出しがホストへ戻るまで待つ |
| `OwnerThreadRequired` | その画像をロードしたスレッドで操作する |
| `UnvalidatedCandidate` | 検証を完了できなかった画像は保持する。拒否されたことを終了確認に使わない |
| `ExternalQuiescenceUnconfirmed` | 外部 Waker の帰還、TLS、依存先等をホストが確認し、明示回収の契約を満たす |
| `Reclaiming` | close とその終了処理が戻るまで再回収しない |
| `CloseUncertain` | close が失敗した画像とコピーを保持し、消費したハンドルを再利用しない |
| `RegisteredLocals { count }` | 登録した世代ローカル値の所有が終了し、破棄が戻るまで待つ。不確定な残留も数える |
| `LocalCleanupFailed { count }` | 登録値の破棄に失敗した画像を保持する。同じ破棄関数は再実行しない |

回収APIは SDK 側の条件が不足すると `ReclaimResult::Retained { reasons }` を返します。
成功時は `Released { copy_removal_error }` で、ローダー参照の解放後にコピーを削除します。
コピー削除の失敗は別に報告し、close 成功を取り消しません。`CloseFailed` は解放状態が不確定であり、
同じハンドルを再度 close しません。回収済み・不明な ID は `NotFound` です。

`Released` は SDK のローダー参照を解放した結果です。OS による画像の常駐、別のロード参照、実メモリ量は別に確認します。
`released_native_images` と `close_failure_count` で累積件数を観測できます。

## コード保持と処理の終了

コードを保持しても、タスクの停止や借用先の寿命延長は保証しません。
`spawn_child` と `register_activity` で明示登録した処理は、ルートの終了待ちに含めます。
外部処理のアダプターは、停止要求に加え、join・購読解除・実行中コールバックの帰還を確認してください。
詳しい契約は [管理する子処理とコールバック](MANAGED_ACTIVITIES.md) を参照してください。

ロジックが独自に生成したスレッド・ランタイムタスク・外部コールバックは自動発見しません。
アプリケーションは、それらがホスト資源や借用先にアクセスし続けないように管理する必要があります。
低水準 `unsafe FfiFuture::new` も、コード・データの寿命を呼び出し元が保証する境界です。

## 保持量と掃除

更新を重ねるとコード・静的データ・コピーが蓄積します。
共有 `GenerationBudget` は累積ロード試行数を制限し、`retention_snapshot()` は保持画像を観測します。
明示回収に成功しても、累積試行数は減りません。
予算の枯渇後は開発プロセスを再起動してください。メモリ量の上限を保証する仕組みではありません。

コピー先はユーザー一時領域の `reknot/managed-v1/sessions/<SessionId>/copies/` です。
終了後のコピーは、セッション lease の排他を確認する `reknot-cache clean --apply` で掃除します。
これは終了済みセッションのファイル操作であり、実行中コードのアンロードは行いません。
旧 PID 形式・リンク・不明な対象は自動削除しません。[観測と掃除](RETENTION_AND_CLEANUP.md) に条件をまとめています。

## 検証

`native_routing` は Router の破棄後も旧要求を poll・破棄できることと、外部コールバックの終了待ちを検査します。
`cache_lifecycle` は未採用・拒否候補を含む保持量、Router 全破棄後の保持、別プロセスの lease と終了後の掃除を検査します。
`native_reclamation` は既定の保持、退役世代・完了済みハンドル・候補の参照、再入したホストから見た実行中の記録、
外部 Waker の最後の解放から DLL 帰還までの区間、40回の連続更新を実 DLL で検査します。
Windows と Linux 用に OS のロード状態の検査を実装し、Windows では実際のコピー削除失敗も作ります。
close 失敗後の扱いは、無効な OS ハンドルを作らず、戻り値を注入する単体試験で検査します。
Waker の所有参照と別スレッドでの wake・解放は `task` の単体テストで確認します。
Miri は実 DLL の代わりに所有・借用の経路を扱います。再実行と限界は [テスト](TESTING.md) を参照してください。
