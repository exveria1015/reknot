# 登録型TLSとホスト所有の通知

[English](../docs/MANAGED_LOCALS_AND_NOTIFICATIONS.md) | 日本語

フレームワークは、SDKの経路で管理する通知と、明示登録した世代ローカル値の寿命を管理します。
依存ライブラリ固有の終了処理は、アプリケーションや個別アダプターが実装します。
その処理が終わったことは、引き続きDLL回収の条件です。

| 対象 | SDKが管理する範囲 | 利用側が確認する範囲 |
| --- | --- | --- |
| 外部通知 | `call_async` のホストI/O通知、`HostNotification` のC通知ハンドル | DLL側のRust Wakerを外部ランタイムへ直接渡す経路、独自コールバック |
| TLS | `GenerationLocal<T>` に登録した、所有スレッドと世代に属する値 | 任意の `std::thread_local!`、管理外スレッドや依存先内部の未登録TLS |
| 依存先の終了 | `ManagedActivity` による共通の停止・join契約 | 固有の停止API、購読解除、特別な破棄順序、OS登録の解除 |

## 世代と所有スレッドに属する値

`GenerationLocal<T>` は登録型TLSの静的なキーです。値はホストの世代管理に登録し、
同じRouterの同じ世代で共有します。別Routerや新世代の値は独立します。
Router、要求、登録処理は従来どおり所有スレッドに留まり、値もそのスレッドだけで扱います。

```rust
use reknot::{GenerationLocal, RequestContext, ServiceError};

static BUFFER: GenerationLocal<Vec<u8>> = GenerationLocal::new(Vec::new);

fn append(ctx: &RequestContext) -> Result<usize, ServiceError> {
    BUFFER.with_mut(ctx, |buffer| {
        buffer.extend_from_slice(ctx.input());
        buffer.len()
    })
}
```

最初のアクセス時に初期化します。`with` は共有借用、`with_mut` は排他的な借用です。
借用はクロージャー内に限定し、値への参照を外へ返せません。借用競合と同じキーの初期化中の再アクセスは
`BUSY` です。初期化・アクセス中のpanicは `PANICKED` を返します。値や外部副作用の巻き戻しは行いません。
上限 `MAX_GENERATION_LOCALS` は1世代64キーです。値のバイト数を制限する上限ではありません。

世代の最後の所有参照を解放すると、初期化と逆の順序で値を破棄します。
旧要求や完了済み `RequestTask` のハンドルも世代を保持するため、タスクを破棄し、
必要に応じて `release_retired_generations()` を呼びます。値の破棄はDLLが有効な間に行い、
破棄関数がホストへ戻るまでコードを保護します。破棄開始後は、その世代のローカル値への再アクセスを拒否します。

破棄がpanicすると、その状態を不確定としてコードを保持し、同じ破棄関数を再実行しません。
ネイティブ画像の `registered_locals` と `local_cleanup_failures`、保持理由の
`RegisteredLocals` と `LocalCleanupFailed` で観測できます。ここでの登録数には不確定な残留も含みます。
終了済みであっても他のDLL所有物が残る場合は、それらの所有も解放する必要があります。

この方式は任意のTLSを列挙する機構ではありません。世代専用スレッドの生成・joinや、
依存先固有のTLS終了処理も自動追加しません。依存先の終了が不明なら、KeepLoadedを継続します。

## DLLを呼び出さない外部通知

SDKの `ctx.call_async` / `call_service_async` は、ホストI/Oにホスト所有の通知を渡します。
I/O側が保持するWakerの生成・clone・wake・dropはホストコード内で完結します。
ホストexecutorのWakerも、リロードする世代とは独立して有効なものを使ってください。

1. 要求のpoll時に、所有スレッドのexecutorを通知先として更新します。
2. 外部I/Oの通知は、そのexecutorを起こします。DLL側の処理は通常の管理されたpollで進みます。
3. ルートが終了しても登録処理が残る間は、通知を有効に保ちます。キャンセル後もホストが終了待ちを進めます。
4. 登録処理まで終了した時点で通知先を切り離します。遅延した通知やclone・dropは、DLL回収後もホスト側だけで完結します。

通知と終了が競合した場合、切断前に取得したホスト通知が届くことがあります。
終了済みの要求をDLL内で再実行する経路は作りません。未完了の登録処理を、通知の切断だけで終了済みにもしません。
ホスト側の通知ゲートは、executorへの通知とWakerの最終破棄で生じるpanicを別々に捕捉します。
通知中に要求が終了し、通知先が切り離される場合も対象です。unwindするpanicへの対処であり、
`panic=abort`からは回復しません。

独自のC通知経路には `ctx.host_notification()?` を使えます。
`HostNotification::as_raw()` は借用した `HostWaker` 記述子を返します。
外部側が保持する場合は、そのvtableの `clone` で所有ハンドルを取得し、各ハンドルに対して
`drop` または所有を消費する `wake` を一度だけ呼びます。`wake_by_ref` は所有を消費しません。
Rustのラッパーは所有スレッドに留め、外部へはC通知ハンドルを渡します。
DLL内でRust Wakerへ包み直してから外部へ渡す経路は、別途そのDLLコードの寿命を保証してください。

## ABIと回収契約

Handler ABI 5とタスク制御ABI 2の既存レイアウトを維持しています。
通知と登録値は、タスク制御テーブルの末尾にある任意のGeneration services拡張ABI 1を使います。
SDKはテーブルのサイズと拡張ヘッダーを検査してから拡張コールバックを読みます。
古いホストや不明な拡張では、新しいAPIは `UNSUPPORTED_OPERATION` です。
`call_async` は古いホストの場合、従来の保持契約を使う通知経路へ戻ります。

これらの管理が完了しても、依存先の独自処理、未登録TLS、管理外のコールバックが
終了したことまでは証明しません。`unsafe reclaim_native_image` の呼び出し元が確認します。
既定のKeepLoadedと、回収成功で返却しない累積ロード予算を維持します。

## 検証

```powershell
cargo test -p reknot --test generation_locals --locked
cargo test -p reknot --test native_managed_generation --locked -- --test-threads=1
```

静的試験では値の共有・分離、借用、panic、古いテーブルと不明な拡張の拒否を検査します。
実SDKから作ったDLLでは、キャンセル後の終了待ち、登録値の破棄、DLL回収後の遅延通知、
破棄失敗時の理由付き保持を検査します。WindowsとLinuxのCIで実行します。
Rustの所有・借用・通知競合はMiriの両モデルでも検査し、実ローダーの試験とは分けます。
