// デフォルトのデモ録画シナリオ。
// echonet-radar の webview UI（単一ページ）を録画する。
// PR固有のシナリオは `.tmp/demo/` に追加スクリプトを置いて拡張する。
import {
  launchBrowser,
  createContext,
  step,
  clickWithHighlight,
  finishRecording,
  convertToMp4,
} from '../lib/helpers.mjs';

const BASE_URL = process.env.DEMO_BASE_URL || 'http://localhost:5173';
const VIDEO_DIR = '.tmp/demo/videos';

const scenarios = [
  { name: 'home', label: 'echonet-radar トップ画面（ヘッダーとイベントテーブル）', path: '/' },
];

const browser = await launchBrowser();

for (const scenario of scenarios) {
  const context = await createContext(browser, VIDEO_DIR);
  const page = await context.newPage();

  await page.goto(`${BASE_URL}${scenario.path}`);
  await page.waitForLoadState('networkidle');
  await step(page, scenario.label, 2500);
  await clickWithHighlight(page, 'button:has-text("Poll now")');
  await step(page, 'Poll now でデバイスの再取得を実行', 2000);

  const webmPath = await finishRecording(page, context, VIDEO_DIR, scenario.name);
  await convertToMp4(webmPath, webmPath.replace('.webm', '.mp4'));
}

await browser.close();
console.log('全てのシナリオの録画が完了しました。');

