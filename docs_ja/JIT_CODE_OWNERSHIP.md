# コードの所有権と実験的なCOFF/JITLink

[English](../docs/JIT_CODE_OWNERSHIP.md) | 日本語

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

リンクに成功したJITの機械語・データ・依存先・登録したアンワインド表はプロセス終了まで保持します。
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

外部コールバックは最後のWakerを解放した後にも、旧コード上の命令を実行し得ます。
参照数だけでは外部実行の終了を証明できません。safeな自動アンロードAPIは追加していません。
既存のunsafeなDLL回収にも、[外部からの帰還とTLSの終了条件](UNLOAD_SAFETY.md)が引き続き必要です。

## JITの対応範囲

任意選択の `jit` featureは、別途ビルドしたLLVM 23.1.0のブリッジをWindows x86-64で使います。
ORCのObjectLinkingLayer / JITLinkを明示的に選び、ハンドラーの入口を公開する前に
COFFの.pdataをRtlAddFunctionTableへ登録します。各イメージには別のJITDylibを割り当て、
既存イメージをその場で書き換えません。

入力は信頼済みで自己完結したMSVC x86-64 COFFオブジェクトであり、Handler ABI 5の
hot_handler_apiを公開する必要があります。検証fixtureはno_std、panic=abort、
明示的なアンワインド表、世代ごとに同時タスク1個という構成です。ホストSDK自体の通常のタスクモデルは維持します。

ブリッジはリンク済みPE、別形式・別アーキテクチャ、TLS、CRTの初期化・終了処理の節を拒否します。
プロセス内の任意のシンボルは探索せず、未解決のランタイム依存は失敗します。
LLVMへの局所的な補正はfixtureが出力する強いローカルCOMDATリーダーを扱い、それ以外の形式は拒否します。

一般的なstd、アロケーター、外部crate、ランタイム初期化、TLSデストラクター、
panic・外部例外の伝播にはまだ対応していません。現在のABI表と旧プレフィックスを扱えることは、
通常の生成済み型付きモジュールのJIT対応を示すものではありません。
CodeViewの節は受け入れますが、デバッガー登録はありません。JITフレームを通るスタック走査を確認しており、
panicによる巻き戻しや、その間のRustのDropは別の未検証項目です。

## ビルドと利用

jitを有効にしてもLLVMをダウンロードせず、Cargoのビルドスクリプトも追加しません。
このソースチェックアウトから、VS 2022 Build Tools、CMake、Windows SDKと公式の
[LLVM 23.1.0 SDK](https://github.com/llvm/llvm-project/releases/tag/llvmorg-23.1.0)、
[zlib 1.3.1](https://github.com/madler/zlib/releases/tag/v1.3.1)、
[zstd 1.5.7](https://github.com/meta/zstd/releases/tag/v1.5.7) のソースディレクトリーを使います。
スクリプトはシステムへのインストールを行いません。

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
次の試行で消去されます。watcherやCargoの経路でCOFFを自動生成・選択する機能はまだありません。

## 検証

コアのテストはタスク・デストラクター・遅延Wakerの所有権、破棄・拒否した候補、ホストへの帰還ガード、
型付きDrain、Restartと移行を扱います。compile-failの例で移動・スレッド制約を確認します。
Miriスクリプトにはloaded-handlerを追加しています。意図的にmem::forgetするリークの検査だけはMiriの外で実行します。

実JITのテストは明示的に選択した場合だけ実行します。非公開の検証用関数を付けてビルドし、次を実行してください。

```powershell
./scripts/build-jit.ps1 -LlvmRoot C:/deps/llvm-23.1.0 `
  -ZlibSource C:/deps/zlib-1.3.1 -ZstdSource C:/deps/zstd-1.5.7 -Testing
$env:REKNOT_JIT_BRIDGE = (Resolve-Path target/jit-bridge/Release/reknot_jit_bridge.dll).Path
cargo test -p reknot --features jit --lib jit::tests::real_coff -- --ignored --nocapture
```

COFF fixtureを新しくコンパイルし、エンジンとルーターを破棄した後の旧版・新版の応答、
登録したJITフレームのスタック走査、不正ABI・TLS・未解決依存の拒否、
破棄・forgetした所有者を含む試行数とバイト数の上限を確認します。
任意のネイティブコードの安全性、Rustランタイム全体への対応、プロセス全体のメモリー上限の証明ではありません。
共有する保持参照・ガードを変更する際はDLL回収のテストも実行してください。DLLとJITでは回収方針が異なります。
