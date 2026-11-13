# リリースの準備

[English](../docs/RELEASING.md) | 日本語

Reknot の現在の候補は **ソース RC** の `v0.1.0-rc.2` です。
`reknot`、`reknot_abi`、`reknot_macros` の版を `0.1.0-rc.2` にそろえ、
内部のレジストリ依存も同じ版に固定します。サンプルや生成したアプリの版は
SDK の版とは独立しています。ソース RC では crates.io への公開は行いません。

## 対象のソースを確定する

統合先と既定ブランチを `main` にそろえます。この候補には
`feature/websocket-quiz-room` の実装を含めます。対象は型付き契約と Restart、
WebSocket クイズ、通知駆動のホスト、キャンセルとソース監視の修正、
Reknot 名だけを使うキャッシュ設定、プロジェクト生成、および文書・検証スクリプトです。
作業ツリーをクリーンにし、`git rev-parse HEAD` の結果をリリースノートに記録します。
公開済みタグを別のコミットへ付け替えてはいけません。

次の候補では SDK 3 crate の版、内部依存の完全一致指定、`Cargo.lock`、変更履歴、
タグを一緒に更新します。版を変えた後に `cargo metadata --format-version 1 --offline`
を実行し、lockfile の差分を確認します。無関係な依存更新は公開準備に含めません。

## 公開候補を検証する

最終ソースで [CI](../.github/workflows/ci.yml) の9ジョブをすべて成功させます。
対象は Windows/Linux のコア・配布物、Windows/Linux のコア MSRV、Bevy MSRV、
両 Miri 借用モデル、Windows/Linux の GUI です。コアのジョブは型付き実 DLL 更新、
生成プロジェクト、ブリッジ、展開した配布物の利用側も確認します。
Linux GUI は実 WebView の操作を含み、Windows GUI はウィンドウを開かない
バックエンド検証です。

クリーンなチェックアウトからパッケージ公開の予行を実行します。

```sh
cargo publish --dry-run --locked -p reknot_abi -p reknot_macros -p reknot
```

複数パッケージの公開に対応する Cargo を使います。開発環境で検証した版は
Rust/Cargo 1.99.1 です。ビルドの MSRV は引き続き Rust 1.95 です。
dry-run や展開済み配布物への patch を使った検証に成功しても、名前の予約、
所有権、実際の crates.io からの取得を確認したことにはなりません。

新しいチェックアウトへ公開候補を取得でき、文書のコマンドをそこで解決できることを
確認します。既定ブランチやリポジトリ名を変えた後は外部リンクを確認します。
ローカル検証ログは Git にコミットするソースに含めません。

## ソース RC を準備し公開する

GitHub に **ドラフトのプレリリース** を作成します。タイトルは
`Reknot v0.1.0-rc.2`、タグは `v0.1.0-rc.2`、対象は検証済みコミットの完全な SHA
とし、以下を記載します。

- ソースの SHA と、その版で成功した CI 実行の URL。
- 型付き契約と Restart、WebSocket クイズ、通知・キャンセル・ソース監視の修正、
  プロジェクト生成、キャッシュの破壊的変更。
- Windows/Linux の検証範囲、コア Rust 1.95・Bevy Rust 1.97.1 の要件。
- 下記の制限、変更履歴と互換性の文書へのリンク。
- ソース取得手順と問題の報告先。

候補を公開する段階でプレリリースを発行します。ドラフトを作るだけではタグは
公開されません。発行後にタグを取得し、検証済みコミットを指すことを確認します。
その後の修正は新しい版にします。以下は候補の公開後に使うコマンドです。

```sh
git clone --branch v0.1.0-rc.2 --depth 1 https://github.com/exveria1015/reknot.git
cd reknot
cargo run -p typed_bot_host --locked
```

インストールした `reknot-new` は、チェックアウトを保持せず Git の固定コミットを使えます。
`cargo install --git ... --rev SHA` と `reknot-new PATH --sdk-rev SHA` に同じ検証済みの完全な SHA を指定します。
既定のレジストリ経路には別途 crates.io 公開が必要です。[型付きハンドラー入門](TYPED_HANDLERS.md) を参照してください。

## 制限を明記する

Reknot はローカル開発中の信頼済み Rust コードを対象とします。同じ SDK と互換な
ターゲット・ツールチェーンでホストとロジックを一緒にビルドしてください。
型付き API は単一 DLL、Drain、明示登録したワーカーの Restart に対応します。型付き契約は形式 v2 となり、`JsonSchema` が必要です。
rc.1 から更新するときはプロトコルの型に `#[reknot::schema]` を付け、ホストとロジックを再ビルドします。
ハンドラー入出力の契約変更ではホストも再起動し、ワーカーの状態変更には明示的な移行を使います。

Handler ABI 5 とタスク制御 ABI 2 は、宣言サイズ内の prefix 読み取りと任意の拡張ヘッダーを
維持します。異なる SDK リビジョン間の相互運用を保証するものではなく、ホストとロジックには
同じ SDK を使います。要求のキャンセル待機には停止通知 ABI 1 の拡張が必要で、
存在しない場合は明示的に失敗します。

キャッシュルートの環境変数は `REKNOT_CACHE_ROOT`、対応するセッションマーカーは
`reknot-cache-v1` のみです。以前の別名は無視し、未対応の既存キャッシュを移行・削除しません。
詳細は [保持量の観測と掃除](RETENTION_AND_CLEANUP.md) を参照してください。

既定では DLL をロードしたまま保持し、ロード試行の累積上限は失敗を含めて32回です。
unsafe な明示回収にはアプリ側の静止性の証明が必要で、回収しても予算は戻りません。
自動アンロード、外部副作用の巻き戻し、本番配信、macOS の検証は現在の対象外です。
Windows の実 GUI 操作は自動バックエンド試験の検証範囲に含まれません。
[互換性](COMPATIBILITY.md) と [テスト](TESTING.md) を参照してください。

## crate の公開は別に行う

将来の crates.io 公開は別の配布手順です。公開直前に名前の利用可否とアカウントの
所有権を再確認します。SDK の版と内部の完全一致依存をそろえ、最終 CI と dry-run を
やり直した上で、`reknot_abi` と `reknot_macros`、最後に `reknot` を公開します。

公開後は、この workspace の外に新しい利用側を作り、レジストリ依存だけで
ビルドします。`path`、Git 依存、`[patch]`、ローカル Cargo 設定の継承は使いません。
既定の typed/native 構成と `default-features = false` の両方を確認します。
これによって実レジストリからの導入経路を確認できます。

## 問題を報告する

再現できる問題は [Reknot issues](https://github.com/exveria1015/reknot/issues) に
報告してください。版またはコミット、OS・ターゲット、`rustc --version`、feature、
再現手順、関連するエラー出力を添えます。投稿前に認証情報や非公開のアプリデータを
取り除いてください。
