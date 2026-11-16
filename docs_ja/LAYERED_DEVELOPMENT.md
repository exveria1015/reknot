# 二層開発と管理COFFイメージの回収

[English](../docs/LAYERED_DEVELOPMENT.md) | 日本語

Windows x64 MSVC、PowerShell 7、コンパイラーを一致させた常駐std/supportを使う実験的な経路です。
ホストの `jit-dev` featureは `jit` と `notify` を含み、`native` / `typed` は要求しません。
logic/supportの依存構成は[コード所有権ガイド](JIT_CODE_OWNERSHIP.md)に従います。Handler ABI 5は維持します。

## 起動と進行

```rust,no_run
use reknot::HostResources;
use reknot::jit::development::{LayeredDevOptions, LayeredDevSession};

# fn example() -> Result<(), String> {
let mut options = LayeredDevOptions::new(
    "project/Cargo.toml", "my_logic", "my_support",
    "reknot/scripts/build-layered-jit.ps1", "project/target/layered-cache",
);
options.toolchain = "stable".into();
let resources = HostResources::new(); // ここでアプリケーションのサービスを登録。
// projectの生成元・依存・将来の編集を含むネイティブ実行の信頼契約。
let mut session = unsafe { LayeredDevSession::start(options, resources)? };
// 既存の所有スレッドのイベントループから定期的に呼ぶ。
if let Some(event) = session.poll_update() {
    eprintln!("{:?}: {:?}", event.state, event.diagnostic);
}
// 最初のActivatedまではrouter()がNone。要求のdispatch/pollと
// poll_cancelled_requestsは、通常どおりホストのexecutorで進める。
session.shutdown()?; // 終了時にはビルドworkerの終了を明示的に待つ。
# Ok(()) }
```

`start` の初期探索・準備は同期処理です。初回ビルドより先に監視を登録します。
その後の入力探索、ハッシュ、Cargo、成果物取得は常駐ホストの単一workerで処理し、
`poll_update` はworkerを待ちません。リンク、entry、契約検証、有効化は所有スレッドで実行します。
実行中ジョブは一件、保留は最新の観測revision一件、結果チャネルは容量一件です。
子プロセスから取得する出力と診断を制限し、セッションは最新のイベントだけを保持します。

既定debounceは50 msです。新要求は新世代へ送り、受付済み要求と後始末は元のコードで完了させます。
`snapshot().has_active_generation` で初回失敗と旧世代の継続を区別できます。
`BuildFailed`、`Rejected`、`Superseded`、`WatchUnavailable`、`CacheFull`、
`BudgetExhausted`、`WaitingForRetirement`、`RetentionBlocked`、`RecoveryRequired`、
`RestartRequired` は現用Routerの受付先を変更しません。`CancelOldest` を選んだ場合は、
現用世代の受付を維持しつつ、退役世代の要求をキャンセルすることがあります。

ロード前と有効化直前に観測revisionを照合します。入力グラフの変更時は監視を再登録し、
新しい監視下で再ビルドします。入力探索が成功していれば、成果物の生成に失敗した場合も監視を更新し、
新しく追加した依存側の修正・保存だけで再ビルドできます。
ディレクトリーの置換、監視異常、再走査通知も再登録・全確認の対象です。
これは観測した通知の順序であり、ソース全体の原子的スナップショットではありません。
遅れて届いた変更通知は次の更新に属します。実行後に古くなった候補も寿命policyに応じて保持・計上します。

## 入力・土台・キャッシュ

共通watcherは推移的なローカル依存、manifest、lockfile、toolchainファイル、Cargo設定、
明示的な `additional_inputs` を監視します。`LayeredInput` には `Logic` / `Foundation` の所属を指定します。
build scriptやincludeの外部入力は完全には推定できません。`declared_inputs_complete` の既定はfalseで、
毎回Cargoで土台を再確認します。既知の入力をすべて申告できる場合にtrueにするとlogicのみの再利用を許可します。
不明な入力をlogicだけの変更と見なさないでください。

土台再利用キーは依存解決、compiler、builder、固定した子プロセス環境、設定、申告した土台入力を
ハッシュします。このキーは再利用候補の選択用で、実行の判定には実際の `foundation_id` を使います。
再利用失敗時は同じrevisionで一度だけ全確認へ切り替えます。stderrの文字列から土台変更を推定しません。
土台IDの変更はlogic entryより先に `RestartRequired` を返し、旧Routerを継続します。
プロセス再起動や接続移行は行いません。ソースを元へ戻してもDLLのバイト列が一致するとは限らず、
実際のIDが一致した場合だけ通常更新へ復帰します。

Cargoはmanifestのディレクトリーから、起動時に捕捉した子プロセス環境で実行します。
環境・設定を変更した場合は `reconfigure(toolchain, features, no_default_features)` で明示的に再設定します。
`retry()` はエンジンを作り直さず全確認を再要求します。

キャッシュは再利用する `cargo/`、不変の `foundation/` と `objects/`、
一意な `sessions/<id>/jobs/<id>/` から成ります。同じCargo出力先のbuilderはwriter lockで直列化し、
workerは実際に終了するまでセッションleaseを保持します。schema 2は変更しません。
版1の `build-result.json` で生成結果をジョブと関連付け、`build-receipt.json` に取得済みmanifestのハッシュ、
土台・COFFの同一性と時間を記録します。receiptは信頼や回収許可の証明ではありません。

`max_cache_bytes` の既定は4 GiBです。生成前に現使用量を確認し、不変コピーの公開前にも必要量を保守的に確認します。
容量不足時は公開を拒否し、ファイルを自動削除しません。ただしビルド中のCargo可変出力やエラーreceiptは閾値を
超える場合があります。生成・公開を止めるガードであり、OSのディスクquotaではありません。ファイル掃除は独立操作です。

`shutdown` は結果の公開を止め、実行中ビルドの終了を待つ同期操作です。
Dropは公開を停止してworkerを切り離し、Cargoが終了済みだとは見なしません。

## KeepLoadedとManaged

`CoffPolicy` はコードの寿命を二つに分けます。

| 方針 | 退役したlogic | 容量の扱い |
| --- | --- | --- |
| `KeepLoaded` | 要求完了後もプロセス終了まで保持 | 累積ロード・受付枠を使い、再起動で更新余地を戻す |
| `Managed` | 追跡対象の所有と実行中呼び出しがすべて終了した後に回収可能 | 実保持イメージ数・ページ量を制限し、回収成功で容量を再利用 |

Managedのロードは、エンジンとRouterの累積32試行枠を消費しません。
`load_attempts()` は全試行の診断用カウンター、`keep_loaded_attempts()` は
`CoffLimits::load.max_generations` に計上するKeepLoadedの試行数です。
native DLL、raw COFF、LLVM、static/custom Routerの既存の計上は維持します。
Managedはプロセス終了後のデータ永続化を表さず、すべてのイメージを回収できる保証でもありません。

## 管理寿命の追加契約

通常の `LayeredEngine::load` と `LayeredDevSession::start` はKeepLoadedです。
回収対象にする場合は、**entry実行前**に追加のunsafe契約を受け入れます。

```rust,no_run
use reknot::jit::{CoffLimits, LayeredArtifact, LayeredEngine};
# fn example() -> Result<(), Box<dyn std::error::Error>> {
let trusted = unsafe { LayeredArtifact::read("logic.obj.json")?.into_trusted() };
let mut engine = LayeredEngine::open(&trusted, CoffLimits::default())?;
let managed = unsafe { trusted.into_managed() }; // 管理profile v1の追加契約。
let handler = engine.load_managed(managed)?;
let image_id = handler.coff_image_id(); // 診断専用。コード参照を再取得できない。
# let _ = image_id; Ok(()) }
```

logicのコード、静的データ、vtable、破棄関数、panic payloadを追跡可能な所有の内側に閉じます。
所有スレッドのRouter呼び出し、GenerationLocal、ローカル子処理、常駐ホスト通知が基本の対応範囲です。
未管理thread、callback、logic Waker、hook、atexit、動的TLS、supportへ保存した参照を残してはいけません。
依存、entry、validationもこの契約に含みます。

外部 `ManagedActivity` はアダプターごとの監査が必要です。`Ready(Err)` を含むReadyは、
callback/Wakerの最終帰還、threadのjoin、登録の閉鎖、所有するlogic値の破棄を確認した状態を表します。
解除要求、参照数、timeout、活動失敗の確認操作だけでは終了証明になりません。
終了不明はPendingのまま、panicでpoisonされたアダプターは保持します。
試験ではWakerを破棄した後の末尾をbarrierで保留する遅延起動・thread joinのfixtureを検証します。
その結果を任意の外部runtimeへの対応保証に広げません。

開発セッションでは `unsafe start_managed` で将来のすべての編集にも追加契約を適用します。
公開policy名は `Managed`、成果物の寿命契約profileは引き続きversion 1です。
manifestの自己申告による管理型への昇格はありません。

## Managedの容量待ちとキャンセル

`LayeredDevOptions::managed_reload` は `start_managed` にだけ適用します。
既定は `ManagedReloadOptions { max_live_generations: 2, pressure: ManagedPressure::Wait }` です。
実保持数には、このセッションが公開した現用イメージ、ロード済み候補、保持中の退役イメージを含みます。
現用ハンドラーで応答を続けながら候補を検証するため、2枠以上が必要です。
他controllerのイメージも、`CoffLimits` の所有スレッド合計数・bytesには含まれます。

```rust,no_run
use reknot::jit::development::{LayeredDevOptions, LayeredDevSession, ManagedPressure};
use reknot::HostResources;

# fn example(mut options: LayeredDevOptions) -> Result<(), String> {
options.managed_reload.max_live_generations = 2;
// 未完了の旧要求を終了させてよい場合に選択する。
options.managed_reload.pressure = ManagedPressure::CancelOldest;
let mut session = unsafe { LayeredDevSession::start_managed(options, HostResources::new())? };
// 所有スレッドのループでpoll_updateと、所有するRequestTaskのpoll・Dropを進める。
# session.shutdown()?; Ok(()) }
```

- `Wait` は旧要求を維持し、コードの所有が終了するまで `WaitingForRetirement` として待ちます。
  timeoutや自動キャンセルはありません。
- `CancelOldest` は容量不足で更新できないとき、このセッションの最古の退役イメージに属する
  rootのキャンセルを要求します。現用イメージと他controllerは対象外です。
  rootの破棄はそのexecutorによる次のpollで行います。その後も活動のstop/join、実行中callbackの帰還、
  localsの破棄、完了ハンドルのDropを待ちます。副作用を巻き戻したり、実行中のnative呼び出しを中断したりしません。

Routerを直接使う場合は `cancel_retired_generation(id)` で同じキャンセルを要求できます。
戻り値は新たに要求したroot数です。キャンセルされたrootは後始末の完了後に `RunStatus::Error` で終了します。
外部に保持されたハンドルや未追跡の所有を、この操作で消すことはできません。

Managedの `poll_update` は、切り離された後始末、Router登録worker、退役世代の管理情報整理と、
**このセッションのイメージIDだけ**を対象とした回収を進めます。定期的に呼び出してください。
回収専用のbackground処理はありません。`poll_update_with_context(cx)` なら後始末とworkerが
ホストexecutorを起こせますが、ソース変更・ビルド完了の検出には引き続き定期pollが必要です。
外部に所有する `RequestTask` のpollとDropは、そのexecutor側で行います。
登録workerも状態・checkpoint参照のため完了済みtaskを保持します。
必要なcheckpointを取り出してから `remove_worker(id)` で登録を解除すると、その所有が終了します。
`collect_retired(options)` も明示保守用に残し、KeepLoadedではNoneを返します。
`shutdown` はビルドworkerをjoinしますが、要求のキャンセルは行いません。

ビルド開始前と割り当て・entry実行前に容量を確認し、解析後は必要なページ量も検査します。
ビルド済み候補が容量待ちになった場合は捕捉済み候補を一つ保持し、再ビルドや再実行をせず受付を再試行します。
待機中に新しい編集を観測した場合は、その候補を破棄して最新revisionを優先します。
KeepLoadedの累積枠を使い切った場合も、新しいビルドを止めます。`retry` で枠はリセットしません。

`RecoveryRequired` は自身のイメージが後始末・解放失敗で回収不能になった状態です。
失敗世代を増やさず、更新を停止します。`RetentionBlocked` は、自身で待てる退役イメージがない状態で
所有スレッド全体の数・ページ量が不足したことを示します。どちらも現用Routerは維持します。
ハンドルの保持、破棄失敗、外部処理の終了不明、他controllerの保持によって更新は待機し得ます。
Managedでなくなるのは累積回数による更新上限であり、寿命管理の条件は残ります。

セッションsnapshotには `policy`、両方の試行数、`live_images`、直近の `reclamation` 結果を含みます。
これらのIDや数値は診断用であり、コードの所有権を与えません。

## 明示回収と診断

要求・cancel後の後始末・活動を進め、完了済みハンドルをDropし、
`router.release_retired_generations()` でRouterの保持を終了します。その後、ロードしたスレッドで
`jit::collect_retired_coff(options)` を呼びます。controllerのDrop後も呼べます。
要求を強制終了せず、保持一覧のロック中にlogicの任意callbackを呼びません。

物理ロードごとに異なる `CoffImageId` を付けます。`coff_retention_snapshot()` はpolicy、状態、
割り当てページ量、参照、実行中呼び出し、locals、活動、失敗理由を返します。診断値は所有権を与えません。
nativeの既存retention APIは維持します。
LoadedHandler、PreparedHandler、RequestRouterから同じ診断用 `coff_image_id()` を取得できます。
公開前rollbackの解放失敗も計上します。公開情報を取得する前のため、その記録は土台IDなし・COFFハッシュ空になります。

退役、locals・活動の終了、失敗、呼び出し帰還を再確認し、Arcの排他的所有取得で新しいleaseの経路を閉じます。
所有スレッドでの保守処理だけが登録時のunwind表を解除し、`VirtualFree(base, 0, MEM_RELEASE)` を実行します。
失敗時は自動再試行せず保持し、実際の解放成功分だけ実保持予算を戻します。
最後のleaseのDropや所有スレッド終了からはコードを解放しません。スレッド終了時の未回収イメージと表は
保持を続けます。logic全件を回収してもstd/supportとそのファイル固定はプロセス寿命まで残ります。

`CoffLimits` の既定はKeepLoadedの累積32試行、入力32 MiB、一割り当て64 MiB、所有スレッド全controllerの合計で
保持256イメージ・イメージページ256 MiBです。KeepLoaded、Pinned、解放失敗も合計へ含めます。
異なる所有スレッドを含むプロセス全体のメモリー上限ではありません。
Routerの試行数は別に既定32です（開発設定の `max_router_generations`）。Managed COFFはこの枠を消費しません。
どちらのKeepLoaded累積枠も返却しません。実保持数・ページ量の上限は両方のpolicyに適用します。
解放済み世代の詳細履歴を蓄積せず、保持中一覧と累積解放件数・bytesを残します。

一回の評価を `max_candidates` 件へ制限し、巡回により後方の候補も評価します。
回収結果には解放ID、待機・保持ID、OSエラー、所要時間を返します。OS呼び出しを中断するhard deadlineは提供しません。
