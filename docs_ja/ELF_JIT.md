# Linux ELFオブジェクトのロード

[English](../docs/ELF_JIT.md) | 日本語

`jit::ElfEngine`は、`x86_64-unknown-linux-gnu`上で信頼済みのELF再配置可能
オブジェクトを直接ロードします。ロード時にシステムリンカーやLLVMブリッジを
呼び出しません。ホストの依存で`default-features = false, features = ["jit"]`を
指定します。通常の`.so`ロードには引き続き`native`を使えます。

## 基盤の自動同一性検証とManaged回収

Python 3.9以上、rustup、stable Rust、Cリンカーを用意し、
[build-layered-elf.py](../scripts/build-layered-elf.py)を使います。
supportはSDKを再エクスポートするRustの`dylib`依存とし、ビルダーはlogicを
単一のlibrary objectとして出力します。先にworkspaceの`Cargo.lock`を生成します。

```sh
python3 scripts/build-layered-elf.py \
  -ManifestPath /absolute/project/Cargo.toml -Package my_logic \
  -TargetDirectory /absolute/project/target/layered \
  -SupportLibrary /absolute/project/target/layered/x86_64-unknown-linux-gnu/debug/libmy_support.so \
  -Output /absolute/project/target/elf/logic.o
```

`logic.o.json`（schema 3）と、ハッシュ別に保存したobject・support・stdを生成します。
コンパイラー、target、コード生成フラグ、supportの解決済みfeaturesとprofile、
Cargo.lockを記録し、Cargoが実際に選んだsupportとコンパイル前後の基盤のバイト列を
照合します。同じCargo出力先を使うビルダーはロックで直列化します。
容量超過は`CacheFull`とし、他セッションのファイルを自動削除しません。

```rust,no_run
use reknot::jit::{LayeredArtifact, LayeredEngine, LayeredLimits,
    collect_retired_elf, JitReclaimOptions};
use reknot::{HostResources, RequestRouter};

unsafe fn start() -> Result<RequestRouter, Box<dyn std::error::Error>> {
    let artifact = LayeredArtifact::read("target/elf/logic.o.json")?;
    // ネイティブコードと生成者の意味的な宣言を信頼する。
    let trusted = artifact.into_trusted();
    let mut engine = LayeredEngine::open(&trusted, LayeredLimits::default())?;
    // 外部処理の完全なreturnと、管理外へのコード参照流出がないことも保証する。
    let loaded = engine.load_managed(trusted.into_managed())?;
    let router = RequestRouter::from_loaded(loaded, HostResources::new())?;
    let _report = collect_retired_elf(JitReclaimOptions::default());
    Ok(router)
}
```

`LayeredArtifact`はobjectを読み取ってハッシュを検証し、そのバイト列を保持します。
`LayeredEngine::open`は初期化処理を呼ぶ前にstd/supportのハッシュ、SONAME、依存名を
検証し、検証済みの内容を`memfd`へコピーして書き込み・サイズ変更・seal変更を禁止します。
そのパスからロードしたことを確認し、ファイル記述子とライブラリを常駐させます。
元のファイルが置換されてもロード済みの内容は変わりません。検証済み基盤は
プロセスにつき1つです。異なる同一性、検証経路外で先にロードされたstd/support、
初期化途中の失敗では新しいプロセスが必要です。GNUのシステムライブラリと
ローダー設定・シンボル介入は引き続き信頼範囲です。ハッシュは生成者の真正性や
任意のネイティブコードの安全性を保証しません。

ManagedはCOFFと所有権・呼び出しreturnのガードを共有します。旧リクエスト、
完了済みハンドル、locals、外部アダプター、Wakerの所有権がすべて終了してから、
所有スレッドの明示的な回収でlibgccのunwind登録を解除し、`munmap`します。
cleanup・join・OS解放の失敗は理由付きで保持し、自動再試行しません。
controllerのDropだけでは実行ページを解放しません。通常の`load`はKeepLoadedです。
OS共通の診断・回収には`jit_image_id`、`jit_retention_snapshot`、`JitPolicy`、
`collect_retired_jit`を使えます。既存の`Coff*`型との互換性も維持します。

Managedの上限は同時に保持するイメージ数とバイト数で、KeepLoadedの累積試行回数や
Routerの世代枠を消費しません。既定は所有スレッドにつき256イメージ・256 MiBで、
同じスレッドの他engineも含みます。プロセス全体の上限や自動回収ではありません。

## ソース監視とビルド（`jit-dev`）

Linuxでも[LayeredDevSession](LAYERED_DEVELOPMENT.md)を使えます。
`LayeredDevOptions::new`のbuilderに`scripts/build-layered-elf.py`を指定し、
supportには`lib`・`.so`を除いたCargo target名を渡します。workerは`python3`で
ビルダーを呼びます。`start`はKeepLoaded、`unsafe start_managed`は今後の編集も含めて
Managed契約を受け入れます。ビルド失敗時は現行routerを継続し、新しい編集は古い候補を
置き換え、基盤変更時は`RestartRequired`になります。容量待ちには`Wait`と
`CancelOldest`があり、cleanupに不確実性があれば`RecoveryRequired`になります。
キャンセル後もtaskの破棄と外部処理のjoinを待ちます。基盤の再利用には
`declared_inputs_complete`が必要で、それ以外は毎回Cargoで基盤を再確認します。

## raw APIでのロードと世代切り替え

```rust,no_run
use reknot::jit::{ElfEngine, ElfLimits};
use reknot::{HostResources, RequestRouter};
use std::path::Path;

unsafe fn start(std_so: &Path, support_so: &Path)
    -> Result<RequestRouter, Box<dyn std::error::Error>>
{
    // オブジェクト、依存、初期化処理を信頼し、コンパイラー・target・features・
    // panic方式・supportのビルドを全世代で一致させる。
    let mut engine = ElfEngine::open_layered(std_so, support_so, ElfLimits::default())?;
    let first = engine.load("target/elf/logic-v1.o")?;
    let mut router = RequestRouter::from_loaded(first, HostResources::new())?;
    let next = engine.load("target/elf/logic-v2.o")?;
    let candidate = router.prepare_loaded(next)?;
    router.activate(candidate)?;
    Ok(router)
}
```

実際のリクエストには、先にアプリケーションのリソースを登録します。
試行回数とメモリの計数を維持するため、更新セッションでは同じengineを使います。
既存のリクエストは元のコードで終了し、ホストのリソースは引き継がれます。
同じ`LoadedHandler`を、型付きAPIの`ModuleBuilder::from_loaded`や
`ModuleHost::reload_loaded`にも渡せます。

raw APIのコードはプロセス終了まで常駐します。engine、router、準備済み候補、最後の
Wakerを破棄してもコードやunwind登録は解放しません。ABIが不一致でも、entryを
呼んだ後はそのコードを保持します。unwind登録前の失敗では未公開の領域を解放します。
Managed回収には上記の検証付きAPIを使います。デバッガー登録は未対応です。

## 常駐基盤とオブジェクトのビルド

supportはRustの`dylib`とし、SDKを再エクスポートして常駐させたい依存処理を
非ジェネリックな関数で包みます。logicはsupportに依存する`cdylib`とします。
リポジトリの[support](../reknot/tests/fixtures/jit_layered_support.rs)と
[logic](../reknot/tests/fixtures/jit_layered_logic.rs)が実行テスト用の例です。
再エクスポートだけで、すべてのジェネリックな実体化がsupportへ移るわけではありません。

この構成のworkspaceで、`my_logic`を実際のlogicパッケージ名に置き換えて実行します。

```sh
mkdir -p target/elf
CARGO_INCREMENTAL=0 \
CARGO_PROFILE_DEV_CODEGEN_UNITS=1 CARGO_PROFILE_DEV_LTO=false \
RUSTFLAGS='-C prefer-dynamic=yes -C relocation-model=pic -C force-unwind-tables=yes -C panic=unwind' \
cargo rustc --lib -p my_logic --target x86_64-unknown-linux-gnu \
  --target-dir target/elf-build -- \
  --emit=obj="$PWD/target/elf/logic-v1.o"

rustc --print target-libdir --target x86_64-unknown-linux-gnu
```

最後のコマンドが示す場所の`libstd-*.so`と、生成された
`target/elf-build/x86_64-unknown-linux-gnu/debug/lib<support_crate>.so`を
`open_layered`へ渡します。全世代で同じフラグとtoolchainを使い、logicを更新するときは
出力名を変えます。実行中は基盤ファイルを変更しません。support、std、コンパイラー、
依存構成を変えたら新しいプロセスで開始します。raw APIはコンパイラーの同一性や
ライブラリの真正性を自動検証しません。

両パスをcanonicalizeしてから`RTLD_NOW | RTLD_LOCAL`で開きます。途中で失敗しても、
既に開いたライブラリは常駐します。外部シンボルの探索先は、指定したstd/supportの
ハンドルとそのELF依存、および`libc.so.6`、`libm.so.6`、`libgcc_s.so.1`です。
`RTLD_DEFAULT`によるプロセス全体の探索は行わず、別のJIT世代も探索先にしません。
依存ライブラリの探索、シンボルの介入、preload、ファイルの同時置換は、信頼する
プロセス構成の範囲です。`ElfEngine::open(limits)`は固定のGNUランタイムだけを供給し、
Rustのsupport基盤を必要としない自己完結したオブジェクト向けです。

## 対応範囲と上限

- ELF64 little-endian x86-64の`ET_REL`、通常のシンボル表1つとRELA形式。
  コード、定数、可変データ、BSSを配置し、非ALLOCのデバッグ情報などは破棄します。
  archive、リンク済み実行ファイル・共有ライブラリ、選択処理を要する重複定義、
  commonシンボル、IFUNC定義、拡張番号形式は非対応です。
- 再配置は`R_X86_64_NONE`、`64`、`PC64`、`PC32`、`PLT32`、`32`、`32S`、
  `GOTPCREL`、`GOTPCRELX`、`REX_GOTPCRELX`に対応します。GOTの間接参照を維持し、
  外部PLT呼び出しにはイメージ内のジャンプスタブを使います。
  直接PC相対のデータ参照などで変位が表現範囲を超えたら拒否します。
- logicのnative/emulated TLS、TLS再配置、initializer/finalizer、起動処理、
  実行可能スタック、書き込み可能な実行セクションは拒否します。
  logicには`GenerationLocal`を使います。常駐supportではOS管理のTLSを使えます。
- entryの前にコードをRX、読み取り専用セクションとGOTをR、可変データをRWにします。
  version 1のCIEを含むDWARF32 `.eh_frame`が必須で、libgccへ登録します。
  レコードの範囲とCIEへの後方参照は検査しますが、任意のCFI命令の意味まで証明する
  検査ではありません。unwind情報もネイティブコードとともに信頼する対象です。
  panicはlogic/supportのRust呼び出し内で捕捉し、Cコールバックから返す前に収束させます。
- 既定値は失敗を含む32試行、入力32 MiB、1イメージ64 MiB、raw engineごとの累積公開領域
  256 MiBです。加えて上記の所有スレッド単位の保持上限が適用されます。BSS、アラインメント、ページ余白、GOT、スタブ、unwind終端も数えます。
  依存ライブラリ、パーサーやキャッシュ、libgccの管理情報はこの上限に含めません。
  別engineには別の累積予算がありますが、同じ所有スレッドの保持量は共有して数えます。
  プロセス全体のELFメモリ上限ではありません。
- パーサー上限は8,192セクション、100万シンボル、100万再配置です。
  他アーキテクチャ、musl、Mach-O、独自allocator、Cargo archiveの自動統合は対象外です。
  ロードは`unsafe`な信頼境界であり、サンドボックスではありません。

`last_load_metrics`は読込、検査、実体化、unwind登録の時間を返し、コンパイル時間は
含めません。`retained_bytes`はentry拒否も含めた公開済みのページ量です。
計測値の`linked_bytes`はページ余白を除きます。これだけでコンパイル時間や
編集から応答までの性能が改善したとは判断できません。

再配置の定義は[x86-64 psABI](https://gitlab.com/x86-psABIs/x86-64-ABI/-/blob/master/x86-64-ABI/object-files.tex)、
unwind登録は[GNUのランタイム](https://github.com/gcc-mirror/gcc/blob/master/libgcc/unwind-dw2-fde.c)に基づきます。

## 検証

Linux x86_64 GNU、Python 3.9以上、stable Rust、Cリンカーのある環境で実行します。

```sh
cargo test -p reknot --no-default-features --features jit-dev --lib --locked
cargo test -p reknot --no-default-features --features jit-dev --lib real_elf --locked -- \
  --ignored --nocapture --test-threads=1
```

最初のコマンドで独立したfixture workspaceにも必要な依存キャッシュが揃います。
2つ目ではSDK・support・logicを実際にコンパイルし、世代切り替え、旧リクエスト、
ホスト状態、GenerationLocal、supportのTLS、panicの捕捉、キャンセル、engine破棄、
不正import・TLS・ABI・容量超過の拒否を検査します。別プロセスで、基盤のsealと同一性、
先行ロード拒否、累積上限を超える128回のManaged更新、FDEとマッピングの実際の消失、
外部スレッドのjoin、解放失敗時の保持、実際のソース監視（Wait／CancelOldest、
回復要求、ビルドエラー、基盤変更による再起動要求）も検査します。
`--nocapture`では意図して捕捉した
panicのメッセージも表示されます。[JIT用CI](../.github/workflows/jit.yml)には
Linux ELFとWindows COFFのジョブを設定しています。
