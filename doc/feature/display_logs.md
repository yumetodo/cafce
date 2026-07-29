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

cafce が要求する IAM 権限は `s3:GetObject` と `s3:ListBucket` の両方（`store` を使う場合は `s3:PutObject` も）。

いずれの異常系でも exit code は非 0。

## store サブコマンド

`cafce store <config>` — `paths` にマッチしたファイル/ディレクトリを決定論的な tar + zstd アーカイブに固め、primary key のオブジェクトへアップロードする。

### 正常系

| 出力先 | 内容 |
|--------|------|
| stdout | `true`（アップロードした場合）または `false`（内容が同一でアップロードを省略した場合）|
| stderr | （なし） |

**stdout の出力例**

```
true
```

```
false
```

`store` の stdout も `probe` と同じく `true` / `false` の 2 値のみ。書き込み先は常に primary key であり、`fallback_keys` へは書かない。対象件数・アーカイブサイズ・オブジェクトキーは `log::info!` / `log::debug!` へ出し、`RUST_LOG` で制御する。

### 異常系（stderr）

`key` サブコマンドと共通の異常系（config パース失敗、変数展開エラー等）に加え、以下が発生しうる。

| 状況 | stderr メッセージ例 |
|------|-------------------|
| `paths` が空配列 | `キャッシュ対象の解決に失敗しました` + `paths が空です。store するキャッシュ対象を 1 つ以上指定してください` |
| `paths` が 0 件マッチ | `キャッシュ対象の解決に失敗しました` + `paths のどのパターンにも 1 件もマッチしませんでした: ["target"]` |
| `paths` に絶対パスを指定 | `絶対パスのパターンは指定できません: /etc/passwd` |
| `paths` が基準ディレクトリ外を指す | `基準ディレクトリの外を指すパスは指定できません: ../outside` |
| アーカイブが 5 GiB を超えた | `アーカイブサイズが単一 PutObject の上限を超えています: ... bytes > 5368709120 bytes` + `cafce はマルチパートアップロードに未対応です。paths を絞り込んでください` |
| S3 head_object で 403 AccessDenied | `S3 HeadObject に失敗しました (bucket=my-bucket, key=my-project/cache-v1)` |
| S3 put_object で失敗 | `S3 PutObject に失敗しました (bucket=my-bucket, key=my-project/cache-v1)` + `BadDigest の場合はアップロード内容が転送中に壊れています` |

**チェックサム非対応サーバへのフォールバックについて**

`CAFCE_S3_CHECKSUM=auto`（既定）では、チェックサム非対応に起因すると判断できる失敗のときに stderr（`log::warn!`）へ次を出し、チェックサム無しで 1 回だけ再試行する。

```
S3 がフレキシブルチェックサムに対応していない可能性があります (status=Some(400), code=Some("InvalidRequest"))。チェックサム無しで再試行します
```

`BadDigest` は再試行せずそのままエラー終了する。サーバが機能を理解したうえで内容の不一致を検出した結果であり、本物の破損を意味するためである。

いずれの異常系でも exit code は非 0。

## restore サブコマンド

`cafce restore <config>` — primary key と `fallback_keys` を順に `get_object` で試し、最初にヒットしたアーカイブをカレントディレクトリへ展開する。

### 正常系

| 出力先 | 内容 |
|--------|------|
| stdout | `true`（展開した場合）または `false`（全 miss で何もしなかった場合）|
| stderr | （なし） |

**stdout の出力例**

```
true
```

```
false
```

全 miss でも exit code は 0。CI スクリプトからは `if [ "$(cafce restore cafce.toml)" = "true" ]` のように条件分岐に使える。

`restore` は `paths` を参照しない。アーカイブに何が入っているかはアーカイブ自身が持つ情報である。

### 異常系（stderr）

| 状況 | stderr メッセージ例 |
|------|-------------------|
| S3 get_object で 403 AccessDenied | `S3 GetObject に失敗しました (bucket=my-bucket, key=my-project/cache-v1)` + `403 の場合は s3:GetObject 権限を確認してください` |
| 転送中の破損（SDK のチェックサム検証） | `S3 からのボディ受信に失敗しました` + `チェックサム不一致の場合は転送中にオブジェクトが壊れています` |
| 未知のスキーマ版数 | `キャッシュの metadata を解釈できませんでした` + `このキャッシュは未知のスキーマ版数 2 で作られています（この cafce が解釈できるのは 1 までです）` + `cafce を更新してください` |
| 未知のアーカイブ形式 | `未知のアーカイブ形式です: zip（この cafce が対応しているのは tar+zstd のみです）` |
| 展開時のパス脱出（zip-slip） | `アーカイブ内のエントリが展開先ディレクトリの外を指しています: ../evil.txt` |
| 展開時の絶対パスエントリ | `アーカイブ内のエントリが絶対パスです: /etc/evil.txt` |
| 外部を指すシンボリックリンク | `シンボリックリンクのリンク先が展開先ディレクトリの外を指しています: ... -> ../../etc/passwd` |
| 内容ハッシュ不一致 | `キャッシュの内容ハッシュが一致しません (expected=..., actual=...)` + `作業ディレクトリに中途半端に復元されたファイルが残っている可能性があります` |
| `CAFCE_S3_CHECKSUM=required` でチェックサムが返らない | `CAFCE_S3_CHECKSUM=required ですが、S3 がオブジェクトのチェックサムを返しませんでした` + `サーバがフレキシブルチェックサムに対応していない可能性があります` |

**破損検出のタイミングについて**

転送経路の破損（ビット化け、途中切断）は、ボディを一時ファイルへ書き切る段階、つまり展開を始める前に検出できるため作業ディレクトリは汚れない。一方、内容ハッシュの不一致は展開後にしか判明せず、その時点でファイル木は既に書き換わっている。そのためこのメッセージだけは「中途半端に復元されたファイルが残っている可能性」を明示する。

**cafce の metadata が無いオブジェクトについて**

`aws s3 cp` などで手置きされた、cafce の metadata を持たないオブジェクトを引いた場合は、検証する材料が無いため展開のみ行い、`log::warn!` で次を出す。

```
cafce の metadata が無いため内容ハッシュ検証をスキップしました
```

いずれの異常系でも exit code は非 0。
