# ホスト資源・DB の移行

[English](../docs/HOST_RESOURCE_MIGRATION.md) | 日本語

`restart_with_commit(candidate, migrate_checkpoint, commit)` と、SQLite＋ホスト内キャッシュを
同時に切り替える実 DLL サンプルを提供します。SQL・排他・復旧方針はアプリケーションが所有します。
コアに DB 依存や汎用マイグレーション機構は追加していません。Handler ABI 5 は変更ありません。

## 旧新コードを共存させる条件

| 更新 | このサンプルの方針 |
| --- | --- |
| コードだけ変更し、DB 操作・値の単位・スキーマを維持 | 同じ DB v1 サービスを割り当てて Drain。旧要求と新要求が同じ資源を使う |
| 列名・値の単位・ホスト内状態を変更 | DB v2 サービスを準備し、旧処理を Restart で終了してから移行する |
| 旧コードへ戻す | 永続スキーマに対応するコードを選ぶ。v2 更新後の v1 への自動逆変換は行わない |

主サービスはホスト所有の Session のまま維持し、追加サービスの binding 1 に DB を割り当てます。
互換版 DLL は DB v1、非互換版は DB v2 を要求します。
`prepare_with_environment` で新しい DB アダプターを指定できるため、同じ Router で切り替えられます。
主サービスそのものの ID／版を変える機能ではありません。

同じサービス版であることはデータ互換性の証明ではありません。旧新の読み書きの意味が一致することを
アプリケーションが確認します。DB v2 に移行後、旧 v1 アダプターは CONTRACT_MISMATCH を返し、
さらに旧 ResourceId を revoke すると REVOKED になります。

## 確定フックの順序

```rust,ignore
let candidate = unsafe {
    router.prepare_with_environment(new_dll, next_environment)
}?;
let generation = router.restart_with_commit(
    candidate,
    |_, checkpoint, _| Ok(checkpoint.clone()),
    || database.migrate_to_v2(|_| Ok(())).map_err(|error| error.service),
)?.await?;
```

1. 候補を検査し、受付を停止する。
2. 現世代と旧 Drain 世代の管理ルートへ停止を通知し、子処理・外部処理も join する。
3. 全ワーカーのチェックポイントを変換・検査し、新しい Future を全件生成する。
4. サービス割り当てを再検査し、`commit` を一度呼ぶ。
5. 成功なら世代・ワーカーを切り替え、開始前の受付状態を復元する。新版の本体は次の poll で動く。

`migrate_checkpoint` は所有バイト列の純粋な変換に使い、資源の変更は `commit` に置きます。
未 poll の factory はアプリ状態を変更せず I/O を開始しない契約です。
`commit` は準備済みのサービスを失効させてはいけません。旧 DB ID の revoke は成功後に行います。

通常の `restart` は成功する空のフックを使い、従来の動作を維持します。
チェックポイント／factory／join が失敗した場合、フックは呼ばれません。
Pending の Restart を Drop してもフックは呼ばれず、受付は停止したままです。
通常要求はホストの Executor が並行して poll し、停止応答を進めます。自動再実行はしません。

フックの Err は `RouteError::HostCommit(error)`、unwind は HostCommit(PANICKED) です。
旧世代・停止時のチェックポイントを保持し、受付を停止します。コアはフックの副作用を巻き戻しません。
失敗後の再試行は pause 中に始まるため、成功後も `resume()?` を明示的に呼びます。

## SQLite とホスト資源の排他

サンプルの `Database` は接続・`MemoryState`・状態フラグを一つの `Arc<Mutex<Store>>` に置きます。
DB v1／v2 アダプターとアプリの管理操作はこの同じロックを使います。
`try_lock` の競合は BUSY、未確定の不整合は CLOSED とし、部分移行を観測させません。
SDK の Services／Router を別スレッドへ移す実装ではありません。
別所有スレッドにアダプターを登録する場合も共有するのはホスト内の `Database` です。

移行は次の順序です。

1. 既知の `PRAGMA user_version`・対象行とホスト内キャッシュを照合する。
2. 新しい `MemoryState::V2` を準備し、整数変換の overflow を検査する。
3. `BEGIN IMMEDIATE` を開始し、書き込みロック取得後にも移行前データを照合する。
4. `value` 列を `value_minor` に変更し、値を100倍、`user_version` を2にする。
5. トランザクション内の結果を照合して COMMIT する。
6. ロックを保持したまま、準備済みのホスト内状態を代入してアクセスを再開する。

列・値・永続版は一つの SQL トランザクションで更新します。
永続 COMMIT 後のキャッシュ公開には、追加の SQL・割り当て・ユーザーコールバックを挟みません。
既に v2 なら検証後に成功し、再試行で100倍変換を重ねません。
各通常更新も同じロックとトランザクションを使い、返信容量を副作用の前に確認します。

SQLite のロックは別接続にも働きます。BEGIN IMMEDIATE は別 writer、COMMIT は別 reader により
BUSY となり得ます。この例は rollback journal と busy timeout 0 を使い、待ち続けず失敗を返します。
詳細は [SQLite のトランザクション](https://sqlite.org/lang_transaction.html) を参照してください。
無関係な外部 writer をホット更新全体で停止させる機能ではありません。
共有する全 Router・サービス利用者・外部接続の停止や版の調整はアプリケーションが管理します。

この例の SQL 呼び出しと確定フックは同期です。busy timeout 0 はファイル I/O の待ち時間を制限しません。
排他と復旧の確認用であり、フレームや高負荷サーバーの短時間処理を保証する実装ではありません。
通常の DB 操作を長時間待つアプリでは、既存の非同期サービス／ホスト I/O スレッドを使う構成を検討します。
この確定フック自体を非同期化する機能は追加していません。

## 失敗と復旧

| 状況 | DB／キャッシュ | Router／次の操作 |
| --- | --- | --- |
| SQL 変更中の Err・panic、BEGIN／COMMIT の BUSY | ROLLBACK を試み、autocommit と移行前の版・対象値への復帰を確認できた場合だけ Ready | 旧世代で pause。競合解消後に同じチェックポイントから再試行できる |
| 旧状態への復帰を確認できない | RecoveryRequired にして CLOSED。旧キャッシュで受付を開かない | pause を維持し、永続状態を確認する |
| 外部変更で版・値とキャッシュが不一致 | RecoveryRequired にして CLOSED | 対応する既知のスキーマを復元／選択し、明示的に `recover()` する |
| `recover()` 成功 | 既知の永続版・対象値からキャッシュを再構成 | 対応 DLL とサービス版を選び直す。recover 自体は Router を再開しない |

rusqlite の Transaction は既定で Drop 時に rollback を試みますが、その成功だけを仮定しません。
残存トランザクションには明示的に ROLLBACK し、接続状態とこのアプリの対象データを照合します。
この確認は小さな ledger スキーマ用であり、任意の DB の完全な整合性検査ではありません。
[rusqlite Transaction の契約](https://docs.rs/rusqlite/0.41.1/rusqlite/struct.Transaction.html) も参照してください。

DB の durable COMMIT と Router のメモリ上の切り替えは、プロセスクラッシュに対する一つの原子操作ではありません。
その間にプロセスが終了した場合、次の起動は永続版からキャッシュ・対応コードを選びます。
サンプルの `open`／`recover` は既知の版のキャッシュ復元を提供し、DLL の自動選択は行いません。
未知の版は拒否します。COMMIT 成功後の任意の副作用や、新版の更新後の自動ダウングレードも対象外です。

## 実行と検証範囲

```powershell
cargo run -p db_migration_host --locked
cargo test -p db_migration_host --test database --locked
cargo test -p db_migration_host --test native_migration --locked -- --ignored --test-threads=1
```

サンプルは DLL を3種類ビルドし、`target/db-migration-demo/<実行ID>/application.sqlite` を新規作成します。
既存のアプリ DB は使用しません。出力したファイルは再確認用に残します。
`db_migration_host` のみに rusqlite 0.41.1（bundled SQLite）を追加したため、C コンパイラも必要です。

ネイティブ試験は次を検査します。

- 実 DLL で互換 Drain 中の旧新要求、非互換 Restart の停止待ち、失敗後の再試行とワーカー状態の継続。
- 列変更直後・データ変更後・COMMIT 直前の障害と panic に対する rollback、キャッシュ保持。
- 別接続による実際の BEGIN／COMMIT BUSY、別スレッドからの移行中呼び出しの BUSY。
- 旧アダプター／失効 ID の拒否、未知の版の遮断、明示復旧、overflow と返信容量の事前検査。
- 接続を閉じた後の再 open で、v2 の版と値が永続化されていること。

コアの `host_commit` はフックの順序・一回性・未呼び出し・失敗／panic を検証します。
Miri の対象はこのコア部分です。SQLite の C コード・ファイル I/O・実 DLL はネイティブテストで分けて確認します。
強制終了やディスク障害の注入、他 DB、分散トランザクション、
長時間の非同期移行、フレーム時間の保証はこの検証に含みません。
