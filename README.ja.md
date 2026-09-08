# gloop

[English](README.md) | 日本語

複数のAIツールを、ひとつのターミナルから動かすワークスペースです。作業手順を組み立て、独立した処理を並行して実行し、その結果を次の処理へ渡せます。

- 処理ごとにAIツール・モデル・指示を選べます。
- 自分でワークフローを作ることも、AIに手順を提案させて確認することもできます。
- 実行状況をその場で確認したり、バックグラウンドで動かして後から結果を確認したりできます。
- 出力と実行履歴をプロジェクト内に保存し、内容の確認や実行記録のリプレイに使えます。

## 動作イメージ

**ターミナルでワークフローを実行。** 3つのローカルチェックを並行して動かし、最後の処理で結果をまとめます。

[![gloopのターミナルでワークフローを実行する様子](assets/videos/gloop-terminal.gif)](https://github.com/moto-taka/gloop/releases/download/v0.8.1/gloop-terminal.mp4)

**独立した処理を動かし、結果をひとつに。** 複数のツールへ作業を分け、それぞれの出力を次の処理へつなぎます。

[![3つの独立した処理からひとつの結果へまとめる流れ](assets/videos/gloop-parallel.gif)](https://github.com/moto-taka/gloop/releases/download/v0.8.1/gloop-parallel.mp4)

## インストール

macOS・Linuxでは[Homebrew](https://brew.sh/)でインストールできます。

```bash
brew install moto-taka/tap/gloop
```

Cargoを使う場合は、GitHubからインストールできます。

```bash
cargo install --git https://github.com/moto-taka/gloop --locked gloop-cli
```

## はじめる

作業するプロジェクトでgloopを開きます。

```bash
cd /path/to/project
gloop --lang ja
```

`--lang` を省略するとシステムの言語設定に従います。英語で使う場合は `gloop --lang en` を指定します。保存済みのグラフがある場合は、一覧から **Enter** で開き、**r** で実行できます。

ワークフローを作るには、**+ グラフ · Manual** を選びます。

1. **a** を押して指示を入力し、**Enter** でAIの処理を追加します。
2. **p** でAIツール、**m** でモデルを選びます。
3. 必要な処理を追加します。**A** で分岐を追加し、**c** で既存の処理同士をつなげられます。
4. **r** で実行し、**s** で保存します。

**Enter** で処理を編集、**O** で保存済みのグラフを開き、**Tab** で詳細設定を表示できます。**q** はManualの実行中なら停止、それ以外はホームへ戻ります。グラフの編集だけではAIを呼び出しません。

**Auto** を選ぶとAIが手順を提案し、実行前に確認できます。単独の依頼には **1 AI** を使います。ブラウザ画面で操作する場合は `gloop ui` を実行します。

## バックグラウンドで実行する

タスクを開始し、後から進捗や結果を確認できます。

```bash
gloop start "変更内容をレビューして" --profile codex
gloop tasks
gloop tasks TASK_ID
gloop stop TASK_ID
```

Autoと単独AIのタスクは、ワークスペースを閉じても動き続けます。保存済みのワークフローは **バックグラウンド実行** からも開始できます。処理は手元のマシンで動くため、実行中はマシンを起動したままにしてください。途中で対話的な承認が必要なワークフローにはManualを使います。

キー操作、結果の引き継ぎ、実行上限については[ワークスペースガイド](docs/TASKS.md)を参照してください。

## 対応ツール

**Codex、Claude Code、Qwen、Cursor Agent、Pi、OpenCode** のプロファイルを標準搭載しています。使いたいツールにログインし、次のコマンドで利用可能な状態か確認します。

```bash
gloop provider list --json
gloop provider doctor --json
```

カスタムプロファイルで、ほかのCLIツール、OpenAI互換API（OpenRouterを含む）、Anthropic互換APIにも接続できます。各処理は、選択したプロバイダーの認証情報と利用枠を使います。処理の実行順の制御や結果の引き継ぎにはLLMを使いません。

プロジェクト内のプロファイルを使う場合は `--trust-project-profiles` が必要です。gloop自体は汎用のファイルシステムサンドボックスを追加しないため、信頼できるツールを設定してください。詳しくは[プロバイダー設定](docs/ADVANCED.md#provider-profiles)を参照してください。

## コマンド

| コマンド | 用途 |
| --- | --- |
| `gloop` | ターミナルのワークスペースを開く。 |
| `gloop run --graph FILE` | 保存したワークフローを、このターミナルで実行する。 |
| `gloop start "TASK" --profile TOOL` | バックグラウンドタスクを開始する。 |
| `gloop tasks [ID]` | タスク一覧や結果を確認する。 |
| `gloop stop ID` | バックグラウンドタスクを停止する。 |
| `gloop graph --help` | ワークフローの作成・編集・検証方法を確認する。 |
| `gloop provider --help` | AIツールの設定・利用確認の方法を調べる。 |
| `gloop ui` | ブラウザのワークスペースを開く。 |
| `gloop debug --help` | 実行状況や記録の調べ方を確認する。 |

別のプロジェクトを対象にするには `--repo PATH` を指定します。結果を返すコマンドでは、スクリプト向けに `--json` を使えます。

## ドキュメント

詳細ドキュメントは英語です。

- [ワークスペースとバックグラウンドタスク](docs/TASKS.md)
- [コマンドリファレンス](docs/CLI.md)
- [詳細設定・実行上限・制約](docs/ADVANCED.md)
- [ワークフローのサンプル](examples)
- [グラフのスキーマ](docs/SCHEMA.md)
- [アーキテクチャ](docs/ARCHITECTURE.md)

[開発への参加](CONTRIBUTING.md) · [セキュリティ](SECURITY.md) · [ライセンス](LICENSE) · [出典・謝辞](THIRD_PARTY_NOTICES.md)
