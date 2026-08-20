# liplus-chat 要求仕様

## purpose

人間と複数の独立した AI セッションが、一つのローカルな会話面を共有するためのデスクトップアプリケーション。

アプリが持つのは**部屋の壁だけ**である。機能・ツール・認証・MCP は各 CLI がそのまま持ち、アプリはそれらを内包しない。各セッションの文脈と人格は独立したまま保たれ、発言者を識別できる共通の場で対話する。

狙いは `Liplus-Project/liplus-language` が自ら未達と記している基盤層である。判断層の Sheepdog は到達済みで、保留されているのは物理的なイベント駆動基盤（polling-on-input からの脱却）のみであり、本アプリはそこを閉じる。

設計母体は `Liplus-Project/liplus-desktop` #90。前身である liplus-desktop が掲げていた「アダプター層・タスク層・オペレーション層を UI 側に内包し、CLI にはモデル層のみを渡す」方針は撤回済みである。Li+ の L6 Adapter Layer は基盤を所有しないための層であり、内包はその反転にあたる。

## premise

### 実測で確認済み（2026-08-19、liplus-desktop #90）

- **channel による双方向疎通が成立する。** サーバが `notifications/claude/channel` を push した約 10 秒後（モデル思考時間込み）、ユーザー入力を一切与えない状態でエージェントが自発的に反応し、返信用 MCP tool を呼び返した。
- 検証環境: Claude Code CLI `2.1.209` / Windows / PTY を与えた対話セッション / 依存ライブラリなしの生 JSON-RPC で書いた最小 MCP サーバ。
- 入力路 = channel notification、出力路 = 通常の MCP tool。webhook 版の一方向 channel と異なり、会話に必要な双方向が成立する。

### 成立条件（すべて実測で確定。いずれも外すと不通になる）

- MCP サーバは `.mcp.json` 等へ**名前で正式登録**が必須。`--mcp-config` によるファイル渡しでは channel 側が名前を解決できず失敗する。
- 起動フラグは `--dangerously-load-development-channels server:<name>` を**単独指定**。`--channels` を併記すると同一サーバが二重登録され、非 dev 側のエントリが allowlist で弾かれて全体が不通になる。
- エントリはタグ必須（`server:<name>` または `plugin:<name>@<marketplace>`）。
- `--print`（`--input-format` / `--output-format stream-json` を含む）では push が届かない。**対話セッションが必須**であり、これが PTY を必要とする理由である。
- `claude/channel` を宣言するのは**サーバ側**であり、ホストはそれを消費する側。ホストが initialize で申告する capability はフラグの有無に関わらず変化しない。
- `--channels` / `--dangerously-load-development-channels` はいずれも `--help` 非掲載（`2.1.209`）。

### liplus-chat の現況

- Tauri 2 / Vite / TypeScript のスキャフォールドが存在する。
- Rust 側に PTY 層（`spawn_pty` / `write_pty` / `resize_pty` / `kill_pty`）、config 層、session 保存が実装済み。
- フロントエンドはプレースホルダーであり、チャットルーム UI と channel 連携は未実装。

## constraints

- **アプリは部屋であってランタイムではない。** Li+ の各層を UI 側のコードとして実装しない。所有した瞬間に MCP 管理・OAuth・設定 UI をすべて自前で抱えることになり、ニッチ化と保守費はその代償として必ず発生する。
- **単一ベンダー構成とする。** 二ベンダーによる独立性確保とサブスク枠の分散は取り下げる。構造を単純化し、他ベンダー側 channel 相当の有無という未検証前提を消すための決定。
  - **代償**: 常駐する全エージェントが同一の利用枠（5 時間枠）を共有する。同時発話数と常駐エージェント数を設計側で抑えること。
- **AI 同士の暴走や対立を意図的に誘発する機構は設けない。** 現在の Li+ を自然に動かした結果として予期しない相互作用が生じた場合は、発言者と会話の流れを追える形で観測できることを目指す。
- **エージェント間の対話を GitHub へ流さない。** `rules/task/task.md` が両者を別物として定義している（`Issue body = judgment record` / `Dialogue message = history`）。加えて書き込み量には実測済みの制約がある。
- **プレビュー結合を受容する。** channel は experimental capability であり、仕様変更がアプリに波及する。早期採用の代償として明示的に受容する。
- **構想上の機能と実装済みの機能を明確に区別する。** 未実装を完成済みとして記載しない。
- ライセンスは Apache-2.0。`portable-pty-patch/` は MIT License のコードを含む。
- Windows を第一対象とする（ConPTY 依存）。
- 実装変更と docs 更新は同一 PR とする。

## architecture

```
liplus-chat (Tauri app)  =  部屋の壁
├── UI Layer (WebView)
│   ├── チャットルーム（メッセージ一覧 / 発言者表示 / 入力欄）
│   └── セッション制御（起動 / 停止 / タブ設定）
├── App Layer (Rust)
│   ├── PTY 管理（portable-pty: spawn / write / resize / kill）— 対話セッションの保持
│   ├── config / sessions 永続化（JSON）
│   └── Tauri IPC（invoke / emit）
├── MCP Channel Server（Node サイドカー）
│   ├── initialize: capabilities.experimental["claude/channel"] を宣言
│   ├── initialize: instructions で部屋の作法を全エージェントへ配布
│   ├── push: notifications/claude/channel（部屋 → エージェント）
│   └── tool: 返信受け口（エージェント → 部屋）
└── CLI Processes（PTY 経由の対話セッション）
    └── 機能・ツール・認証・MCP はすべて CLI 側が保持する
```

責務境界:

| 所有者 | 持つもの |
|---|---|
| アプリ | 部屋の壁、発言者の識別、会話の並び、セッションの起動と保持 |
| MCP サーバ | 部屋とエージェント間の入出力路、部屋の作法（instructions） |
| CLI | モデル、ツール、認証、MCP 接続、Li+ の全層 |

### 会話ループ

```
[人間の発言] → 部屋 → MCP サーバ
                         ↓ notifications/claude/channel
                    [CLI 対話セッション（PTY 保持）]
                         ↓ MCP tool 呼び出し
                    MCP サーバ → 部屋 → [メッセージ一覧へ追加]
```

push の形（参照実装 `Liplus-Project/github-webhook-mcp` `local-mcp/src/index.ts` に準拠）:

- method = `notifications/claude/channel`
- `params.content` = 発言本文
- `params.meta` = `chat_id` / `message_id` / `user` / `ts` ほか、発言者と会話を追跡するための属性

返信路は通常の MCP tool であり、channel 固有の機構を必要としない。

### 部屋の作法

部屋のルールは MCP サーバが initialize 時に返す `instructions` 文字列により全エージェントへ一元的に配布する。実測では、当該ターンで返信を指示していないにもかかわらずエージェントが返信 tool を呼んだ。駆動源はこの `instructions` である。

同時発話数の抑制、発言の宛先、沈黙してよい条件などはここで与える。

### MCP サーバの実装方式

参照実装は TypeScript / Node、本リポジトリは Rust + TypeScript であるため二択となる。

- (a) Rust へ移植しアプリ本体へ内蔵する。プロセスは 1 つで済むが、参照実装は書き直しとなり、channel のプレビュー仕様が動くたびに Rust 側の改修が要る。
- (b) **Node のサイドカーとして同梱する（採用）。** 実証済みの実装をほぼそのまま使え、仕様追随が軽い。プロセスは増える。

疎通検証自体を Node の最小サーバで通しており、実測の裏付けがある側を採る。

### 再利用範囲

PTY 層と config 層は liplus-desktop から移植済みであり、上記 premise により必要であるため再利用する。

liplus-desktop の `stream_parser.rs` および `spawn_stream_pty` / `spawn_stream_pipe` は引き継がない。「CLI 出力をパースして channel push を検出する」という、本設計とは逆向きのモデルに基づくためである。本設計では CLI の出力パースを会話の情報源としない。

## 実装状況

### 実装済み

- Tauri 2 による Windows デスクトップアプリの基盤
- Vite と TypeScript による最小のフロントエンド
- Rust の PTY コマンド（起動 / 入力 / リサイズ / 終了）
- タブ設定とセッションデータの JSON 永続化
- Windows CI（`npm ci` と Rust コンパイル確認）、Release 公開時の CD

### 未実装

- チャットルーム UI（メッセージ一覧、発言者表示、入力欄）
- MCP channel サーバ（Node サイドカー）
- サーバの `.mcp.json` 登録と、成立条件を満たす CLI 起動フラグの適用
- 部屋の作法（`instructions`）の設計と配布
- 会話ログと観測 UI

## 配布

段階を分ける。第二段階は実装ではなく手続きのトラックであり、第一段階の完了条件には含めない。

- **第一段階**: 開発者自身の環境で動かす。`--dangerously-load-development-channels` を用いる。自作サーバは承認済み channel allowlist に載っていないため、起動ごとに警告バナーが出る。同フラグはローカルの channel 開発専用と明示されている。
- **第二段階**: plugin としてマーケットプレイス経由で allowlist に載せ、一般配布可能にする（`plugin:<name>@<marketplace>`）。組織向けには managed settings の `channelsEnabled` / `allowedChannelPlugins` がある。

## 受容したトレードオフ

| 決定 | 代償 |
|---|---|
| 単一ベンダー構成 | 常駐エージェントが同一の利用枠を共有する |
| Node サイドカー方式 (b) | プロセスが増える |
| channel の早期採用 | experimental 仕様の変更がアプリへ波及する |
| 第一段階の dev フラグ運用 | 起動ごとに警告バナーが出る。一般配布は不可 |

## 位置づけ

Claude Desktop の置き換えではない。単独作業は Desktop、並列複数体の対話は本アプリ、と用途で分ける。同一の Li+ が両方に乗ることは L6 Adapter Layer が保証している。

## 開発環境とビルド手順

[README.md](../README.md) を参照。本仕様では再記述しない。

## 関連

- `Liplus-Project/liplus-desktop` #90 — 設計母体。
- `Liplus-Project/github-webhook-mcp` `local-mcp/src/index.ts` — channel 実装の参照元。
- `Liplus-Project/liplus-language` — 基盤層 Sheepdog 未達の記述（`adapter/claude/CLAUDE.md` 冒頭 Concept framing）。
