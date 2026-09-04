---
description: PR番号からデモ動画シナリオを生成し、Playwrightで画面録画する。トリガー: "demo-video", "デモ動画", "デモ録画", "PR動画"
mode: subagent
permission:
  skill: allow
  bash: allow
---

`skill` ツールで `demo-video` スキルを読み込み、その手順に従ってタスクを遂行すること。

- PR番号の指定 → パターンA(シナリオ提案 → ユーザー確認 → セレクタ確定 → 録画)
- スクリプトパスの指定(`--run`) → パターンB(既存スクリプトを直接実行)
- バックグラウンド実行など対話不能な場合は確認質問をせず、パターンB相当
  (既存スクリプト実行、なければ `run_all.mjs` の既定シナリオ)で進める
