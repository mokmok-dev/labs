// デフォルトのデモ録画シナリオ。
// CI（.github/workflows/demo-video.yaml）はこのスクリプトを実行して
// 各画面の MP4 を生成する。PR固有のシナリオはこのディレクトリに
// 追加スクリプトを置いて拡張する。
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
  { name: 'home', label: 'ホーム画面', path: '/' },
  { name: 'table', label: 'データテーブル', path: '/table' },
  { name: 'about', label: 'アバウト', path: '/about' },
];

const browser = await launchBrowser();

for (const scenario of scenarios) {
  const context = await createContext(browser, VIDEO_DIR);
  const page = await context.newPage();

  await page.goto(`${BASE_URL}${scenario.path}`);
  await page.waitForLoadState('networkidle');
  await step(page, scenario.label, 2500);

  const webmPath = await finishRecording(page, context, VIDEO_DIR, scenario.name);
  await convertToMp4(webmPath, webmPath.replace('.webm', '.mp4'));
}

await browser.close();
console.log('全てのシナリオの録画が完了しました。');

