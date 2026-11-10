# 型付き Restart と状態移行

[English](../docs/TYPED_RESTART.md) | 日本語

`ModuleHost` から、登録ワーカーの協調停止、管理する処理の終了待ち、型付きチェックポイントの
移行、新世代での再生成を行えます。通常の要求に対する既定の方式は引き続き Drain です。
Restart が再生成するのは明示的に登録したワーカーだけで、通常の呼び出しを再実行することはありません。

## ワーカーを宣言して登録する

共有する protocol にハンドラーと、モジュール全体で使う1種類の状態契約を定義します。

```rust
use reknot::typed::{Handler, Module, WorkerState};

pub const RUN: Handler<(), Result<(), String>> = Handler::new("app.worker", 1);
pub const STATE: WorkerState<u64> = WorkerState::new("app.counter", 1);

pub fn module() -> Module {
    Module::new("app", 1).with(RUN).with_state(STATE)
}
```

複雑な状態には、`#[reknot::schema]` と Serde の derive を付けた構造体・列挙型を使います。
モジュール内の全ワーカーがこの状態契約を使います。複数種類の状態は列挙型で表現できます。
ID はアプリ側で選ぶ `WorkerId` です。ワーカーの戻り値は `Result<(), String>` とし、
業務上のエラーは Restart を失敗させます。詳細は `worker_error(id)` で取得できます。

logic crate では protocol を import し、状態を明示的に復元・保存します。

```rust
use app_protocol::{module, RUN, STATE};

#[reknot::handler(RUN)]
async fn run(context: reknot::RequestContext, _: ()) -> Result<(), String> {
    let count = STATE.restore(&context).map_err(|e| e.to_string())?;
    context.cancelled().await;
    STATE.save(&context, &count).map_err(|e| e.to_string())?;
    Ok(())
}
reknot::export_handlers!(module(), [run]);
```

この例は停止を待つだけです。実際のワーカーでは更新した状態を保存し、停止要求に応答します。
子処理は `RequestContext` で登録してください。ホストは
`host.start_worker(WorkerId(1), RUN, (), STATE, &0)?` でルートを登録し、`poll_workers(cx)` を進めます。
`DevModule::update()` は登録ワーカーも駆動します。`stop_worker` は協調停止を要求し、
`remove_worker` は登録を削除して、残った後処理をキューへ移します。削除後も後処理を進めてください。
`worker_status` は未完了または未登録の場合に `None` を返します。
`worker_checkpoint(id, STATE)` は保存済みの状態を型付きで返します。

## 状態を変えずに Restart する

信頼できるネイティブ候補には `unsafe { host.restart(path)? }`、常駐する静的テーブルには
`unsafe { host.restart_api(api)? }` を使います。事前検査に成功すると受付を止め、所有する
`ModuleRestart` Future を返します。この Future を所有スレッドで await/poll しながら、
外部で所有する通常の呼び出しと bridge driver も進めてください。それらの実行器を待機で塞がないようにします。

順序は、候補検査 → 受付停止 → 全管理ルートへの停止通知とルート・子処理の終了待ち →
チェックポイント変換と新ワーカー factory の準備 → ホストの最終 commit → 新世代の適用と
元の受付状態の復元です。commit より前に新ワーカー本体は poll しません。切り替え後にワーカーを駆動します。

既存の `HandlerHandle` と `HandlerClient` は引き続き使えます。受付済みの呼び出しは世代を維持し、
協調停止の通知を受けます。ブリッジの待機中の要求は受付停止中もキューに残り、実際に受け付けた時点で
世代を選びます。切り替え成功または明示的な `resume()` が driver を起こします。
受付停止中の直接呼び出しは、元のルーティングエラー `Paused` で失敗します。

## 状態を明示的に移行する

新しいロジックで、新版の `WorkerState<NewState>` を宣言します。ホストはデコード用に旧型・旧契約を残し、
変換先の新型も定義します。

```rust
let migration = NEW_STATE.migrate(|_id, old_checkpoint| {
    let old = OLD_STATE.decode(old_checkpoint)?;
    Ok(NewState { total: old })
});
let restart = unsafe { host.restart_with(new_dll_path, migration)? };
// Poll restart concurrently with ordinary calls and bridge drivers.
```

静的テーブルには `restart_api_with` を使います。ハンドラーの入出力契約は維持し、その変更には
ホストの再ビルド・再起動が必要です。移行関数は副作用を持たせないでください。一部のワーカーを
変換した後で失敗する場合があり、再試行では保持した旧チェックポイントから変換し直します。
一部だけ移行したワーカー一覧を適用することはありません。

`migration.with_commit(|| -> Result<(), ServiceError> { ... })` で同期的な最終 commit を追加できます。
終了待ちと準備の成功後、新しい本体の実行前に呼びます。トランザクションの永続化・rollback は
アプリ側の責任です。callback の失敗は外部副作用を取り消しません。[ホスト資源の移行](HOST_RESOURCE_MIGRATION.md)
を参照してください。型付きチェックポイントは状態の契約と値の両方を含み、1 MiB の上限には
この包み全体が含まれます。デコードではキーと構造契約を検査してから、アプリの値のデシリアライザーを呼びます。

## 失敗・中断から復旧する

| 結果 | 状態と次の対応 |
| --- | --- |
| 事前検査で候補を拒否 | 旧世代と受付状態を維持する。契約エラーを確認する |
| ワーカーや登録した子処理が未完了 | Restart は Pending のまま。停止処理を進めるか、ホストの timeout 方針を適用する |
| ワーカー・移行・commit のエラー、準備の失敗 | 旧世代・旧チェックポイントを保持し、受付停止を維持する。`Error::Restart` を調べて再試行または明示的に復旧する |
| Restart Future の Drop・timeout | 受付停止を維持する。強制破棄・要求の再実行・自動 resume は行わない |
| 成功 | 新ワーカーを準備し、開始前の受付状態へ戻す。開始時に停止中だったホストは停止を維持する |

`host.resume()?` は復旧後に受付を開きます。ワーカーの再生成や停止通知の取り消しは行いません。
ワーカーの poll へ再入している間に Future を Drop した場合も、次の所有側の保守・復旧呼び出しで
内部フラグを後処理します。必要に応じてワーカー・通常の呼び出し・ブリッジ・キャンセル後の後処理を
進めてください。SDK は停止期限を決めず、応答しないワーカーを強制終了しません。

## 開発中の自動 Restart を選ぶ

`DevModule::start` の前に `DevOptions::with_reload_mode(ReloadMode::Restart)` を指定します。
保存後のビルドで `ReloadEvent::Restarting`、続いて `Reloaded` または `Rejected` を通知します。
`update()` が待機中の Restart と接続したブリッジ・ワーカーを進めます。
状態契約は同じものに保ち、状態スキーマの変更には明示的な移行 API を使います。
停止開始後に失敗した場合は、受付停止を維持します。

Restart 待機中の編集は、その Restart が完了してから処理します。開始済みの停止は巻き戻しません。
生成したアプリと既存ホストの既定値は `ReloadMode::Drain` です。この文書は型付き API 自体を扱います。
実フレームワークのイベントやライフサイクルへの接続は、別途アプリで検証してください。

[契約エラー](CONTRACTS_AND_SCHEMAS.md) · [低レイヤーの Restart](RESTART_AND_MIGRATION.md) · [ドキュメント一覧](INDEX.md)
