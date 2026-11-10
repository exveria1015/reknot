# Restart・状態移行・世代の保持上限

[English](../docs/RESTART_AND_MIGRATION.md) | 日本語

Handler ABI 5 の停止・状態移行・上限の契約。

## 切り替えの単位

Drain は引き続き `activate(candidate)` を使う。既存実行は旧世代、新規受付は新世代となる。
Restart は `restart(candidate, migrate)` を使い、モジュールが管理する実行全体の停止を待つ。
再生成する処理は、`start_worker(WorkerId, resource, input, Checkpoint)` で明示的に登録したワーカーに限る。
通常の `dispatch` リクエストは停止対象になるが、自動再実行しない。

実行手順:

1. 候補の Router ID・基準世代・サービス契約を確認する。ここでの失敗は受付に影響しない。
2. 受付を閉じ、現世代と過去の Drain 世代に残る管理対象タスクへ停止を通知する。直前に poll されたタスクの Waker も通知する。
3. Restart Future が登録ワーカーを poll する。外部所有の RequestTask はホストの Executor が並行して進める。
4. 全管理対象タスクの Future と明示登録処理の join・解除・破棄まで待つ。登録処理の失敗は ActivityFailed、登録ワーカーの Error は WorkerFailed とする。
5. 各ワーカーの最後のチェックポイントをコピーし、移行関数で新しいスキーマへ変換する。
6. 全チェックポイントのスキーマ・サイズを検査し、すべての新版ワーカーの Future を生成する。この段階でハンドラー本体は実行しない。
7. `restart_with_commit` の場合は最終の同期確定フックを一度呼ぶ。通常の `restart` は空のフックを使う。
8. 世代・ワーカー一覧を切り替え、元の受付状態を復元する。最初から pause 中なら再開しない。新版ワーカーは次の poll から動く。

管理対象はこの Router が所有／追跡するルート実行と、そこへ明示登録した処理である。
`spawn_child` / `register_activity` の停止・join は [登録処理の契約](MANAGED_ACTIVITIES.md) に従って待つ。
未登録の独自 spawn・外部コールバックは自動追跡せず、アプリケーションが停止・join する。
任意の Future の途中の実行位置や、外部 I/O の副作用を新コードへ移す機能ではない。

## ワーカー側の停止と保存

```rust
use reknot::{RequestContext, ServiceError, ServiceKey};
const SERVICE: ServiceKey = ServiceKey::new(1, 1);
const STATE: ServiceKey = ServiceKey::new(2, 1);

#[reknot::reload_handler(SERVICE, state = STATE)]
async fn worker(ctx: RequestContext) -> Result<(), ServiceError> {
    let saved = ctx.checkpoint().ok_or(ServiceError::INVALID_ARGUMENT)?;
    let state = saved.bytes.clone(); // アプリ固有の復元をここで行う。
    ctx.cancelled().await;
    ctx.save_checkpoint(STATE, &state)?;
    Ok(())
}
```

`stop_requested()` は同期的な確認、`cancelled().await` は管理対象ルート内での待機に使う。
通知は停止要求であり、終了確認とは別。明示登録した子処理には所有スレッドの poll から停止を伝え、join を待つ。
Drop 済み要求の後処理も Restart から進める。未登録の子処理の待機は自動では起こさない。
フレーム駆動等では `poll_workers(cx)` で登録ワーカーを進め、`worker_status(id)` を確認できる。

`Checkpoint` はスキーマ ID／版を表す ServiceKey と、所有する Vec<u8> の組み合わせ。
初回の seed、DLL からの保存、移行後の状態にサイズ上限を適用する。既定は1ワーカーあたり1 MiB。
`save_checkpoint` は DLL のバッファを呼び出し中にホストへコピーする。最後の確定済みチェックポイントだけを保持する。
停止前の途中更新を自動的に巻き戻したり、未来の停止時に必ず保存できると保証したりはしない。

## ホスト側の移行

```rust,ignore
router.start_worker(WorkerId(1), session.id(), b"worker", initial_checkpoint)?;
let candidate = unsafe { router.prepare(new_library) }?;
let generation = router.restart(candidate, |worker_id, old, target_schema| {
    let bytes = migrate_application_state(worker_id, old)?;
    Ok(Checkpoint::new(target_schema, bytes))
})?.await?;
```

移行関数は `WorkerId, &Checkpoint, 新スキーマ` を受け取り、新しい所有データを返す。
シリアライズ形式と変換内容はアプリケーションが定義する。スキーマの一致はバイト列の意味の正しさの証明ではない。
同じスキーマへの Restart では `|_, state, _| Ok(state.clone())` のように再利用できる。

移行関数は共有資源への変更や外部 I/O を行わず、入力データから出力データを作る処理にする。
出力は全件の成功後に適用する。旧チェックポイントは移行関数のエラー・panic、新版 factory の失敗でも残る。
複数ワーカーの一部だけを新世代で開始することはしない。

Drain の `activate` では、サービス契約に加えて状態スキーマも一致を要求する。
Restart では状態スキーマを変更できるが、対象ホストサービスの ID／版は引き続き一致を要求する。
`restart_with_commit` は全準備後・切り替え前の確定地点を提供する。
ホスト資源本体、DB、接続のプロトコル状態の排他・変更・復旧はアプリケーション側で設計する。
[SQLite とホスト内キャッシュの移行例](HOST_RESOURCE_MIGRATION.md) では、一つのロックと SQL トランザクションを使う。
コアは副作用を巻き戻さず、DB と Router のクラッシュ時の原子性も提供しない。

## 失敗・中断と復旧

| 状況 | 結果 |
| --- | --- |
| 候補のロード・ABI・サービス契約・保持上限の失敗 | 旧受付・既存実行を維持 |
| ワーカーが終了せず Pending のまま | Restart も Pending。Future を破棄せず待つ |
| 登録した子処理・コールバックの終了未確認 | Restart も Pending。解除要求の失敗だけで破棄・切り替えを行わない |
| 登録処理が終了確認後も失敗を持つ | ActivityFailed。旧世代と失敗記録を保持し、受付を停止 |
| 移行エラー／panic、スキーマ不一致、サイズ超過、新版 factory 失敗 | 旧世代・旧チェックポイントを保持し、受付を停止した状態で Error |
| 確定フックの Err／panic | HostCommit。旧世代・旧チェックポイントを保持して受付停止。資源の rollback／遮断はアプリが行う |
| ホストがタイムアウト等で Restart Future を Drop | 受付は停止したまま。旧タスクを強制破棄・強制アンロードしない |
| 成功 | 新世代のワーカーを登録し、開始前の受付状態を復元 |

ライブラリ内部に期限のための時計や強制停止は持たない。ホストの timeout／select やフレーム制御で Restart Future を中断できる。
poll から戻らないコードに対して、同じスレッドからタイムアウトを処理できるとは保証しない。

失敗後は、元のチェックポイントを使って別候補・別の移行関数で Restart を再試行できる。
再試行時の受付は既に pause 中なので、成功後に必要なら明示的な `resume()?` を呼ぶ。
`resume` は旧ワーカーの再生成や外部副作用の切り戻しではない。`remove_worker(id)` は対象の Future を明示的にキャンセルして登録を解除する。
失敗したワーカーを Error のまま残すと再試行でも WorkerFailed になるため、回復方針に応じて登録を見直す。
登録処理の失敗については、対象世代の全処理終了後にアプリケーションが状況を確認し、
`acknowledge_activity_failure(generation)` で記録を確認済みにしてから再試行する。

## 保持上限

既定の RouterOptions は、世代枠32、チェックポイント1 MiB。

```rust,ignore
let budget = GenerationBudget::new(8)?;
let options = RouterOptions {
    generations: budget.clone(),
    max_checkpoint_bytes: 64 * 1024,
};
let router = unsafe { RequestRouter::load_with_options(path, resources, options.clone()) }?;
```

GenerationBudget は複製可能な共有カウンター。Router を作り直す場合も同じ budget／options を使えば累積の枠を維持できる。
`used()` と `limit()` で確認できる。枠がなければ Library::new より前に GenerationLimit を返し、実行中の世代へ影響を与えない。

ネイティブロードは開始前に1枠予約し、ファイルの不存在、ABI 不一致、破棄した候補も含め返却しない。
DLL 初期化コードの実行や KeepLoaded による保持を正確に取り消せないための、保守的な上限である。
静的 API の候補も有効な準備ごとに枠を消費する。候補の activate／Restart 自体では追加消費しない。
タスクが完了しても枠は戻らない。自動アンロードを成立させて枠を再利用する設計は今回含まない。

これは **共有 budget ごとのロード試行数の上限**。総メモリ量・プロセス全体・別 budget を制限するものではない。
KeepLoaded の DLL とコピーは引き続きプロセス終了まで保持され得る。枠の枯渇後は開発プロセスの再起動等で区切る。

## ABI と検証

Handler ABI は **5**、Services ABI は2、タスク制御 ABI は2、Activity ABI は1。
factory は入力・seed をコピーし、サービスとタスク制御を retain する。未 poll の factory／Future の破棄でアプリの変更や I/O を始めない契約が追加される。

実行例:

```powershell
cargo run -p reknot --example restart --locked
```

出力は `generation 1: restored 7` → `generation 2: restored 80`。旧値7を停止時に8として保存し、新版の2バイト形式へ移行する。
この例は静的リンクの API。実 DLL は native_routing テストで、旧版の1バイト値3を停止時に4として保存し、新版の2バイト値40へ移行する経路を確認する。
restart テストは複数ワーカー、旧 Drain 世代の停止待ち、移行失敗と再試行、中断、サイズ上限、panic、段階的な factory 失敗、予算共有を対象にする。
Miri と実 DLL の確認は用途が異なり、いずれもライブラリ全体の安全性の証明とはみなさない。

検証の実行手順と Miri／実 DLL／GUI の区別は [テスト](TESTING.md) を参照。
