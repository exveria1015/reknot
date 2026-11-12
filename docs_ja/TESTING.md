# テストと Miri

[English](../docs/TESTING.md) | 日本語

## 型付きハンドラー

```sh
cargo test -p reknot --no-default-features --features typed --test typed_handlers --test typed_services --test typed_bridge --test typed_contracts --test typed_restart --test typed_cancellation --locked
cargo test -p reknot --test native_typed --locked -- --ignored --test-threads=1
cargo test -p reknot --test scaffold --locked -- --include-ignored
cargo test -p typed_bot_host --no-default-features --features static-logic --locked -- --include-ignored
cargo run -p typed_bot_host --example framework_bridge --no-default-features --features static-logic --locked
```

静的テストはpoll前の世代固定、複数入口、契約拒否、ホスト状態・失効、サイズ・同時要求上限、
panicの報告、子処理・キャンセル後の終了待ちを検査します。compile-fail doctestは型不一致・借用入力・Send制約を扱います。
実DLLテストは隔離プロジェクトでローカル依存先の編集、旧要求と新版の共存、ビルド失敗からの保存による復旧、
契約変更の拒否、追加編集による古い候補の破棄、新しいローカル依存先の追加と監視、出力による監視の再発火がないことを確認します。
別スレッドから旧世代へ渡した要求を切り替え後にキャンセルし、登録したネイティブ子処理の終了も待ちます。
カスタム配置では隣接・入れ子のモジュール、ローカル依存先、Cargo設定を編集して反映を確認し、
ホストの別cwdからの起動と、生成Rust出力がビルドを再発火させないことも検査します。
ブリッジ試験は実スレッド、FIFO受付、未受領結果を含む容量、待機中・実行中のキャンセル、
複数の終了待ち、所有側の破棄、結果通知Wakerのpanic後の継続を確認します。
生成器は既存ディレクトリ拒否・不正入力を検査し、ignored試験では生成物をビルド・起動して
静的／ネイティブ構成を確認します。実行中の編集保存で新版に切り替わり、ホストの件数が残ることも検査します。
配布物検査でも展開済みアーカイブから同じ生成・実行試験を行います。
生成コンソールは保守周期を1時間に延ばし、即時wakeする要求が完了することを確認します。
生成ホストとBotサンプルは、Pending要求中のquitとEOFを独立に試し、子処理の終了まで待つことと、
処理中の通常入力を明示的に拒否することを検査します。ブリッジ・I/OワーカーはWakerのpanic後も他の結果と終了通知が届くことを確認します。
1MiBのサイズ境界は通常実行で検査し、Miriでは小さい入力で所有・FFIの経路を検査します。
Miriスクリプトにも両借用モデルの `typed-handlers` / `typed-services` / `typed-bridge` を追加しています。
契約試験は Serde 両方向、型名・版を変えないフィールド変更、ソース位置、診断のサイズ制限を確認します。
型付き Restart は移行・最終 commit・失敗と中断からの復旧・子処理の join・キューの通知・再入中の Drop を検査します。
実 DLL でも自動 Restart と明示的な状態移行を行います。隔離した Git スナップショットから CLI をインストールし、
Git 依存のアプリを生成して、スキーマ出力と native/static 起動を検査します。
Miri には `abi-metadata`・`typed-contracts`・`typed-restart` を含め、実際に短い ABI 5 テーブルの読み取りも検査します。
意味の互換性、フレームワーク固有の SDK 接続、DLL 自動回収、OS 通知の配達保証を証明するものではありません。

`native_watch_`の試験では、起動後の`.cargo`作成、設定ディレクトリの置換と不正な設定からの復旧を確認します。
ソースディレクトリの削除・再作成・移動後も、その後の編集で更新が続くことを検査します。
生成物や設定ディレクトリ内の無関係なファイルでビルドが始まらないことも確認し、配布物検査では展開済みアーカイブから同じ試験を行います。

## テストの役割

| 対象 | 確認するもの |
| --- | --- |
| `typed_cancellation` | 待機中のCallの明示キャンセル、Restart完了、最新Waker、繰り返しcancelでの自己通知抑制、子の遅延終了 |
| `cleanup_notifications` | 終了処理中にDropされた要求の次回poll、未完了I/Oの自己通知抑制、通知panic・再入での所有保持 |
| `notification_unwind` | 要求終了とホスト通知の競合、最後のexecutor WakerのDrop単独／wakeとの同時panic。通常は子プロセス、Miriでは両モデルで直接実行 |
| `source_watcher` | 低レイヤー監視のソース／プロジェクト再作成、起動時のsrc欠落、manifest・build scriptの置換、出力・ログ・gitの除外 |
| `activity_notifications` | 子内で登録した子・サービスの通知だけによる完了、1回のpollの範囲、待機中の自己wake抑制、追加poll通知のpanic後の所有保持 |
| `async_service::tests` | キャンセルの複数待機、Waker更新・解放、Drop・登録枠の再利用、スレッド間の登録・cancel・Drop競合、通知のpanic・再入 |
| `host_resources` | 所有、失効、資源 ID・版、借用競合、出力容量、panic 境界 |
| `cache_lifecycle` | 別プロセスのホスト／CLI、画像の状態、Router Drop 後の lease、正常／強制終了、非継承、namespace 競合、trash 再試行、不明／旧形式／リンクの除外 |
| `request_routing` | 入力コピー、Drain、候補拒否、受付停止・再開、Router より長い実行寿命 |
| `restart` | 協調停止、旧 Drain 世代の終了待ち、移行の段階適用、失敗・中断・再試行、上限 |
| `host_commit` | 確定フックの順序・一回性、それ以前の失敗・中断・再試行・panic |
| DB host `database` | 実 SQLite の schema/data rollback、BEGIN／COMMIT 競合、キャッシュ排他、復旧時の遮断、overflow |
| DB host `native_migration`（ignored） | 実 DLL 3種類の互換 Drain・非互換 Restart、rollback／再試行、旧資源の拒否、永続版の再 open |
| `managed_activities` | 明示登録、子の遅延終了、解除失敗、Drop 後の終了待ち、上限、panic と再試行 |
| `async_services` | 入出力コピー・サイズと枠の上限、キャンセル終了待ち、失効、操作 ID、バッファ不足での結果保持 |
| `threaded_services` | FIFO・容量・通知・shutdown／join、2本のロジック所有スレッド、実 TCP と読み取り中キャンセル |
| `threaded_io_host` の `native_threaded_io`（ignored） | 実 SDK DLL 2世代、2本のロジックスレッド、ホスト所有の TCP I/O、Drain と所有スレッドでの破棄 |
| `service_contracts` | 複数要求・明示割り当て、初期化スキーマ・形式・panic拒否、失効の再検査、世代間の設定保持 |
| `contracts_host` の `native_contracts`（ignored） | 実 SDK DLL 2世代のサービス不足・版違い・初期化形式拒否、同期／非同期の複数サービス利用 |
| `native_routing` | 実 DLL と同じ TCP 接続、チェックポイント形式変更、別スレッドのコールバック帰還・join・解除失敗、旧 ABI 拒否 |
| `native_reclamation` | 退役・所有参照・実行帰還の検査、外部 Waker の帰還待ち、所有スレッド、拒否候補、40回更新、OS 常駐状態と Windows のコピー削除失敗 |
| `generation_locals` | 世代・Router間の値の分離、借用とpanic、旧テーブル・未知の制御拡張の拒否 |
| `native_managed_generation` | 実SDKのDLL、登録値の破棄、キャンセル終了待ち、ホスト通知を保持したままの回収、破棄panic時の保持 |
| Bevy `native_game` の ignored テスト | 実 DLL と Bevy ECS、フレーム境界、遅延停止・復元、旧世代・Entity・入力版の結果検査、所有スレッド |
| Tauri `engine` の ignored テスト | 独立プロジェクトの実 DLL ビルド、Drain／Restart、値の継続、構文エラー復帰、作業ディレクトリから独立したコピー |

`fixtures/handler.rs` は同梱の固定 ABI 定義 `abi_v5.rs` を使い、SDK や proc macro に依存しない独立した実装です。
マクロ生成側とローダー側が同じ誤りを共有しても検出できるように残しています。

## 配布物と文書の検査

```powershell
python scripts/check-docs.py
./scripts/check-packages.ps1
```

依存を取得済みなら、配布物の検査に `-Offline` を指定できます。
スクリプトは SDK の3 crate をパッケージ化し、展開したアーカイブだけで別ワークスペースを作ります。
全ターゲットのネイティブ・静的構成、独立した ABI DLL、明示回収の実 DLL 試験、静的リンクの利用側プログラムを検査します。
未公開の内部依存は patch で展開済み crate に解決し、元のソースツリーは参照しません。
各アーカイブに README とライセンス本文が必要です。結果とアーカイブのハッシュは
`target/package-checks/` に残します。レジストリへの公開や crate 名の予約は行いません。

文書の検査は相対リンクのファイル・見出しを確認し、公開文書からローカル作業資料へのリンクを拒否します。
どちらの検査も Windows・Linux CI に設定しています。

## 通常の確認

リポジトリのルートで実行します。実 DLL テストが `rustc` を起動するため、Cargo と rustc を PATH に入れてください。

```powershell
cargo fmt --all -- --check
rustfmt --edition 2021 --check reknot/tests/fixtures/handler.rs reknot/tests/fixtures/abi_v5.rs reknot/tests/fixtures/reclamation.rs reknot/tests/fixtures/managed_logic.rs
cargo clippy -p reknot_abi -p reknot_macros -p reknot --all-targets --locked -- -D warnings
cargo clippy -p reknot_abi -p reknot_macros -p reknot --all-targets --all-features --locked -- -D warnings
cargo test -p reknot_abi -p reknot_macros -p reknot --locked
cargo test --workspace --exclude tauri_tokio --all-features --locked
cargo test -p reknot_abi -p reknot_macros -p reknot --release --all-features --locked
```

GUI のバックエンドとフロントエンド構文は別に確認します。テストは通常の GUI ソースを編集せず、ウィンドウも開きません。

```powershell
cargo clippy -p tauri_tokio -p tauri_tokio_logic --all-targets --locked -- -D warnings
cargo test -p tauri_tokio --bin tauri_tokio --locked -- --ignored --test-threads=1
node --check examples/tauri_tokio/dist/main.js
```

ignored 統合テストは一時的にテストプロセスの作業ディレクトリを変えるため、単一テストスレッドで実行します。
実際の GUI の見た目・操作は [GUI Lab](TAURI_GUI_LAB.md) の手順で確認してください。

Linux GUI ジョブでは `tauri-driver 2.1.0` と Xvfb で実際の WebView も起動します。
標準ライブラリだけの Python 試験で、編集例の選択・保存、Drain／Restart、要求送信、
ビルドエラーと修復、同じホストとカウンターの継続を検査します。
終了時にサンプルのソースを復元し、画面と診断ログを artifact に保存します。
画面の目視評価と Windows での実操作は、この自動試験と分けて確認してください。

## Bevy ゲーム統合

Bevy のネイティブ統合は描画なしで別途確認します。詳しくは [ゲームへの最小適用](BEVY_GAME.md)。

```powershell
cargo clippy -p bevy_game_host -p bevy_game_logic -p bevy_game_protocol --all-targets --all-features --locked -- -D warnings
cargo test -p bevy_game_host --test native_game --locked -- --ignored --test-threads=1
```

## ホスト資源・SQLite の移行

```powershell
cargo clippy -p db_migration_host -p db_migration_logic --all-targets --all-features --locked -- -D warnings
cargo test -p db_migration_host --test database --locked
cargo test -p db_migration_host --test native_migration --locked -- --ignored --test-threads=1
```

この例だけが rusqlite と bundled SQLite に依存し、C コンパイラが必要です。
DB は `target/` 内に新規作成し、既存アプリの DB は使用しません。
[移行方針と限界](HOST_RESOURCE_MIGRATION.md) を参照してください。

## 保持量とコピーの掃除

```powershell
cargo test -p reknot --test cache_lifecycle --locked -- --test-threads=1
```

`target/tmp/cache-lifecycle-tests/` 内の独立したルートで、ホスト子プロセスと実 CLI を動かします。
利用者の既定コピー領域を apply するテストではありません。
ignored の2関数は親テストからだけ呼ぶ子プロセスの入口です。
共通の終了・排他・掃除に加え、Windows では削除拒否・junction、Unix では symlink・共有書き込み可能なルートの拒否を検査します。
これらの OS・ファイル検査は Miri の対象外です。[利用契約](RETENTION_AND_CLEANUP.md) を参照してください。

## Miri

`typed_cancellation`・`cleanup_notifications`・`notification_unwind`は両借用モデルで実行します。
配布物検査でもこれらのスイートと実OSの`source_watcher`を展開済みSDKから実行します。

`cancellation_notifications` は `FuturesUnordered` 内での停止・Restart、複数・複製waiter、
登録のDrop・更新、ルートの終了・Drop、子の失敗を、通知に従ってpollする形で検証します。
`native_cancellation` は実DLL境界で同様の処理を行い、終了後に両世代のDLLを解放できることも確認します。
`generation_locals` は未提供・未知バージョンの停止通知拡張について、関数ポインター領域を読まずに拒否する検査を含みます。
配布パッケージ検査でも3スイートを実行し、静的な検査はMiriの両借用モデルでも実行します。

固定ツールチェーンと Linux インタープリターターゲットを使います。通常ビルドの既定ツールチェーンは変更しません。

```powershell
rustup toolchain install nightly-2026-11-02 --profile minimal --component miri --component rust-src
cargo +nightly-2026-11-02 miri setup --target x86_64-unknown-linux-gnu
./scripts/check-miri.ps1 -Toolchain nightly-2026-11-02
```

スクリプトは Stacked／Tree Borrows の両方を seed 0 で実行します。
`-Seed 1` で別 seed、`-BorrowModel Stacked` または `Tree` で単独実行できます。
対象は task、登録型TLS、ホスト通知の競合、resources、routing、restart、host commit、managed activities、async services、threaded services、service contracts です。
Miri では threaded services の実 TCP テストを除外し、同期・通知・所有の検査だけを行います。
SQLite と C コードはネイティブで確認し、Miri はコアの確定フックの管理だけを対象にします。

ログ、コンパイラ情報、対象ソースと Cargo.lock の SHA-256、終了コードを `target/miri-audit/seed-<seed>/` に保存します。
失敗しても残りのグループを実行し、失敗があれば非ゼロで終了します。
DLL・ネットワークを使うテストを Miri が代替するわけではありません。

## 検証範囲

CI には Windows／Linux のコア・ネイティブ統合、コアの最小 Rust バージョン、
Miri 両借用モデル、Windows／Linux GUI バックエンドを設定しています。
結果は対象ソースの版と対応させて確認してください。[ワークフロー](../.github/workflows/ci.yml) も参照できます。
Windows ローカルの成功を Linux や実 GUI の確認結果として扱いません。

## 静的構成と native 単独構成

```sh
cargo clippy -p reknot --all-targets --no-default-features --locked -- -D warnings
cargo test -p reknot --no-default-features --locked
cargo check -p reknot --no-default-features --features native --locked
cargo test -p service_host --release --no-default-features --features static-logic --locked
cargo tree -p service_host --no-default-features --features static-logic --edges normal --locked
```

静的構成の依存ツリーに libloading・notify・旧ランタイムアダプターが含まれないことを確認します。
既定の対象はコア3 crate です。サンプルは `-p` または `--workspace` で明示的に選びます。
