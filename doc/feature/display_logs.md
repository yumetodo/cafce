# cafce サブコマンド出力仕様

各サブコマンドが標準出力・標準エラー出力に出すメッセージの仕様書。

## key サブコマンド

`cafce key <config>` — config file を読み、primary cache key を計算して出力する。

### 正常系

| 出力先 | 内容 |
|--------|------|
| stdout | 計算された primary cache key の文字列（末尾に改行あり）|
| stderr | （なし） |

**stdout の出力例**

```
a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2
```

`key` が literal String 形態の場合（`${VAR}` 展開済みの文字列をそのまま出力）:

```
cache-main
```

`files` に 0 件マッチした場合（GitLab CI 互換フォールバック）:

```
default
```

または prefix あり（例: `prefix = "deps"`）の場合:

```
deps-default
```

### 異常系（stderr）

| 状況 | stderr メッセージ例 |
|------|-------------------|
| config ファイルが存在しない | `設定ファイルの読み込みに失敗しました: config.toml: No such file or directory (os error 2)` |
| TOML パースエラー | `設定ファイルのパースに失敗しました: ...` |
| `project` フィールドが欠落・空文字 | `project が空文字です（展開後も空文字の場合を含む）` |
| `${VAR}` で参照した変数が未定義 | `変数 ${CI_COMMIT_REF_SLUG} が定義されていません` |
| `files` に絶対パスを指定 | `絶対パスのパターンは指定できません: /etc/passwd` |
| ファイル数が上限を超えた | `ファイル数が制限を超えています: 60 > 50` |

いずれの異常系でも exit code は非 0。

## probe サブコマンド

`cafce probe <config>` — primary key と `fallback_keys` を順に S3 で存在確認し、結果を出力する。

### 正常系

| 出力先 | 内容 |
|--------|------|
| stdout | `true`（いずれかのキーが S3 に存在する場合）または `false`（全 miss の場合）|
| stderr | （なし） |

**stdout の出力例**

```
true
```

```
false
```

`probe` の stdout は `true` / `false` の 2 値のみ。使用された cache key や S3 オブジェクトキーは stdout には出力しない（debug が必要な場合は `cafce key` で生成キー文字列を確認する）。

### 異常系（stderr）

`key` サブコマンドと共通の異常系（config パース失敗、変数展開エラー等）に加え、以下が発生しうる。

| 状況 | stderr メッセージ例 |
|------|-------------------|
| `CAFCE_AWS_BUCKET` が未設定 | `環境変数の読み込みに失敗しました: CAFCE_AWS_BUCKET が設定されていません。キャッシュを置く S3 バケット名を指定してください` |
| S3 head_object で 403 AccessDenied | `S3 HeadObject に失敗しました (bucket=my-bucket, key=my-project/cache-v1)` + `403 の場合は s3:ListBucket 権限を確認してください` |
| S3 head_object でリージョン不一致等 | `S3 HeadObject に失敗しました (bucket=my-bucket, key=...)` |
| S3 クライアント構築失敗 | `S3 クライアントの構築に失敗しました: ...` |

**403 AccessDenied について**

AWS S3 では `s3:ListBucket` 権限が無いと、存在しないキーへの `HeadObject` が 404 ではなく 403 で返る。cafce はこれを cache miss として扱わず、エラーとして stderr に出力して exit 非 0 で終了する。silent な auth failure による恒常的 cache miss を防ぐためのフェイルファストである。

cafce が要求する IAM 権限は `s3:GetObject` と `s3:ListBucket` の両方。

いずれの異常系でも exit code は非 0。
