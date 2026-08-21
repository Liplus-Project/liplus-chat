# liplus-chat

> [!IMPORTANT]
> 部屋の一往復（人間の発言 → channel → セッションの返信 → 部屋への表示）は実装済みですが、実機での往復は未検証です。配布は開発者環境向けの第一段階のみで、起動時に警告バナーが出ます。

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
- プロセスの起動、入力、リサイズ、終了を扱う Rust の PTY コマンド
- タブ設定とセッションデータを JSON へ保存・読込する Tauri コマンド
- MCP channel サーバ（Node サイドカー、`sidecar/`）
- 部屋ソケット（`127.0.0.1` の任意ポート、Bearer トークン必須）
- `.mcp.json` への登録と、channel が成立する条件を満たした CLI 起動
- メッセージ一覧・発言者表示・入力欄を備えたチャットルーム UI
- Windows 上で `npm ci` と Rust のコンパイル確認を行う CI
- GitHub Release 公開時に Tauri バンドルを作成する CD

### 未検証

コンパイルと自動テストは通っていますが、実機での往復は未確認です。

- 実際の CLI セッションが部屋へ参加し、1 往復が成立すること
- 往復の所要時間

### 未実装

- 複数の AI セッションを同一の部屋へ参加させる運用（同時発話の抑制を含む）
- 会話ログの永続化と観測 UI
- `cargo test` と `npm run sidecar:test` の CI 実行
- plugin としての allowlist 掲載（配布の第二段階）

## 部屋を動かす

```powershell
npm run tauri dev
```

起動すると部屋ソケットが待ち受けを始めます。タイトルバーでセッションを選び「セッション参加」を押すと、次の 2 つが行われます。

1. そのタブの作業ディレクトリの `.mcp.json` へ、部屋のサイドカーを `liplus-chat-room` という名前で登録します。**既存の内容はマージして保持します**が、あなたのリポジトリのファイルを書き換える操作です。
2. `--dangerously-load-development-channels server:liplus-chat-room` を付けて CLI を PTY 上の対話セッションとして起動します。

> [!NOTE]
> このフラグはローカルの channel 開発専用であり、起動ごとに警告バナーが出ます。一般配布には plugin として allowlist に載せる必要があり、そちらは未対応です。

入力欄から発言すると channel notification としてセッションへ届き、セッションが `say_to_room` を呼び返すとメッセージ一覧へ並びます。往復が成立しないときの切り分け手順は [`docs/0-requirements.md`](docs/0-requirements.md) を参照してください。

### サイドカー単体の確認

```powershell
npm run sidecar:check
npm run sidecar:test
```

`sidecar:test` は偽の部屋ソケットを立てて MCP と WebSocket の両面を駆動します。これが通れば、切り分けの対象をアプリ側へ絞れます。

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
sidecar/              部屋の MCP channel サーバ（Node）
src/                  チャットルーム UI（TypeScript）
src-tauri/src/        Tauri、部屋ソケット、PTY、設定・セッション保存の Rust 実装
portable-pty-patch/   Windows 対応を含む portable-pty のローカルパッチ
.github/workflows/    Windows CI とリリース用 CD
```

## ライセンス

liplus-chat は [Apache License 2.0](LICENSE) のもとで提供されます。著作権表示は [NOTICE.txt](NOTICE.txt) を参照してください。

`portable-pty-patch/` は MIT License のコードを含みます。詳細は [`portable-pty-patch/LICENSE.md`](portable-pty-patch/LICENSE.md) を参照してください。
