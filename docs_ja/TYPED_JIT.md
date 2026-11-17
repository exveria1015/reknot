# COFF・ELFによる型付き開発

[English](../docs/TYPED_JIT.md) | 日本語

同じ`DevModule`、型付き関数、`HandlerHandle`、ブリッジのクライアントを、通常のDLLと
実験的な二層オブジェクト方式で使えます。開発開始時に方式を選び、実行中のセッションは
その選択とロード予算を維持します。

## バックエンドを選ぶ

| 構成 | ホストのfeature | `DevModule::start`へ渡す設定 |
| --- | --- | --- |
| 通常DLL | 既定feature、または`typed,dev-tools` | `DevOptions` |
| Windows x64 MSVC COFF／Linux x86_64 GNU ELF | `typed,jit-dev`。`native`は任意 | `LayeredDevOptions` |

二層方式にはlogic/support構成と、コンパイラーの一致する常駐std/supportが必要です。
SDK内蔵のRust製ビルダーを使う場合、rustup/CargoとOSのリンカーで動き、PowerShell/Pythonは不要です。
[二層開発](LAYERED_DEVELOPMENT.md)と[COFF](JIT_CODE_OWNERSHIP.md)または[ELF](ELF_JIT.md)の構成手順を参照してください。
`reknot-new`は通常DLL向けの構成を生成します。方式の選択だけでsupport crateは生成されません。

supportで`typed`を有効にしたSDKを再エクスポートします。logicからこの依存を`reknot`という
名前で参照すれば、ハンドラーの属性とエクスポートの記述を維持できます。
以下のパスはアプリの`project`とSDKのチェックアウト`reknot`が隣り合う配置の例です。

```toml
# support/Cargo.toml。ホストと同じSDKリビジョンを使う。
[lib]
crate-type = ["dylib"]
[dependencies]
reknot = { path = "../../reknot/reknot", default-features = false, features = ["typed"] }
```

```rust
// support/src/lib.rs
pub use reknot::*;
```

```toml
# logic/Cargo.toml。my_supportは実際のsupportパッケージ名に置き換える。
[lib]
crate-type = ["cdylib"]
[dependencies]
reknot = { package = "my_support", path = "../support" }
```

ホストはSDKに依存させ、support dylibにはリンクしません。ホストとlogicで同じハンドラーの
契約を共有してください。組み込み型の入出力はそのまま使えます。独自のプロトコル型にも、
引き続きスキーマと明示的な契約バージョンが必要です。

## 起動して更新する

通常の`DevOptions`の代わりに二層の設定を渡します。実行環境に応じてCOFFまたはELFを選び、
型付きホストは候補を受け付ける前に契約を検査します。

```rust,no_run
use reknot::jit::development::LayeredDevOptions;
use reknot::typed::{DevModule, Error, Handler, Module};

const ECHO: Handler<String, String> = Handler::new("app.echo", 1);

unsafe fn start() -> Result<DevModule, Error> {
    let options = LayeredDevOptions::builtin(
        "project/Cargo.toml", "my_logic", "my_support",
        "project/target/layered",
    );
    let module = Module::new("app", 1).with(ECHO);
    // プロジェクト、生成者、依存、今後の編集を信頼する。
    let mut dev = DevModule::start(module.builder(), options)?;
    let _echo = dev.host().handler(ECHO)?;
    // 既存の呼び出しとブリッジは同じ型付きAPIを使う。
    if let Some(event) = dev.update() {
        eprintln!("{event}");
    }
    Ok(dev)
}
```

どちらの方式も、最初の世代の検証と適用が終わってから`start`が戻ります。
以後のビルドはworkerで実行します。要求の実行中も`update`を定期的に呼んでください。
接続したブリッジ、後始末、登録ワーカーも進めます。独立したexecutorでは従来どおり
`host().bridge()`を使い、driverを直接pollできます。

`shutdown()`は公開を止めてビルドworkerをjoinするため、Cargoの終了を待つ場合があります。
要求をキャンセルしたり、ハンドルが所有するコードを解放したりはしません。
`Drop`では二層の公開を止めますが、ビルダーの終了を待ちません。Restartを中断した場合は
受付を停止したままにし、後始末を進めて通常のホスト復旧APIを使います。

## Drain・Restart・Managed

既定はDrainとKeepLoadedです。協調的なRestartを選ぶには、設定を
`DevBackend::from(options).with_reload_mode(ReloadMode::Restart)`で変換してから
`start`へ渡します。登録ワーカーの型付きチェックポイントを維持し、通常の要求は再実行しません。
ハンドラーの契約変更にはホストの再ビルド・再起動が必要です。自動Restartでは状態スキーマの変更を移行しません。

`unsafe DevModule::start_managed(builder, options)`はManaged COFF/ELFを選びます。
今後の編集と外部callback/Wakerの最終帰還を含む[管理寿命の契約](LAYERED_DEVELOPMENT.md)を
すべて満たす必要があります。通常DLLの設定は拒否します。`update`が回収するのは、この
セッションの退役イメージだけです。要求・ハンドル・ワーカーの保持によって回収が遅れる場合があり、
controllerを破棄しても強制的にアンロードしません。

型付き`ModuleBuilder`の資源、サービスの割り当て、同時要求数、Router予算を維持します。
KeepLoadedのRouter上限はbuilderと`LayeredDevOptions::max_router_generations`の厳しい方が適用され、
engineの累積ロード上限も適用されます。Managedでは既存の実保持イメージ数・ページ量の制限を使います。

## 失敗と復旧を確認する

| イベント・診断 | 動作 |
| --- | --- |
| `Building`、`Reloaded`、`Restarting`、`Superseded` | 通常DLLと共通の型付きイベント |
| `BuildFailed` | 現用ハンドラーを継続。ソースを直して保存する |
| `Rejected { error: Error::Contract { .. } }` | 構造化された契約差分を維持。現用ホストは変わらない |
| `ReloadEvent::Layered` | `LayeredDevState`による基盤・容量・保持・復旧条件 |
| `layered_snapshot()` | ビルド時間、基盤manifest、現用世代の有無、ロード回数、保持イメージ。通常DLLでは`None` |
| `retry_layered()` | engineや予算をリセットせず再探索。通常DLLまたは停止済みならfalse |

`ReloadEvent`は非網羅的なenumなので、matchには未知のvariantを受ける分岐を置いてください。
`LayeredDevState`にも`Restarting`が追加されるため、網羅的なmatchは更新が必要です。
基盤変更時は`RestartRequired`を通知して旧ホストを維持し、プロセスは自動再起動しません。
`WaitingForRetirement`と`RecoveryRequired`も既存の二層開発と同じ意味です。
ビルド失敗や候補拒否では受付を維持します。協調的なRestartが始まった後は、確定または
アプリケーションによる復旧まで意図的に受付を停止します。

既定はJSONです。[任意のバイナリ形式](BINARY_MESSAGES.md)も同じ所有権とメッセージ上限を維持します。ロード方式の選択だけでは、
個別アプリケーションの高速化は確認できません。リンク・ロード時間と保存から応答までの
時間は含む処理が異なるため、別々に測定します。

## 統合を検証する

対応するどちらの環境でも、リポジトリのチェックアウトから実行します。

```sh
cargo test -p reknot --no-default-features --features typed,jit-dev --test layered_typed --locked -- --ignored --nocapture
```

実オブジェクトと独立プロセスでKeepLoadedとManaged、旧要求の保持、ハンドルとブリッジの維持、
ビルド失敗と復旧、初回・更新時の契約拒否、基盤変更、旧世代の解放待ち、ワーカーのチェックポイント、
Restart時の受付と中断後の明示的な復旧、生成物の再利用・上書き拒否・容量制限を検査します。Rust製ビルダーを使い、キャッシュ済みのCargo依存が必要です。

[型付きハンドラー](TYPED_HANDLERS.md) · [二層開発](LAYERED_DEVELOPMENT.md)
