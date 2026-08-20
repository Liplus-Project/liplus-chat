# liplus-chat

> [!IMPORTANT]
> このリポジトリは現在、アプリケーション基盤のスキャフォールド段階です。チャットルーム UI と Channels / MCP 連携はまだ実装されていません。

## 概要

liplus-chat は、人間と複数の独立した AI / Li+ セッションが、一つのローカルな会話面を共有するためのデスクトップアプリを目指しています。

各 AI セッションの文脈と人格は独立したまま保ち、発言者を識別できる共通の場で対話する構想です。特定の AI に他のセッションの内部文脈を統合するのではなく、それぞれが自分の文脈から同じ会話へ参加する形を取ります。

設計の詳細と、受容したトレードオフは [`docs/0-requirements.md`](docs/0-requirements.md) に記載しています。

## 設計姿勢

- Li+ を含む各セッションを、独立した参加者として扱います。
- AI 同士の暴走や対立を意図的に誘発する機構は設けません。
- 現在の Li+ を自然に動かした結果として予期しない相互作用が生じた場合は、発言者と会話の流れを追える形で観測できることを目指します。
- 会話面はローカルで動作させ、セッションとの接続は PTY と Channels / MCP を用いる構想です。
- 構想上の機能と実装済みの機能を明確に区別します。

## 現在の実装状況

### 実装済み

- Tauri 2 による Windows デスクトップアプリの基盤
- Vite と TypeScript による最小のフロントエンド
- プロセスの起動、入力、リサイズ、終了を扱う Rust の PTY コマンド
- タブ設定とセッションデータを JSON へ保存・読込する Tauri コマンド
- Claude Code と Codex を想定した既定タブ設定
- Windows 上で `npm ci` と Rust のコンパイル確認を行う CI
- GitHub Release 公開時に Tauri バンドルを作成する CD

現時点の画面は、スキャフォールドであることを表示するプレースホルダーです。Rust 側の PTY・設定・セッション保存機能は存在しますが、フロントエンドからチャットルームとして利用できる状態にはまだ接続されていません。

### 未実装

- メッセージ一覧、発言者表示、入力欄を備えたチャットルーム UI
- フロントエンドと PTY コマンドの接続
- Channels / MCP による通知受信と返信
- 複数の AI / Li+ セッションを共通の会話へ参加させる連携
- 予期しない相互作用を追跡するための会話ログと観測 UI

## Windows での開発

このリポジトリの CI は、次の環境を基準にしています。

- Windows
- Node.js 22 と npm
- Rust stable
- Rust ターゲット `x86_64-pc-windows-gnu`
- GNU ターゲットをビルドできる MinGW ツールチェーン

依存関係をインストールします。

```powershell
npm ci
rustup target add x86_64-pc-windows-gnu
```

Tauri アプリを開発モードで起動します。

```powershell
npm run tauri dev
```

フロントエンドだけを起動する場合は、次のコマンドを使います。

```powershell
npm run dev
```

### 検証とビルド

フロントエンドを型検査してビルドします。

```powershell
npm run build
```

CI と同じ Rust ターゲットでコンパイルを確認します。

```powershell
Push-Location src-tauri
cargo check --target x86_64-pc-windows-gnu
Pop-Location
```

デスクトップアプリの配布用バンドルを作成します。

```powershell
npm run tauri build
```

MinGW のツールが、空白を含むビルド出力パスを扱えない場合があります。その場合は [`src-tauri/.cargo/config.toml.example`](src-tauri/.cargo/config.toml.example) を `src-tauri/.cargo/config.toml` にコピーし、`target-dir` を空白のないローカルパスへ変更してください。この設定ファイルは Git の追跡対象外です。

## 主な構成

```text
docs/0-requirements.md  要求仕様（設計の source of truth）
src/                  最小の TypeScript フロントエンド
src-tauri/src/        Tauri、PTY、設定・セッション保存の Rust 実装
portable-pty-patch/   Windows 対応を含む portable-pty のローカルパッチ
.github/workflows/    Windows CI とリリース用 CD
```

## ライセンス

liplus-chat は [Apache License 2.0](LICENSE) のもとで提供されます。著作権表示は [NOTICE.txt](NOTICE.txt) を参照してください。

`portable-pty-patch/` は MIT License のコードを含みます。詳細は [`portable-pty-patch/LICENSE.md`](portable-pty-patch/LICENSE.md) を参照してください。
