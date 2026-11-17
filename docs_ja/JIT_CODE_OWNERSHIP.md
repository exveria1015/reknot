# コードの所有権と実験的なCOFF/JITLink

[English](../docs/JIT_CODE_OWNERSHIP.md) | 日本語

Linux x86_64 GNUにはRust製の[ELFオブジェクトバックエンド](ELF_JIT.md)もあります。
`LoadedHandler`の所有権、検証付き`LayeredEngine`、Managed契約、`LayeredDevSession`を
共有します。Linuxではseal済みmemfdで基盤を固定し、libgccの登録解除とmunmapで回収します。
以下のCOFF用コマンドやWindows APIはOS固有です。PythonビルダーとLinuxでの詳細は
ELFガイドを参照してください。

## 所有権の境界

`LoadedHandler` は検証した関数表とコードの所有者を一体で保持します。
所有スレッドで一度だけ移動する値で、Clone、Send、Sync、生の関数表を返すメソッドはありません。
`RequestRouter::from_loaded` と `prepare_loaded` が受け取ります。型付きAPIでは
`ModuleBuilder::from_loaded`、`ModuleHost::reload_loaded`、`restart_loaded`、
`restart_loaded_with` を使います。既存のDLL・静的APIの入口も維持します。

ネイティブコードを信頼してロードする境界にはunsafeが必要です。信頼するローダーが
LoadedHandlerを生成した後は、準備・契約検証・切り替え・要求をsafe APIで扱います。
ABI検証だけで任意の機械語の正しさは保証できません。`LoadedHandler::from_static_api` もunsafeで、
呼び出し側がプロセス寿命のコールバックとハンドラー契約全体を保証します。この関数はDLLを保持しません。

要求は元の関数表・Future・デストラクター・コード世代を保持します。既存タスクのpoll関数や
レイアウトを新版へ付け替えません。Future、管理対象の活動、世代ローカル値、外部に複製された
ホストWakerがコードの保持参照を持ちます。呼び出しガードも実行数に加えて保持参照を持つため、
コールバック内で最後の別ハンドルを破棄しても、ホストへ帰る前にコード所有者が破棄されません。
既存のバイト列メッセージ、資源ID、明示的な状態移行をABIとして維持します。

## 保持と上限

既定では、リンクに成功したJITの機械語・データ・依存先・登録したアンワインド表をプロセス終了まで保持します。
Rust COFF/ELFでは追加契約を受け入れた世代に限り、[管理回収](LAYERED_DEVELOPMENT.md)を選べます。
ルーター、エンジンの操作ハンドル、拒否した候補、最後のWakerをDropしてもJITコードを自動回収しません。
`mem::forget` により資源が残ることはありますが、安全性を全デストラクターの実行に依存させません。
この保持方針は、任意のRustコードを安全にアンロードできるという保証ではありません。

入口を公開する前に実体化が失敗した場合、LLVMは未公開の割り当てを解放し得ます。
その場合は登録済みのアンワインド表を先に解除します。入口を返した後は、ABIを拒否してもリンク済みイメージを保持します。

`JitLimits` は公開前のロード試行数と入力サイズを制限し、既定値はエンジンあたり32試行、
オブジェクトあたり32MiBです。失敗・拒否・破棄したロードも試行数を消費し、返却しません。
一つのリロードセッションでは同じエンジンを再利用してください。新しいエンジンは別の予算を持つため、
プロセス全体のメモリー上限ではありません。入力サイズはBSS、ページの余白、LLVM管理領域の上限にもなりません。
ルーターの既存の世代予算は、別途、候補の受け入れ試行数を制限します。
この累積制限はLLVMとKeepLoaded COFF/ELFに適用します。検証付きManagedは
実保持数・ページ量で制限し、エンジンとRouterの累積枠を消費しません。

外部コールバックは最後のWakerを解放した後にも、旧コード上の命令を実行し得ます。
参照数だけでは外部実行の終了を証明できません。safeな明示回収には、実行前に受け入れる追加の管理寿命契約が必要です。
既存のunsafeなDLL回収にも、[外部からの帰還とTLSの終了条件](UNLOAD_SAFETY.md)が引き続き必要です。

## JITの対応範囲

任意選択の `jit` featureは、後述のRust製 `CoffEngine`、[ELFガイド](ELF_JIT.md)の
Rust製 `ElfEngine`、LLVM製 `JitEngine` を提供します。
後者は別途ビルドしたLLVM 23.1.0のブリッジをWindows x86-64で使います。
ORCのObjectLinkingLayer / JITLinkを明示的に選び、ハンドラーの入口を公開する前に
COFFの.pdataをRtlAddFunctionTableへ登録します。各イメージには別のJITDylibを割り当て、
既存イメージをその場で書き換えません。

入力は信頼済みのMSVC x86-64 COFFオブジェクトであり、Handler ABI 5のhot_handler_apiを
公開する必要があります。`JitEngine::open` は自己完結したオブジェクトを受け付けます。
`open_with_runtime` を使うと、コンパイラーと一致する常駐Rust std DLLとWindowsの
ランタイムシンボルを提供します。`open_layered` は明示したsupport DLLも提供し、ロジックの
native/emulated TLSを拒否します。ブリッジABIは3です。以前のブリッジは再ビルドしてください。
Handler ABI 5とgeneration servicesは変更しません。

ブリッジはリンク済みPE、別形式・別アーキテクチャ、native TLS、CRTの初期化・終了処理の節を拒否します。
プロセス内の任意のシンボルは探索せず、未解決のランタイム依存は失敗します。
LLVMへの局所的な補正はfixtureが出力する強いローカルCOMDATリーダーを扱い、それ以外の形式は拒否します。

ローダーが受け取るのは一つのオブジェクトで、任意の依存crateをまとめたアーカイブではありません。
独自アロケーター、任意のネイティブライブラリー、CRTの初期化節、外部例外、
Cargoや型付きモジュールの自動バンドルには未対応です。
CodeViewの節は受け入れますが、デバッガー登録はありません。

## 二層ロジックと常駐support

`JitEngine::open_layered(bridge, std_dll, support_dll, limits)` は、std、指定されたsupport DLLの
順に通常のWindowsローダーでロードします。両方のパスを正規化してからDLLをロードします。
supportの依存は自身のディレクトリーとSystem32で探索し、作業ディレクトリーやPATHは追加しません。
JIT世代へのシンボル提供元はstd、そのsupport DLL、後述の固定Windowsランタイム群です。
別のエンジンがsupport DLLをプロセスへロードしても、その公開シンボルが自動的に探索先へ加わることはありません。

初期の実行fixtureは、**コンパイラーを一致させたRust dylib**でSDKを再公開し、非ジェネリックな
`parse_total(&[u8]) -> Result<u64, ServiceError>` と `parse_count() -> u64` を提供します。
JSON解析とSDKの共通処理はPE DLLに置き、ハンドラー、ローカル型、ジェネリックの実体はCOFFにも残ります。
再公開だけですべてのジェネリック処理をsupportに移せるわけではありません。
この境界は実験用のRust ABIであり、バージョン付きの公開support C ABIではありません。
ホストとの境界には既存のHandler ABIとgeneration servicesの関数表を使います。

supportとlogicには、同じrustc、target、依存、featuresを使います。prefer-dynamic=yes、panic=unwind、
codegen-units=1、lto=falseとし、Cargo/rustcで単一のlogicオブジェクトを生成します。
fixtureはstable Rustを使い、`-Z tls-model=emulated` を指定しません。ホストやRouterの依存を
logicへ静的に取り込んだり、すべての依存rlibをJITへ渡したりしないでください。
公開シンボルが不足すれば、ハンドラー公開前に失敗します。supportとstdは初期化の後段で失敗しても保持し、
変更にはプロセス再起動が必要です。この低水準APIではコンパイラー・DLLの一致を呼び出し側が保証します。
後述の検証付きRust APIは、ビルドmanifestを使ってこの一致を検証します。

logicでは `thread_local!` の代わりに `GenerationLocal` を使います。ブリッジはリンク前に未使用分も含めた
すべての節とシンボルを検査し、`.tls*`、CRTの初期化・終了節、`_tls_index` などのnative TLSシンボル、
`__emutls_*` を拒否します。std/supportではOSが管理するTLSを使えます。この検査では動的な
`TlsAlloc`/`FlsAlloc`、プロセスのhook、管理外callbackの不在は保証できず、信頼するソースと依存の寿命契約が必要です。

LoadedHandler、Routerの準備・有効化、世代の保持参照、KeepLoaded方針は共通です。
ホスト資源は切り替え後も保持し、新世代には新しいローカル値を作ります。旧Futureとローカル値の破棄は、
エンジンのDrop後も元のコードで完了させます。ローカル値の破棄に失敗した場合は再試行しません。
panicはC callbackから戻る前に捕捉し、常駐SDKがRustの呼び出しスタック内でlogicのpanicを捕捉する場合もあります。
payloadの破棄は元のコードを保持している間に完了させます。この構成はコードをアンロードしません。
後述のRustバックエンドにも同じ所有権と二層ランタイムの契約が適用されます。

```rust,no_run
use reknot::jit::{JitEngine, JitLimits};
use reknot::{HostResources, RequestRouter};
use std::path::Path;

unsafe fn start_layered(std_dll: &Path, support_dll: &Path)
    -> Result<RequestRouter, Box<dyn std::error::Error>>
{
    // All inputs are trusted and built against this exact resident foundation.
    let mut engine = JitEngine::open_layered(
        "reknot_jit_bridge.dll", std_dll, support_dll, JitLimits::default(),
    )?;
    let first = engine.load("logic-v1.obj")?;
    let mut router = RequestRouter::from_loaded(first, HostResources::new())?;
    let next = engine.load("logic-v2.obj")?;
    let prepared = router.prepare_loaded(next)?;
    router.activate(prepared)?;
    Ok(router)
}
```

要求の実行前にアプリケーションのホスト資源を登録し、リロードのセッション中は同じエンジンを保持してください。
後述のfixture/testでCargoビルドを再現できます。検証付きRustバックエンドでは
[jit-devセッション](LAYERED_DEVELOPMENT.md)でwatcherへ接続できます。

## Rust COFFバックエンド

`CoffEngine::open_layered(std_dll, support_dll, CoffLimits::default())` は、LLVMブリッジやSDKを
使わずに二層構成を実行します。Windows x86_64 MSVCの `jit` featureで利用でき、同じ
`LoadedHandler` を返します。初期化とロードは、信頼するネイティブコードと土台の完全な一致を
呼び出し側に求めるunsafe境界です。

```rust,no_run
use reknot::jit::{CoffEngine, CoffLimits};
use reknot::{HostResources, RequestRouter};

unsafe fn start_rust_coff() -> Result<RequestRouter, Box<dyn std::error::Error>> {
    let mut engine = CoffEngine::open_layered(
        "compiler/std-matching.dll", "support.dll", CoffLimits::default(),
    )?;
    let first = engine.load("logic-v1.obj")?;
    let mut router = RequestRouter::from_loaded(first, HostResources::new())?;
    let next = engine.load("logic-v2.obj")?;
    let candidate = router.prepare_loaded(next)?;
    router.activate(candidate)?;
    Ok(router)
}
```

対応範囲は単一の通常COFFまたはbigobj、ローカルなsection symbol、NoDuplicates/Any/Associativeの
COMDAT、長さを制限したweak aliasです。COMDATの選択を必要とする重複外部定義、common symbol、
weak library search、archive、PE入力、startup/TLSは拒否します。AMD64 relocationはABSOLUTE、
ADDR64、ADDR32、ADDR32NB、REL32〜REL32_5、SECTION、SECRELに対応し、他の種類や範囲外の結果は
エラーにします。デバッグセクションは検査後に破棄し、PDB生成やデバッガー登録は提供しません。

依存は指定DLLと固定ランタイム群だけから解決します。`__imp_` のslotと、実行可能な参照先への
ジャンプスタブは各イメージ内に置きます。`type_info` の特殊シンボルにはホストCRTの実際のvtableを使い、
任意のプロセスシンボル探索や仮の例外ランタイムは使いません。コードをRX、定数をR、可変データをRWに
変更してから命令キャッシュをflushし、unwindを登録します。x64 UNWIND_INFOはversion 1に対応し、
chained recordは拒否します。登録成功前にオブジェクトのcallbackを呼びません。

`CoffLimits` は `load: JitLimits`（KeepLoadedの既定32試行・入力32 MiB）と、`max_image_bytes`（既定64 MiB）を
持ちます。後者にはBSS、import slot、stub、ページ境界の余白を含めます。
`max_retained_images` / `max_retained_bytes` は所有スレッド全controllerの保持を制限します
（既定256イメージ・256 MiB）。管理回収で実際に解放した分だけ、この保持予算を返します。解析上限は8,192セクション、
1,000,000 symbol record、1,000,000 relocationです。KeepLoadedは失敗した試行も累積枠を消費します。
Managedの試行数は診断用の記録だけです。
実行前の失敗ではunwind登録を解除してからイメージを解放し、解除に失敗した場合は両方を保持します。
最初のcallbackが実行され得る時点以降、rawな `CoffEngine` はイメージとunwind表をプロセス終了まで保持します。
検証付きの `LayeredEngine::load_managed` はロード成功後、退役と終了条件を満たせば明示回収できます。
entry・ABIの拒否時はManagedでも回収不能として保持します。コントローラーのDropから回収は行いません。

`last_load_metrics` の `copy_ms` は検査、`add_ms = 0`、`materialize_ms` は割り当て・解決・再配置・
ページ保護・unwind登録の時間です。コンパイル時間は含みません。`linked_bytes` はページ境界の余白を
含まず、LLVMとRustでは保持するデバッグ情報が異なる場合があります。同一入力でロード全体と最初の応答を
比較し、編集から応答までは別に測定してください。ローダーの高速化だけでコンパイルの高速化は示せません。

## 土台と成果物の同一性を検証するAPI

通常の二層リロードには、パスを直接渡す低水準APIの代わりに `LayeredArtifact` と `LayeredEngine` を
使えます。safeな `LayeredArtifact::read` は、サイズ制限内のschema 2のCOFFまたはschema 3のELF manifestを受け取ります。
manifestのディレクトリ配下にある通常ファイルを対象にします。キャッシュが別にある場合は、manifestと同じ場所の付属オブジェクトを
使うか、許可するキャッシュディレクトリを `LayeredArtifact::read_in` で指定します。
対応する構成か検査してから、サイズとSHA-256が一致するオブジェクトのバイト列を保持します。後からパスを差し替えても、
この取得済みオブジェクトは変わりません。schema 1は検証済みと見なさず拒否します。

以下のコマンドとファイル固定の説明はWindows COFF向けです。Linuxのseal済みスナップショットと
Pythonビルダーは[ELFガイド](ELF_JIT.md)を参照してください。

`unsafe into_trusted()` で、ネイティブコードと生成元への信頼を明示します。manifestには実際の
コンパイル時の土台が正しく記録され、コード・DLL初期化処理・依存先がcallbackと寿命の契約を満たす
必要があります。ハッシュの一致だけで真正性・メモリー安全性・ABI互換性は証明できません。
別のnative/unsafeローダーがプロセス内のDLL初期化を妨げないことも必要です。プロセスのロード設定と
依存解決も信頼し、対象DLLのリダイレクトを行わないことを前提とします。Windowsは指定したパスを
リダイレクトする場合があり、ロード後のパス確認で初期化処理を取り消すことはできません。
[LoadLibraryExW](https://learn.microsoft.com/en-us/windows/win32/api/libloaderapi/nf-libloaderapi-loadlibraryexw)を参照してください。この境界を通った後の
`LayeredEngine::open` と `load` はsafeです。

```rust,no_run
use reknot::jit::{CoffLimits, LayeredArtifact, LayeredEngine};
use reknot::{HostResources, RequestRouter};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let first = LayeredArtifact::read("target/layered/logic-v1.obj.json")?;
// SAFETY: 信頼する生成元で作った既知のローカルビルドであり、コードと依存先が
// 二層のABI・寿命契約を満たす。
let first = unsafe { first.into_trusted() };
let mut engine = LayeredEngine::open(&first, CoffLimits::default())?;
let mut router = RequestRouter::from_loaded(engine.load(first)?, HostResources::new())?;

let next = LayeredArtifact::read("target/layered/logic-v2.obj.json")?;
// SAFETY: このローカルビルドにも同じコード・生成元の信頼を適用できる。
let next = unsafe { next.into_trusted() };
let candidate = router.prepare_loaded(engine.load(next)?)?;
router.activate(candidate)?;
# Ok(())
# }
```

土台の識別情報にはrustcのバージョン・commit、target、Cargo.lockのハッシュ、supportのpackage・
features・profile、コード生成の契約、両DLLのハッシュ・サイズ・ファイル名を含めます。
保存ディレクトリーとlogicの最適化・デバッグレベルは含めません。`open` は両DLLを検証してから
初期化し、ファイルと正規化した親ディレクトリーへの書き込み・削除を拒否する読み取りハンドルを
プロセス終了まで保持します。同じディレクトリー内の別ファイルは作成・編集できます。
静的リンクのホストを使ってください。検証セッションの外で既にロードされたstd/supportは拒否します。
プロセスごとに一つの検証済み土台を使い、同じ土台のopenは再利用します。別の土台や初期化途中の
失敗から再開するには、新しいプロセスが必要です。

`load` は信頼済み成果物を消費し、常駐土台と一致することを確認してから取得済みバイト列をリンクします。
不一致のオブジェクトを実行したりRouterへ有効化したりすることはなく、試行予算は消費します。
Cargo.lockの変更は、logicだけに影響する場合も保守的に土台の変更として扱います。
manifestは256 KiB、各DLLは256 MiB、COFFは既定32 MiBまでです。COFFは `read_with_limit` で上限を
変更できますが、エンジン側の上限も適用します。イメージ上限は両policyに、試行数上限はKeepLoadedに適用します。
`last_load_metrics` は成果物の読み取り・ハッシュ検証を含まないため、リロード時間の計測には
`LayeredArtifact::read` も含めてください。

## 開発時の二層ロジックのビルド

常駐support dylibと、それに依存するlogic crateをCargoワークスペースに用意した後、
[build-layered-jit.ps1](../scripts/build-layered-jit.ps1)をPowerShell 7で実行します。

```powershell
./scripts/build-layered-jit.ps1 -ManifestPath path/to/workspace/Cargo.toml `
    -Package logic -TargetDirectory target/layered `
    -SupportDll target/layered/x86_64-pc-windows-msvc/debug/support.dll `
    -Output target/layered/logic.obj -Offline
```

`-Offline` を使う場合はCargo.lockと依存キャッシュを先に用意してください。ワークスペースのdev profileは
panic=unwind、lto=falseにします。スクリプトは動的std、unwind table、logicのcodegen unit数1を固定し、
終了時には呼び出し元の環境変数を復元します。logicの依存グラフからsupportを特定し、logicをコンパイルした
Cargo呼び出しが報告したsupportを取り込みます。コンパイル中はstdlibのDLLとCargo.lockを固定し、
support DLLはCargo終了後に読み取り共有で開きます。
`RUSTC_WRAPPER` または `RUSTC_WORKSPACE_WRAPPER` を指定した環境は拒否します。
ローカルのCargo設定も信頼できる必要があります。通常のSDK依存crateを自動で二層化する機能ではありません。

Cargoは `--emit=obj` を追加しても通常のリンク成果物を生成します。このスクリプトは
`--crate-type lib` を指定し、Cargoが扱う実際のrlibとCOFFを生成することで、logicのPE DLL/PDBリンクを
省きます。supportは宣言どおりdylibでビルドします。logicの既定値は開発用のopt-level=0、debuginfo=0です。
最適化やデバッグ情報を重視する場合は `-Optimization 1 -DebugInfo 2` を指定してください。
最適化を下げると保持コード量や実行コストが増える場合があります。fixtureだけでアプリケーションの
処理性能が維持されるとは判断できないため、対象ワークロードでも測定してください。

schema 2の付属JSONは、`-CacheDirectory` 内（省略時は出力先に隣接）の `foundation/` と `objects/` に
ハッシュで配置した不変コピーを参照します。キャッシュがmanifestのディレクトリ配下にないときは、付属の `.obj` も
サイズ・ハッシュを照合して使えます。毎回異なるstaging
オブジェクトを生成し、Cargoの古い成果物を誤って証明することを避け、manifestを最後に公開します。
旧コピーは既存ホストが使えるように保持します。

logicだけの編集を続ける場合は、同じコマンドに `-FoundationManifest target/layered/logic.obj.json` を
追加すると、記録した土台を再利用し、metadataの探索とsupportのビルドを省けます。
コンパイラー・lockfile・DLLの内容が一致し、Cargoが同じsupport成果物・profile・featuresを報告する
ことを検証します。依存変更により固定中のsupportの再ビルドが必要になれば、新manifestの公開前に
失敗します。`-FoundationManifest` を外して新しい土台を用意し、新しいホストを起動してください。
safeローダーは旧土台との混在を拒否します。[jit-devセッション](LAYERED_DEVELOPMENT.md)で
watcherを接続でき、追加契約付き世代では明示回収を利用できます。
スクリプトの起動・ハッシュ検証・不変コピーには、Cargoのコンパイル単独計測とは別の時間がかかります。

## Rustランタイム・TLS・panic

`open_with_runtime(bridge, rust_std_dll, limits)` には、コンパイルに使ったものと同じ
コンパイラーのstd DLLを指定します。prefer-dynamic、panic=unwind、emulated TLSで
コンパイルしてください。Rust内部のABIはコンパイラー間で安定していないため、
この一致はunsafeなロード契約の一部です。オブジェクト生成スクリプトはコンパイラー情報、
フラグ、パス、ハッシュを付属JSONに記録しますが、ローダーはその内容を自動検証しません。
既定のアロケーターは常駐stdランタイムが提供します。Rustの割り当て、trait object、
panicペイロードは生成元のランタイム内で扱い、Reknotの境界には既存のバージョン付きC ABIを使います。

依存の探索先はstdと、System32のvcruntime140、ucrtbase、kernel32、ntdll、userenv、
ws2_32、bcryptprimitives、api-ms-win-core-synch-l1-2-0に限定しています。
実行可能な公開シンボルにはイメージ内のジャンプスタブを設け、COFFの相対呼び出しと
SEH personalityのRVAを成立させます。データ用のimport slotはLLVMが生成します。
未解決の依存は公開前に失敗し、別世代やホストへの探索には切り替えません。
依存DLLは後段の処理が失敗しても保持します。同じstd DLLを使う世代はpanic hookなどの
グローバル状態を共有するため、ランタイムの隔離を提供するものではありません。

別経路の `open_with_runtime` では、`thread_local!` はJITイメージ内のLLVM emulated TLS記述子を使います。記述子の同一性と
実際のOSスレッドをキーにし、旧世代と新世代の値を分け、ロード前から存在するスレッドでも動作します。
コンパイラーが挿入するTLSアクセスではWindowsの最終エラー値を保持します。
値のデストラクターは常駐stdランタイムを通じてRustが登録・実行します。
ブリッジは複製したスレッドHANDLEが終了を通知してから格納領域を解放します。
FLSとDLLの破棄順序や、再利用されるスレッドIDには依存しません。
回収はロード時、スレッドの初回JIT TLSアクセス時、または `collect_finished_threads()` で実行し、
同じブリッジを使う全エンジンを対象にします。生存中のスレッドの領域は保持し、メインスレッドの
領域はプロセス終了まで残り得ます。これらはオブジェクトの入力サイズ制限には含まれません。

panicは世代内で `catch_unwind` により捕捉する構成に対応します。登録したアンワインド表と
イメージ内のpersonalityスタブによりRustの後始末を実行し、入れ子のcatch/resumeと
ペイロードのDropをfixtureで検証します。Reknotの各Cコールバックからpanicを漏らしてはいけません。
未捕捉panic、abortモード、外部例外、ホストのC ABIをまたぐ巻き戻しは回復経路として扱いません。
[RustのFFIアンワインド契約](https://doc.rust-lang.org/nomicon/ffi.html#ffi-and-unwinding)も参照してください。
コード保持により遅延したTLSデストラクターやstdのコールバックを支えますが、
無関係なunsafe Rustや任意のネイティブコードまでメモリー安全にするものではありません。

## ビルドと利用

jitを有効にしてもLLVMをダウンロードせず、Cargoのビルドスクリプトも追加しません。
このソースチェックアウトから、Visual Studio C++ Build Tools、CMake、Windows SDKと公式の
[LLVM 23.1.0 SDK](https://github.com/llvm/llvm-project/releases/tag/llvmorg-23.1.0)、
[zlib 1.3.1](https://github.com/madler/zlib/releases/tag/v1.3.1)、
[zstd 1.5.7](https://github.com/meta/zstd/releases/tag/v1.5.7) のソースディレクトリーを使います。
スクリプトはシステムへのインストールを行いません。
インストール済みのVisual Studioに対応するCMakeを使用してください。スクリプトは
特定のVisual Studioの版に固定せず、CMakeの既定のVisual Studioジェネレーター
（既存のビルドディレクトリーではキャッシュされたジェネレーター）を使用します。
CMakeはPATH、またはVisual Studio Installerの `vswhere` から探します。

必要なら `./scripts/setup-jit.ps1` で固定したアーカイブを取得し、SHA-256を検査して
target/jit-depsへ展開できます。そのディレクトリーをビルドスクリプトへ渡してください。
Windows用のJIT専用ワークフローもこの手順を使います。

```powershell
./scripts/build-jit.ps1 -LlvmRoot C:/deps/llvm-23.1.0 `
  -ZlibSource C:/deps/zlib-1.3.1 -ZstdSource C:/deps/zstd-1.5.7
```

出力はtarget/jit-bridge/Release/reknot_jit_bridge.dllです。取り込んだLLVMファイルの
[元のライセンス](../native/jit/vendor/llvm/LICENSE.TXT)と
[補正内容](../native/jit/vendor/llvm/README.md)を保持しています。
crates.ioのSDKアーカイブには、別途ビルドするこのブリッジは含まれません。

```rust,no_run
use reknot::{HostResources, RequestRouter};
use reknot::jit::{JitEngine, JitLimits};

fn start() -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: matching trusted bridge and self-contained objects that obey
    // Reknot's complete handler ABI and owner-thread contracts.
    let mut engine = unsafe {
        JitEngine::open("reknot_jit_bridge.dll", JitLimits::default())?
    };
    let first = unsafe { engine.load("logic-v1.obj")? };
    let mut router = RequestRouter::from_loaded(first, HostResources::new())?;
    let next = unsafe { engine.load("logic-v2.obj")? };
    let candidate = router.prepare_loaded(next)?;
    router.activate(candidate)?;
    Ok(())
}
```

この例ではreknotのjit featureを有効にし、要求を送る前にホスト資源を登録します。
`last_load_metrics()` は成功したロードの読み込み・コピー・追加・実体化・登録時間を返し、
次の試行で消去されます。検証付きRustバックエンドでは[jit-devセッション](LAYERED_DEVELOPMENT.md)で
COFFの生成・選択を自動化できます。

emulated TLSの構成にはnightlyツールチェーンを明示的に導入してください。専用ワークフローでは
nightly-2026-11-02に固定しています。emulated TLSには不安定なコンパイラーオプション
[`-Z tls-model=emulated`](https://doc.rust-lang.org/unstable-book/compiler-flags/tls-model.html)
を使います。ロジックにnative TLSを含むビルドは拒否されます。
上記の二層構成ではこのオプションは不要で、emulated TLSも拒否します。

```powershell
./scripts/build-jit-object.ps1 -Source logic.rs -Output target/logic-v1.obj `
  -Toolchain nightly-2026-11-02
$runtime = (Get-Content target/logic-v1.obj.json -Raw | ConvertFrom-Json).rust_std_dll
```

生成スクリプトは単独のソースcrateとstdを扱い、Cargoの依存解決は行いません。
記録されたランタイムのパスをホストへ渡します。

```rust,no_run
use reknot::jit::{JitEngine, JitLimits};
use reknot::{HostResources, RequestRouter};
use std::path::Path;

unsafe fn start_std(std_dll: &Path) -> Result<RequestRouter, Box<dyn std::error::Error>> {
    let mut engine = JitEngine::open_with_runtime(
        "reknot_jit_bridge.dll", std_dll, JitLimits::default(),
    )?;
    let loaded = engine.load("target/logic-v1.obj")?;
    Ok(RequestRouter::from_loaded(loaded, HostResources::new())?)
}
```

## 検証

コアのテストはタスク・デストラクター・遅延Wakerの所有権、破棄・拒否した候補、ホストへの帰還ガード、
型付きDrain、Restartと移行を扱います。compile-failの例で移動・スレッド制約を確認します。
Miriスクリプトにはloaded-handlerを追加しています。意図的にmem::forgetするリークの検査だけはMiriの外で実行します。

実JITのテストは明示的に選択した場合だけ実行します。非公開の検証用関数を付けてビルドし、次を実行してください。

```powershell
./scripts/build-jit.ps1 -LlvmRoot C:/deps/llvm-23.1.0 `
  -ZlibSource C:/deps/zlib-1.3.1 -ZstdSource C:/deps/zstd-1.5.7 -Testing
$env:REKNOT_JIT_BRIDGE = (Resolve-Path target/jit-bridge/Release/reknot_jit_bridge.dll).Path
$env:REKNOT_JIT_TOOLCHAIN = 'nightly-2026-11-02'
$env:REKNOT_JIT_LAYERED_TOOLCHAIN = 'stable'
cargo test -p reknot --features jit --lib real_coff --locked -- --ignored --nocapture
```

COFF fixtureを新しくコンパイルし、エンジンとルーターを破棄した後の旧版・新版の応答、
登録したJITフレームのスタック走査、不正ABI・TLS・未解決依存の拒否、
破棄・forgetした所有者を含む試行数とバイト数の上限を確認します。
stdのfixtureではVec/String/HashMap、Arc/Mutex/channel、時刻・環境変数、stdスレッド、
TLSのアラインメント・初期化と最終エラー値の保持、破棄中に生成するTLS、旧世代と新世代の分離、並行回収、
入れ子のpanic/resume、スタック変数とペイロードのDropを扱います。
エンジンとルーターの破棄後にも旧版・新版の要求と遅延したTLSデストラクターを実行します。
任意のネイティブコードの安全性、stdの全操作、プロセス全体のメモリー上限の証明ではありません。
共有する保持参照・ガードを変更する際はDLL回収のテストも実行してください。DLLとJITでは回収方針が異なります。

二層SDK fixtureは `target/jit-layered-fixtures/<pid>` の独立したCargoワークスペースで
一つのsupport DLLと旧新版のlogic COFFをビルドし、ビルドログと生成物を検査用に残します。
常駐するJSON/TLSサービス、世代ローカル値の分離と遅延破棄、旧新版の応答、初期化・アクセスのpanic、
payloadのDrop、管理された子タスクのキャンセル、再試行しない破棄失敗を検証します。
native/emulated TLS、supportやimportの不足、不正COFF、不正Handler ABIを拒否し、旧Routerを維持します。
Cargoの依存キャッシュと、拒否検証用のemulated TLS fixtureを作るnightlyが必要です。
受け入れる二層構成の入力はstableでビルドします。

Rustバックエンドだけを検証する場合は、LLVMの準備とブリッジの環境変数を省略できます。

```powershell
$env:REKNOT_JIT_TOOLCHAIN = 'nightly-2026-11-02'
cargo test -p reknot --no-default-features --features jit --lib jit::coff --locked
cargo test -p reknot --no-default-features --features jit --lib real_coff_rust_layered --locked -- --ignored --nocapture
cargo test -p reknot --no-default-features --features jit --lib real_coff_verified_layered_identity --locked -- --ignored --nocapture
```
