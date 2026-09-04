---
name: demo-video
description: |
  PR番号からデモ動画シナリオを生成し、Playwrightで画面録画する。
  クリック箇所のリップル強調 + アノテーション表示付き。
  text inputのみのLLMがコンポーネントの位置・見え方を会話で確認できるヘルパー付き。
  トリガー: "demo-video", "デモ動画", "デモ録画", "PR動画"
  使用場面: (1) PRのデモ動画作成、(2) 機能紹介動画の録画、(3) 手順の動画化
---

# Demo Video（デモ動画生成）

PRの変更内容を分析し、Playwrightで画面操作を録画してデモ動画を生成するスキル。
クリック箇所にリップル強調、画面下部にステップ説明のアノテーションを表示する。

本リポジトリの `echonet-radar` webview UI（`echonet-radar/web`: React + Vite、
単一ページ・ルーティングなし）向けに設計している。ログイン認証は不要。
WebSocket（`/ws`）で ECHONET Lite のデバイス検出イベントを受け取っており、
未接続時は「reconnecting」バッジと空テーブル
（Waiting for ECHONET Lite device activity…）が表示される。

## 使い方

```
/demo-video 2                  # PR番号からシナリオを自動提案・生成
/demo-video --run path.mjs     # 既存スクリプトを直接実行
```

上記コマンドのほか、以下の起動方法がある。

- `@demo-video` でメンションして subagent として実行
  （メインセッションのコンテキストを汚さない）
- `opencode run --agent demo-video "..."` でヘッドレス実行
- `OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS=true` でバックグラウンド実行
  （完了時に通知）

バックグラウンド実行など対話不能な場合はパターンB（既存スクリプト実行）のみ
扱う。パターンAのシナリオ承認（手順2）は対話が必要なため、対話が必要な
依頼はメインセッションか `@demo-video` メンションで実行する。

## 実行手順

### パターンA: PR番号指定（シナリオ自動生成）

#### 1. PR情報の取得と分析

```bash
gh pr view <PR番号> --json title,body,files,baseRefName
```

変更ファイルから影響する画面要素を特定する。
UIは `echonet-radar/web/src/`（単一ページの `App.tsx`）、
Rust側（webview・WSサーバー）は `echonet-radar/src/`。

#### 2. シナリオ提案

分析結果をもとに、以下の形式でシナリオを提案する。

```
シナリオ1: [タイトル]
  操作: [画面遷移] → [操作1] → [操作2] → ...
  確認ポイント: [何が見えればOKか]

シナリオ2: [タイトル]
  ...
```

ユーザーに確認を取り、必要に応じて調整する。

#### 3. text-only LLMによる画面の位置・見え方の確認

シナリオ内で操作するコンポーネントのセレクタを確定するため、
テキストのみのLLM（本エージェント自身）が以下のヘルパーで会話できる。

```bash
node -e '
  import("./.agents/skills/demo-video/lib/helpers.mjs").then(async (h) => {
    const { chromium } = await import("playwright");
    const b = await h.launchBrowser(true);
    const c = await b.newContext({ viewport: { width: 1280, height: 720 } });
    const p = await c.newPage();
    await p.goto("http://localhost:5173/");
    await p.waitForLoadState("networkidle");
    console.log("==== A11Y ====");
    console.log(await h.snapshotA11y(p));
    console.log("==== WHERE ====");
    console.log(await h.whereIs(p, "h1"));
    await b.close();
  });
'
```

- `snapshotA11y(page)` … アクセシビリティツリー（現在表示中の要素一覧）
- `whereIs(page, selector)` … 要素の位置（boundingBox）と見え方（computed style）
- `comparePosition(page, selA, selB)` … 2要素の位置関係（上/下/左/右/重なり）

これらを繰り返し、セレクタとレイアウトを会話で確定してから録画スクリプトを生成する。

#### 4. スクリプト生成

確定したシナリオを `.tmp/demo/` に Playwright スクリプトとして生成する。

各スクリプトは以下のヘルパーを使用する（`.agents/skills/demo-video/lib/helpers.mjs`）:

| 関数 | 用途 |
|------|------|
| `launchBrowser()` | ブラウザ起動（PLAYWRIGHT_BROWSERS_PATH 未設定なら自動DLのChromium） |
| `createContext(browser, videoDir)` | 録画付きコンテキスト生成（1280x720） |
| `step(page, text, ms?)` | アノテーション表示 + 待機 |
| `clickWithHighlight(page, selector)` | リップル強調 → クリック |
| `fillWithHighlight(page, selector, text)` | フィールド強調 → 入力 |
| `selectWithHighlight(page, selector, value)` | セレクト強調 → 選択 |
| `acceptNextDialog(page)` | confirmダイアログ自動承認 |
| `finishRecording(page, context, dir, name)` | 録画終了 + ファイルリネーム |
| `convertToMp4(input, output)` | WebM → MP4変換 |

スクリプトのテンプレート:

```javascript
import {
  launchBrowser, createContext,
  step, clickWithHighlight,
  finishRecording, convertToMp4,
} from '../../.agents/skills/demo-video/lib/helpers.mjs';

const BASE_URL = process.env.DEMO_BASE_URL || 'http://localhost:5173';
const VIDEO_DIR = '.tmp/demo/videos';

const browser = await launchBrowser();
const context = await createContext(browser, VIDEO_DIR);
const page = await context.newPage();

await page.goto(`${BASE_URL}/`);
await page.waitForLoadState('networkidle');

// --- シナリオ固有の操作 ---
await step(page, 'ステップの説明');
await clickWithHighlight(page, 'セレクタ');
// ---

const webmPath = await finishRecording(page, context, VIDEO_DIR, 'scenario_name');
await convertToMp4(webmPath, webmPath.replace('.webm', '.mp4'));
await browser.close();
```

#### 5. 録画実行

```bash
pnpm --dir echonet-radar/web dev &    # vite devサーバー（スタイル確認用）
node .agents/skills/demo-video/scripts/run_all.mjs
```

データが入った状態で録画する場合は Rust アプリを起動し、表示されたURLを
`DEMO_BASE_URL` に指定する（埋め込みUI + 実デバイス検出の `/ws` が含まれる）。

```bash
cargo run -p echonet-radar            # 起動時に http://127.0.0.1:<port> を表示
DEMO_BASE_URL=http://127.0.0.1:<port> node .agents/skills/demo-video/scripts/run_all.mjs
```

コミット済みのデフォルトシナリオ（単一ページのヘッダー・テーブル確認）は
`.agents/skills/demo-video/scripts/run_all.mjs`。PR固有のシナリオは
`.tmp/demo/` に置いた専用スクリプトで実行する。

#### 6. 出力確認

```bash
ls -la .tmp/demo/videos/*.mp4
```

出力先パスをユーザーに報告する。ffmpeg が無い場合は WebM のまま出力される
（GitHub コメントの `<video>` 埋め込みで再生できるのは MP4 のみ）。

### パターンB: 既存スクリプト実行

```bash
node <指定されたスクリプトパス>
```

## 環境変数

| 変数 | デフォルト | 説明 |
|------|-----------|------|
| `DEMO_BASE_URL` | `http://localhost:5173` | 対象サーバーURL（vite dev または cargo run で表示されるURL） |
| `DEMO_HEADLESS` | `true` | ヘッドレスモード（falseで表示） |

## 前提条件

- Node.js 24（Nix devShell `nix develop` / direnv で提供）
- `playwright` npmパッケージ: スキルディレクトリ内
  （`.agents/skills/demo-video/package.json`）で管理。初回のみインストールする:

  ```bash
  pnpm --dir .agents/skills/demo-video install
  pnpm --dir .agents/skills/demo-video exec playwright install chromium
  ```

- ffmpeg: MP4変換に使用。PATHに無い場合は変換をスキップし、WebM のまま出力する
- 対象サーバーが起動済み（vite dev または `cargo run -p echonet-radar`）

## 出力

動画ファイルは `.tmp/demo/videos/` に出力される。

- WebM形式: Playwright録画のネイティブ出力
- MP4形式: ffmpegで変換後（PRへの添付やSlack共有に便利）

