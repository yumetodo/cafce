# CLAUDE.md

## コードを読む際の注意: `doc/design/` の Design Doc について

このプロジェクトは ADR を採用せず、`doc/design/` 配下の Design Doc がその代替を部分的に担う。ただし Design Doc は Living Document ではない。

- Design Doc の有効期間は、それが作成された作業ブランチが生きている間のみ。ブランチがマージ・削除された後は、その Design Doc を「作成時点の設計意図を記録した歴史的資料」としてのみ扱うこと。以後、実装が変わっても Design Doc 側は更新されない前提である。
- そのため、コードの**現在の**仕様・挙動を確認したいときは、Design Doc を一次情報にしない。実際のソースコード・テスト・コミットログを一次情報として参照すること。Design Doc は「なぜその設計にしたか」という意図・背景を理解する目的にのみ使う。
- Design Doc の内容と実装の現在の挙動が食い違っていても、Design Doc 側の記述ミスとは限らない。Design Doc 作成後に実装側が変更された可能性を先に疑うこと。
- 日付の古い Design Doc を見つけても、「最新化されていない」と早合点して書き換えない。それが仕様である。

新しい Design Doc を作成する場合は `design-doc` skill（`.claude/skills/design-doc/`）を使う。
