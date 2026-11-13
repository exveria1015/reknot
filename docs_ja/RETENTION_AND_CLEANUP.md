# 保持量の観測とコピーの掃除

[English](../docs/RETENTION_AND_CLEANUP.md) | 日本語

ホスト内の DLL 参照の観測、プロセス寿命のセッション管理、明示的なコピー掃除を提供します。
ネイティブ画像は既定ではプロセス終了まで保持し、ロード予算と Handler ABI 5 の契約を維持します。
終了条件を確認できるホスト向けの明示回収は [DLL の寿命](UNLOAD_SAFETY.md) を参照してください。

## ホストで観測する

```rust,ignore
let snapshot = reknot::retention_snapshot();
println!("native_images={} recorded_copy_bytes={}",
    snapshot.tracked_native_images, snapshot.recorded_copy_bytes);
for image in snapshot.images {
    println!("{:?} {:?} {:?}: {} {:?}", image.id, image.state,
        image.outcome, image.copy.display(), image.retention_reasons);
}
println!("attempts={}/{}", router.options().generations.used(),
    router.options().generations.limit());
```

`retention_snapshot()` はこのホストの SDK 実体が保持するネイティブ参照を数えます。
現用、準備中、退役を区別し、検証拒否・候補破棄も追跡します。同じ Router 世代から prepare した複数候補にも
別々の `ArtifactId` を付けます。静的 API はネイティブ画像数には入りません。
スナップショットは値だけを所有し、画像・Future・Waker の寿命を延ばしません。

`recorded_copy_bytes` は保持する画像のコピー作成時のファイル長の合計です。
実メモリ使用量、OS のモジュール数、現在のディスク使用量とは別です。
`GenerationBudget::used()` も累積試行数であり、掃除や画像解放で減りません。
`released_native_images` は SDK のローダー参照の解放成功数です。`close_failure_count` は close の失敗数で、
失敗した画像は `close_error` を持つ不確定な保持として件数・コピー長に含めます。
これらも OS 上の画像消滅やメモリ解放量を表しません。
コピー失敗・ネイティブ open 失敗は累積件数を持ち、コピー削除エラーは直近32件と累積件数を返します。

登録型TLSの残数は `registered_locals`、破棄失敗数は `local_cleanup_failures` で観測します。
どちらも回収を止める理由になり、破棄失敗後は同じ破棄関数を再実行しません。
SDKが管理する通知と登録値の範囲は [登録型TLSとホスト所有の通知](MANAGED_LOCALS_AND_NOTIFICATIONS.md) を参照してください。

## CLI でコピーを調べる

リポジトリのルートで実行します。

```powershell
cargo run -p reknot --bin reknot-cache --locked -- inspect
cargo run -p reknot --bin reknot-cache --locked -- clean
cargo run -p reknot --bin reknot-cache --locked -- clean --apply
```

`inspect` と `--apply` のない `clean` は読み取りだけです。欠けた管理ファイルを作成せず、削除もしません。
apply は表示済みの計画を信用せず、その時点で再検査します。
外部 CLI は実行中の他プロセスのメモリ量や保持画像数を取得しません。

既定のルートは `<temp>/reknot/` です。ホストを起動する前に `REKNOT_CACHE_ROOT` で
専用のローカル絶対パスを指定できます。SDK 実体は最初のネイティブロード時にルートを確定します。
CLI／API では `--root`／`CacheOptions::root` で同じルートを指定してください。
相対パス、`..`、リンク・junction 等を経由するルートは拒否します。

```powershell
cargo run -p reknot --bin reknot-cache --locked -- inspect --root C:\scratch\reknot-cache
cargo run -p reknot --bin reknot-cache --locked -- clean --apply --max-sessions 100 --max-entries 10000
```

既定上限はセッション1024件、ディレクトリ項目65536件です。未知・旧形式の走査も項目上限に含めます。
打ち切りや読み取れない項目は `partial=true` として表示し、完全な合計とは扱いません。
件数・bytes は対象コピーのファイル長です。OS が報告する空き容量の増加を保証する値ではありません。

| 表示 | 意味 |
| --- | --- |
| InUse | 現 SDK のセッション、または lease が取得できない。他プロセスの生存を推測して削除しない |
| Eligible | 新形式・構造・権限の検査と lease 取得に成功した候補 |
| Trash | 利用終了を確認して削除専用領域へ移動済み。削除を再試行できる |
| Legacy | PID だけを使う旧形式。容量とパスの表示だけで、apply でも削除しない |
| Unknown | 不明・不完全な管理情報、未知ファイル、リンク、権限等の問題。削除しない |
| Deleted | 当該セッションの掃除が完了 |
| PartialFailure | trash の処理失敗。削除できた件数・bytes とエラーを報告し、次の apply で再試行 |

CLI の終了コードは0が完全な走査・操作、1が引数・ルート・ロック等のエラー、2が部分集計・部分失敗です。
正常な InUse／Legacy のスキップはエラーではありません。

## キャッシュの設定と保存形式

キャッシュルートの指定に使う環境変数は `REKNOT_CACHE_ROOT` だけです。
未設定なら `<temp>/reknot/` を使います。CLIで明示する `--root` は環境変数より優先します。
以前の環境変数の別名は参照しません。

セッションは `managed-v1/` 配下に置き、`session.meta` の識別子を `reknot-cache-v1` とします。
検査・掃除が認識する識別子はこれだけです。以前のソース候補のキャッシュ形式には対応せず、
以前指定していたキャッシュルートを含め、既存ファイルの自動移行・削除は行いません。

未対応の識別子は検査時に `Unknown` として報告します。`clean --apply` はそのファイルを保持し、
不完全な結果として終了コード2を返します。既に `trash` にある項目は `PartialFailure` として報告します。
PIDだけを使う項目も `Legacy` として保持します。対応する形式のセッションも、削除前に
lease・権限の検査を通る必要があります。

## セッションと排他

新しいコピーは `managed-v1/sessions/<SessionId>/copies/<ArtifactId>.<ext>` に置きます。
プロセス／SDK 実体ごとに一つのセッションを使い、
一意な ID と新規作成で上書きを避けます。PID や時計の値だけでは削除を許可しません。

ホストはセッションの `lease.lock` をプロセス終了まで排他保持します。
最後の Router の Drop、全タスクの終了、prepare の失敗でも閉じません。
子プロセスへハンドルを継承しません。SDK 自体の途中アンロードや、コピーを別プロセスのロード元として渡す使い方は対象外です。

掃除は `namespace.lock` と対象の lease を非ブロッキングで取得します。
取得できない場合は見送り、成功時だけ `trash/<SessionId>` に移して既知の通常ファイルを個別に削除します。
掃除中断後の trash はロード元に戻しません。meta は最後まで残し、残った空ディレクトリだけを最後に削除します。
再帰的な一括削除、元のビルド成果物、プロジェクトの `target/`、DB、Cargo キャッシュの掃除は行いません。

namespace の排他はセッション作成・掃除を直列化します。競合時は WouldBlock／CLI のエラーとなり、
ホストは必要に応じて後でロードを再試行します。既存セッションでの通常ロード・実行にはこのロックを使いません。
自動アンロードや自動再起動、ロード予算の再利用も行いません。

## 権限と対象環境

同じプロトコルを守るホストが使う、同一利用者の専用ローカル領域を対象とします。
新規ディレクトリは Unix では0700、新規ファイルは0600で作成します。
Windows では新規ディレクトリだけに、利用者・SYSTEM・Administrators を許す継承可能な保護 DACL を設定します。
既存ディレクトリの権限は変更せず、所有者と他主体の書き込み権限を検査して、不適格なら拒否します。
Windows の [DACL 設定 API](https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-setnamedsecurityinfow) を使用しています。

権限を確認できない場合、未知の ACL、ロック非対応、I/O エラーを終了済みと読み替えません。
共有 `/tmp` のルートが別ユーザー所有等の場合も拒否するため、必要なら自分の専用ルートを明示します。
管理外の同時書き換え、攻撃者の管理するルート、ネットワーク FS のロック動作は対象外です。
この仕組みは任意の利用者による直接の DLL ロードを検出するものではありません。

## 検証

```powershell
cargo test -p reknot --test cache_lifecycle --locked -- --test-threads=1
```

親ホスト・子ホスト・掃除 CLI を別プロセスにした試験で、次を検査します。

- 実 DLL の現用・同一世代の複数候補・拒否・破棄・退役、静的 API と試行予算の区別。
- 全 Router の破棄後も生存中は削除せず、正常終了・強制終了後だけ適格になること。
- 子プロセスが残っていても、終了した親ホストの lease を継承していないこと。
- namespace の競合、二つの掃除 CLI の同時 apply、表示後の lease 取得、壊れた／過大な meta、lease 欠落、未知ファイル、旧形式、走査上限。
- junction とリンク先の sentinel を削除しないこと、trash の途中状態、実際のファイル削除失敗と再試行。
- CLI の既定が読み取りであることと、明示 apply の削除。
- Reknotの環境変数だけを使うこと、実ホストが現在の識別子を書き込むこと、未対応形式の
  session／trashの管理情報とコピーをCLIのapplyでも保持すること。

純粋な Rust の所有・実行経路は Miri でも回帰確認しますが、ファイルロック・DACL・実 DLL の代替検証にはしません。
ignored の2関数は親から起動する子プロセスの入口です。
親テスト間のセットアップは内部で直列化し、確認対象のプロセス競合は各試験内で明示的に作っています。

実行方法と検証全体は [テスト](TESTING.md) を参照してください。
