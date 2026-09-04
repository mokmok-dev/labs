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

本リポジトリ（console: React + TanStack Router + Vite の SPA）向けに設計している。
ログイン認証は不要（`/api/*` は未接続のため、画面遷移のみを対象とする）。

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

変更ファイルから影響する画面・ルートを特定する。
ルートは `apps/console/src/routes/` のファイルベースルーティングに対応する。

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
    await p.goto("http://localhost:5173/table");
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
pnpm dev &                        # 開発サーバー起動（別ターミナル）
node .agents/skills/demo-video/scripts/run_all.mjs
```

コミット済みのデフォルトシナリオ（全画面を巡回）は
`.agents/skills/demo-video/scripts/run_all.mjs`。PR固有のシナリオは
`.tmp/demo/` に置いた専用スクリプトで実行する。

#### 6. 出力確認

```bash
ls -la .tmp/demo/videos/*.mp4
```

出力先パスをユーザーに報告する。MP4 は PR コメントへ手動添付できる
（GitHub はコメントの `<video>` 埋め込みで mp4 を再生できる）。

### パターンB: 既存スクリプト実行

```bash
node <指定されたスクリプトパス>
```

## 環境変数

| 変数 | デフォルト | 説明 |
|------|-----------|------|
| `DEMO_BASE_URL` | `http://localhost:5173` | 対象サーバーURL（vite dev） |
| `DEMO_HEADLESS` | `true` | ヘッドレスモード（falseで表示） |

## 前提条件

- Nix devShell: `nix develop`（`flake.nix` が `playwright-test`, `ffmpeg`,
  `PLAYWRIGHT_BROWSERS_PATH`（nixpkgs製Chromium）を提供）
- `playwright` npmパッケージ: ルート `package.json` の devDependencies で管理
  （バージョンは `pnpm-workspace.yaml` の catalog と nixpkgs の playwright と一致させる）
- フォールバック: nixpkgs の Chromium がビルドできない環境では
  `npx playwright install chromium` で取得したブラウザを使う
  （`PLAYWRIGHT_BROWSERS_PATH` を unset すれば自動検出）
- 対象サーバーが起動済み（`pnpm dev`）

## 出力

動画ファイルは `.tmp/demo/videos/` に出力される。

- WebM形式: Playwright録画のネイティブ出力
- MP4形式: ffmpegで変換後（PRへの添付やSlack共有に便利）

