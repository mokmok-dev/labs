// デモ動画録画ユーティリティライブラリ
// Playwright Node.js APIを使用した画面録画 + クリック強調 + アノテーション
// 加えて、text inputのみのLLMが「コンポーネントの位置・見え方」を
// 会話できるよう、a11y tree / 要素の位置・スタイルのダンプ関数を提供する。

import { chromium } from 'playwright';
import { rename } from 'fs/promises';
import { join } from 'path';
import { execFileSync } from 'child_process';

/**
 * recordVideo付きBrowserContextを生成する（1280x720）
 * @param {import('playwright').Browser} browser
 * @param {string} videoDir - 動画出力先ディレクトリ
 * @returns {Promise<import('playwright').BrowserContext>}
 */
export async function createContext(browser, videoDir) {
  return browser.newContext({
    viewport: { width: 1280, height: 720 },
    recordVideo: {
      dir: videoDir,
      size: { width: 1280, height: 720 },
    },
    locale: 'ja-JP',
  });
}

/**
 * ブラウザを起動する
 * `playwright install chromium` で取得したブラウザを使用する。
 * @param {boolean} headless - ヘッドレスモード（デフォルトtrue）
 * @returns {Promise<import('playwright').Browser>}
 */
export async function launchBrowser(headless = true) {
  return chromium.launch({ headless });
}

/**
 * 画面下部中央にアノテーション（ステップ説明）を表示する
 * @param {import('playwright').Page} page
 * @param {string} text - 表示テキスト
 * @param {number} ms - 表示時間（ミリ秒、デフォルト2000）
 */
export async function step(page, text, ms = 2000) {
  await page.evaluate((t) => {
    let el = document.getElementById('demo-annotation');
    if (!el) {
      el = document.createElement('div');
      el.id = 'demo-annotation';
      el.style.cssText = `
        position: fixed; bottom: 24px; left: 50%;
        transform: translateX(-50%); z-index: 99999;
        background: rgba(0,0,0,0.8); color: #fff;
        padding: 10px 28px; border-radius: 8px;
        font-size: 16px; font-family: sans-serif;
        transition: opacity 0.3s;
        white-space: nowrap;
      `;
      document.body.appendChild(el);
    }
    el.textContent = t;
    el.style.opacity = '1';
  }, text);
  await page.waitForTimeout(ms);
}

/**
 * アノテーションをフェードアウトする
 * @param {import('playwright').Page} page
 */
export async function clearAnnotation(page) {
  await page.evaluate(() => {
    const el = document.getElementById('demo-annotation');
    if (el) el.style.opacity = '0';
  });
}

/**
 * 対象要素にリップルエフェクトを表示してからクリックする
 * @param {import('playwright').Page} page
 * @param {string} selector - クリック対象のCSSセレクタ
 */
export async function clickWithHighlight(page, selector) {
  const locator = page.locator(selector).first();
  await locator.scrollIntoViewIfNeeded();
  const box = await locator.boundingBox();
  if (!box) throw new Error(`Element not found: ${selector}`);

  const x = box.x + box.width / 2;
  const y = box.y + box.height / 2;

  await page.evaluate(({ x, y }) => {
    if (!document.getElementById('demo-ripple-style')) {
      const style = document.createElement('style');
      style.id = 'demo-ripple-style';
      style.textContent = `
        @keyframes demo-ripple {
          0% { transform: scale(0.5); opacity: 1; }
          100% { transform: scale(2.5); opacity: 0; }
        }
      `;
      document.head.appendChild(style);
    }

    const ripple = document.createElement('div');
    ripple.className = 'demo-click-ripple';
    ripple.style.cssText = `
      position: fixed; left: ${x - 20}px; top: ${y - 20}px;
      width: 40px; height: 40px; border-radius: 50%;
      background: rgba(255, 82, 82, 0.5);
      border: 3px solid rgba(255, 82, 82, 0.8);
      pointer-events: none; z-index: 99999;
      animation: demo-ripple 0.6s ease-out forwards;
    `;
    document.body.appendChild(ripple);
    setTimeout(() => ripple.remove(), 600);
  }, { x, y });

  await page.waitForTimeout(300);
  await locator.click();
  await page.waitForTimeout(300);
}

/**
 * 対象フィールドを強調してからテキストを入力する
 * @param {import('playwright').Page} page
 * @param {string} selector - 入力対象のCSSセレクタ
 * @param {string} text - 入力テキスト
 */
export async function fillWithHighlight(page, selector, text) {
  const locator = page.locator(selector).first();
  await locator.scrollIntoViewIfNeeded();

  await locator.evaluate((el) => {
    el.style.outline = '3px solid rgba(66, 133, 244, 0.8)';
    el.style.outlineOffset = '2px';
    el.style.transition = 'outline 0.3s';
  });
  await page.waitForTimeout(300);

  await locator.fill(text);
  await page.waitForTimeout(200);

  await locator.evaluate((el) => {
    el.style.outline = '';
    el.style.outlineOffset = '';
  });
}

/**
 * セレクトボックスを強調してから値を選択する
 * @param {import('playwright').Page} page
 * @param {string} selector - セレクト要素のCSSセレクタ
 * @param {string} value - 選択する値
 */
export async function selectWithHighlight(page, selector, value) {
  const locator = page.locator(selector).first();
  await locator.scrollIntoViewIfNeeded();

  await locator.evaluate((el) => {
    el.style.outline = '3px solid rgba(66, 133, 244, 0.8)';
    el.style.outlineOffset = '2px';
    el.style.transition = 'outline 0.3s';
  });
  await page.waitForTimeout(300);

  await locator.selectOption(value);
  await page.waitForTimeout(400);

  await locator.evaluate((el) => {
    el.style.outline = '';
    el.style.outlineOffset = '';
  });
}

/**
 * confirm()ダイアログを自動承認する
 * @param {import('playwright').Page} page
 */
export function acceptNextDialog(page) {
  page.once('dialog', (dialog) => dialog.accept());
}

/**
 * 録画を終了し、動画ファイルをリネームする
 * @param {import('playwright').Page} page
 * @param {import('playwright').BrowserContext} context
 * @param {string} outputDir - 出力先ディレクトリ
 * @param {string} outputName - 出力ファイル名（拡張子なし）
 * @returns {Promise<string>} 出力ファイルパス
 */
export async function finishRecording(page, context, outputDir, outputName) {
  await page.close();
  const video = page.video();
  if (!video) throw new Error('No video recorded');

  const tempPath = await video.path();
  await context.close();

  const finalPath = join(outputDir, `${outputName}.webm`);
  await rename(tempPath, finalPath);
  console.log(`  録画完了: ${finalPath}`);
  return finalPath;
}

/**
 * WebMファイルをMP4に変換する（ffmpeg使用）
 * ffmpeg がPATHに無い場合は変換をスキップし、WebM のまま出力する。
 * @param {string} inputPath - 入力WebMファイルパス
 * @param {string} outputPath - 出力MP4ファイルパス
 */
export async function convertToMp4(inputPath, outputPath) {
  console.log(`  MP4変換中: ${inputPath} → ${outputPath}`);
  try {
    execFileSync('ffmpeg', [
      '-y', '-i', inputPath,
      '-c:v', 'libx264', '-preset', 'fast', '-crf', '23',
      '-pix_fmt', 'yuv420p',
      outputPath,
    ], { stdio: 'pipe' });
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    console.warn(`  警告: ffmpeg が見つからないため WebM のまま出力します: ${inputPath}`);
    return;
  }
  console.log(`  MP4変換完了: ${outputPath}`);
}

/**
 * アクセシビリティツリーをダンプする（CDP Accessibility domain）。
 * text inputのみのLLMが「今画面に何が表示されているか」を把握するために使う。
 * @param {import('playwright').Page} page
 * @returns {Promise<string>} インデント付きa11yツリー
 */
export async function snapshotA11y(page) {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Accessibility.enable');
  const { nodes } = await cdp.send('Accessibility.getFullAXTree');

  const interesting = (n) =>
    n.ignored !== true &&
    (n.name?.value ||
      n.role?.value === 'button' ||
      n.role?.value === 'link' ||
      n.role?.value === 'textbox');

  const lines = [];
  const walk = (n, depth) => {
    if (n.ignored !== true && interesting(n)) {
      const role = n.role?.value ?? '';
      const name = n.name?.value ?? '';
      lines.push(`${'  '.repeat(depth)}[${role}] ${name}`.trimEnd());
    }
    for (const child of n.childIds ?? []) {
      const c = nodes.find((x) => x.nodeId === child);
      if (c) walk(c, depth + 1);
    }
  };

  for (const n of nodes) {
    if (!n.parentId) walk(n, 0);
  }
  return lines.join('\n');
}

/**
 * 要素の位置・見え方をダンプする。
 * text inputのみのLLMが「◯◯ボタンはどこにある？見え方は？」
 * を会話するための情報源。
 * @param {import('playwright').Page} page
 * @param {string} selector - 対象CSSセレクタ
 * @returns {Promise<object>} 位置・スタイル情報
 */
export async function whereIs(page, selector) {
  const locator = page.locator(selector).first();
  const box = await locator.boundingBox();
  const styles = await locator.evaluate((el) => {
    const s = getComputedStyle(el);
    const r = el.getBoundingClientRect();
    return {
      display: s.display,
      visibility: s.visibility,
      opacity: s.opacity,
      zIndex: s.zIndex,
      position: s.position,
      backgroundColor: s.backgroundColor,
      color: s.color,
      fontSize: s.fontSize,
      fontFamily: s.fontFamily,
      tag: el.tagName,
      text: (el.textContent ?? '').trim().slice(0, 80),
      viewport: { width: window.innerWidth, height: window.innerHeight },
      rect: {
        top: Math.round(r.top),
        left: Math.round(r.left),
        right: Math.round(r.right),
        bottom: Math.round(r.bottom),
      },
    };
  });
  return { selector, boundingBox: box, styles };
}

/**
 * 2つの要素の位置関係を判定する。
 * @param {import('playwright').Page} page
 * @param {string} selectorA
 * @param {string} selectorB
 * @returns {Promise<object>}
 */
export async function comparePosition(page, selectorA, selectorB) {
  const a = page.locator(selectorA).first();
  const b = page.locator(selectorB).first();
  const [boxA, boxB] = await Promise.all([a.boundingBox(), b.boundingBox()]);
  if (!boxA || !boxB) throw new Error('Both elements must be visible');

  const dx = boxB.x - boxA.x;
  const dy = boxB.y - boxA.y;
  const overlapX = Math.max(0, Math.min(boxA.x + boxA.width, boxB.x + boxB.width) - Math.max(boxA.x, boxB.x));
  const overlapY = Math.max(0, Math.min(boxA.y + boxA.height, boxB.y + boxB.height) - Math.max(boxA.y, boxB.y));
  const overlaps = overlapX > 0 && overlapY > 0;

  return {
    a: boxA,
    b: boxB,
    relation: {
      horizontal: overlaps ? 'overlap' : dx >= 0 ? 'right-of' : 'left-of',
      vertical: overlaps ? 'overlap' : dy >= 0 ? 'below' : 'above',
      overlaps,
      overlapPx: { x: Math.round(overlapX), y: Math.round(overlapY) },
    },
  };
}

