---
description: PR番号からデモ動画シナリオを生成し、Playwrightで録画する (例: /demo-video 2)
agent: demo-video
subtask: true
---

次の依頼に基づいてデモ動画を作成する: $ARGUMENTS

引数が空の場合は、既定の全画面巡回シナリオ
(`.agents/skills/demo-video/scripts/run_all.mjs`) の実行を提案する。
