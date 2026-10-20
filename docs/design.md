# 詳細設計: メタデータカタログ・補完

`docs/architecture.md` の部品について、受け渡すデータの型と公開 API を決める。
探索 UI（Explorer）は Catalog との契約だけを決め、画面の詳細は別の回で設計する。

各節の **[縦1本]** は最初に通す縦の 1 本に含めるもの。それ以外は後から広げる。

## モジュール構成

```
src/
  snowflake.rs        接続設定、Session、QueryOutput、時刻の整形（既存を拡張）
  query.rs            QueryEngine（Session を使い回すよう変更）
  catalog/
    mod.rs            識別子、Scope、ノード、Children、Catalog
    source.rs         MetadataSource トレイトと ShowSource
    worker.rs         MetaWorker、WorkerEvent
    store.rs          CacheStore（SQLite）
    prefetch.rs       Prefetcher（何をいつ要求するかの方針）
  complete/
    token.rs          寛容なトークナイザ
    context.rs        CompletionContext の構築、文の副作用の検出
    rank.rs           候補の収集と順位付け
    keywords.rs       Snowflake のキーワード表
  app.rs              補完ポップアップと MetaSystem の組み込み
```

追加する依存: `rusqlite`（`bundled`）、`frizbee`、`dirs`、`serde_json`（現在は推移的依存）。

## Session と QueryOutput [縦1本]

```rust
pub struct Session { api: SnowflakeApi, runtime: tokio::runtime::Runtime }

pub enum QueryOutput {
    Arrow(Vec<RecordBatch>),
    /// SHOW / DESCRIBE など JSON rowset で返る結果。値は文字列か null。
    Rows { columns: Vec<String>, rows: Vec<Vec<Option<String>>> },
    /// 0 行。snowflake-api は 0 行のとき列の情報も返さない。
    Empty,
}

impl Session {
    pub fn connect(params: &SnowflakeParams) -> Result<Self>;
    pub fn exec(&self, sql: &str) -> Result<QueryOutput>;
}

impl QueryOutput {
    /// 結果グリッドで表示するための変換。Rows は全列 Utf8 の RecordBatch、Empty は 0 個のバッチ。
    pub fn into_batches(self) -> Result<Vec<RecordBatch>>;
}
```

- `SnowflakeParams` に `Clone` を付け、QueryEngine と MetaWorker がそれぞれ Session を 1 つ持つ。Session はスレッドの寿命の間使い回す
- JWT の更新は snowflake-api が行う
- `columns` は Snowflake が返す列名をそのまま持つ。`SHOW` の列名は小文字
- QueryEngine は Session の接続に失敗したら、最初のクエリの結果としてエラーを返す。次のクエリで接続し直す
- ユーザーが打った `SHOW` も `into_batches` で結果グリッドに出せるようになる

## 識別子と Scope [縦1本]

```rust
/// Snowflake に格納されている名前そのもの（クォートしない識別子なら大文字）。
pub struct Name(String);

impl Name {
    /// クォートなしの入力は大文字にし、クォート付きはそのまま。
    pub fn from_ident(part: &IdentPart) -> Self;
    /// エディタや SQL に埋める識別子の表記。クォートが要る名前（小文字・記号を含む等）は "…" で囲む。
    pub fn to_sql(&self) -> String;
    /// `FROM '<name>'` 用の文字列リテラル。`'` をエスケープする。
    pub fn to_sql_string(&self) -> String;
}

/// account と role は大文字に正規化して持つ。
pub struct CatalogKey { pub account: String, pub role: Name }

/// 子一覧を取得する単位（コンテナ）。
pub enum Scope {
    Account,                     // → databases
    Database(Name),              // → schemas
    Schema(Name, Name),          // → relations
    Relation(Name, Name, Name),  // → columns
}

pub struct CurrentScope { pub database: Option<Name>, pub schema: Option<Name> }
```

## ノードと Children [縦1本]

```rust
pub struct Database { pub name: Name, pub comment: Option<String> }
pub struct Schema   { pub name: Name, pub comment: Option<String> }
pub struct Relation {
    pub name: Name,
    pub kind: RelationKind,
    pub rows: Option<u64>,
    pub bytes: Option<u64>,
    pub comment: Option<String>,
}
/// `SHOW OBJECTS` の kind は TABLE / VIEW のみで、dynamic / Iceberg は is_dynamic / is_iceberg 列で区別する。
pub enum RelationKind { Table, View, Dynamic, Iceberg, Other(String) }
pub struct Column {
    pub name: Name,
    pub data_type: String,
    pub nullable: bool,
    pub comment: Option<String>,
}

/// Scope の種類と中身の種類は 1 対 1。apply は不一致の Patch を拒否する。
/// Vec の順序は Snowflake が返した順序をそのまま保つ。
pub enum Children {
    Databases(Vec<Database>),
    Schemas(Vec<Schema>),
    Relations(Vec<Relation>),
    Columns(Vec<Column>),
}
```

## Catalog [縦1本。seed / seeded は CacheStore と一緒に]

```rust
pub struct Catalog {
    key: CatalogKey,
    entries: HashMap<Scope, Entry>,
    next_ticket: u64,             // 取得の世代。Catalog 全体で単調に増やす
}

struct Entry {
    children: Option<Children>,   // 失敗時や再取得中も直前の中身を残す
    fetched_at: Option<SystemTime>,
    seeded: bool,                 // ディスクから読んだだけで、このセッションでは未検証
    in_flight: Option<u64>,       // 取得中の世代
    invalidated: bool,
    error: Option<(String, SystemTime)>,
}

pub enum Freshness { Missing, Loading, Fresh, Stale, Failed }

pub struct ScopeView<'a> {
    pub freshness: Freshness,
    pub children: Option<&'a Children>,
    pub error: Option<&'a str>,
}

impl Catalog {
    pub fn new(key: CatalogKey) -> Self;
    pub fn key(&self) -> &CatalogKey;
    pub fn view(&self, scope: &Scope, now: SystemTime) -> ScopeView<'_>;
    /// 取得を始めてよければ世代を返す。FetchRequest にはこの世代を載せる。
    pub fn begin_fetch(&mut self, scope: &Scope, now: SystemTime) -> Option<u64>;
    pub fn apply(&mut self, patch: CatalogPatch) -> ApplyOutcome;
    /// ディスクのキャッシュで埋める。children が無い Entry だけを埋め、世代と in_flight には触れない。
    pub fn seed(&mut self, scope: Scope, fetched_at: SystemTime, children: Children);
    pub fn invalidate(&mut self, scope: &Scope, recursive: bool);
}

pub enum ApplyOutcome { Applied, Discarded }
```

世代と取得中の契約:

- `begin_fetch`: `in_flight` が None で、失敗後のバックオフ（30 秒）を過ぎていれば、`next_ticket` から新しい世代を払い出して
  `in_flight` に入れ、それを返す。それ以外は None
  - 世代を Entry ごとではなく Catalog 全体で払い出すのは、削除して作り直した Entry が、削除前に出した要求の結果を受理しないようにするため
- `apply`: `patch.generation == in_flight` のときだけ受理し、`in_flight = None` にする。一致しない Patch は何も変えずに `Discarded` を返す
  - 成功: children を置き換え、`seeded` / `invalidated` / `error` を消す。消えた子の子孫スコープの Entry も削除する
  - 失敗: children を残し、`error` を立てる
- `invalidate`: `in_flight = None`、`invalidated = true` にする。取得中の結果は届いても世代が合わず捨てられる

`view` の鮮度の判定:

| 状態 | Freshness |
|---|---|
| Entry が無い / children も error も無い | `Missing`（取得中なら `Loading`） |
| children が無く error がある | `Failed` |
| children があり、取得中 | `Loading` |
| children があり、`seeded` / `invalidated` / TTL 超過 / error あり のいずれか | `Stale` |
| それ以外 | `Fresh` |

- `seeded` を常に `Stale` とするので、ディスク上のデータはセッションごとに 1 回は必ず再検証される。
  CacheStore が古い結果を書いても、再起動後にそれが `Fresh` として残ることはない
- TTL の既定値: Account と Database は 1 時間、Schema と Relation は 30 分。設定化は後回し
- Catalog は UI スレッドが単独で所有する。ロックは使わない

## FetchRequest と WorkerEvent [縦1本]

```rust
pub struct FetchRequest {
    pub scope: Scope,
    pub generation: u64,
    pub priority: Priority,
}

/// 小さいほど先に処理する。Visible（Explorer で見えているノード）は Explorer と一緒に Context と Crawl の間へ足す。
pub enum Priority { Demand = 0, Context = 1, Crawl = 2 }

pub enum WorkerEvent {
    /// 接続が確定した。実際の role と既定の位置。
    Connected { key: CatalogKey, current: CurrentScope },
    ConnectFailed(String),
    /// ディスクのキャッシュ。Connected より前に届くことがある。CacheStore と一緒に足す。
    Seed(Vec<(Scope, SystemTime, Children)>),
    Patch(CatalogPatch),
}

pub struct CatalogPatch {
    pub scope: Scope,
    pub generation: u64,
    pub fetched_at: SystemTime,
    pub result: Result<Children, String>,
}
```

- MetaWorker は (account, role) につき 1 つ。role が変われば作り直すので、Patch に key は載せない
- `Seed` は Prefetcher の `on_applied` の対象にしない（起動時に巡回の要求が大量に出るのを避ける）

## MetadataSource [縦1本は Account / Database / Schema]

```rust
pub trait MetadataSource {
    /// 実際の role と既定の位置。`SELECT CURRENT_ROLE(), CURRENT_DATABASE(), CURRENT_SCHEMA()`（warehouse 不要）。
    fn identify(&mut self) -> Result<(CatalogKey, CurrentScope)>;
    fn fetch(&mut self, scope: &Scope) -> Result<Children>;
}

pub struct ShowSource { session: Session, account: String }
```

| Scope | SQL | 備考 |
|---|---|---|
| Account | `SHOW DATABASES` | DB の数は少ないので TERSE にせずコメントも取る |
| Database | `SHOW SCHEMAS IN DATABASE <db>` | |
| Schema | `SHOW OBJECTS IN SCHEMA <db>.<schema> LIMIT 10000 [FROM '<last>']` | kind / rows / bytes / comment が返る。ページングは下記 |
| Relation | `SHOW COLUMNS IN <fqn>` | `TABLE` / `VIEW` を省略でき、kind を知らなくても引ける |

- SQL に埋める識別子は `Name::to_sql` を、`FROM` の文字列は `Name::to_sql_string` を通す
- `Rows` は列名で引いて変換する。必須の列が無ければエラー。`Empty` は空の Children として扱う
- `SHOW COLUMNS` の `data_type` 列は JSON 文字列。ここから型名と nullable を取り出す
- MetadataSource をトレイトにするのは、MetaWorker のテストで偽の実装を差し込むため

**Schema のページング:** snowflake-api は JSON 応答が分割されると先頭の chunk しか読まず、欠けを呼び出し側から検出できない
（数百行で切れる）。そこで「1 ページが 10,000 件なら続きを取る」ではなく、**新しい名前が 1 件も増えなくなるまで**
`FROM '<最後の名前>'` で取り続ける。`FROM` は指定した名前の次の行から返すので、欠けても次のページで回収できる。
念のため名前で重複を除く。

**既知の制約:**

- `SHOW COLUMNS` は `LIMIT … FROM` を持たないので、分割で欠けた分を回収できない。極端にカラムの多いテーブルではカラムが欠けうる
- snowflake-api は、実行が長引いたときの非同期応答を解釈できずエラーになる。`SHOW COLUMNS IN ACCOUNT` のような重い文は投げない
- `SHOW … ->> SELECT …`（パイプ）は JSON でも Arrow でも行を返さないため、chunk の欠けの回避には使えない

## MetaWorker [縦1本]

```rust
pub struct MetaWorker { cmd_tx: Sender<FetchRequest>, event_rx: Receiver<WorkerEvent>, _handle: JoinHandle<()> }

impl MetaWorker {
    /// スレッド内で CacheStore を開いて Seed を送り、そのあと接続して Connected を送る。
    pub fn spawn(params: SnowflakeParams) -> Self;
    /// テスト用。任意の MetadataSource で動かす。key は source の identify で決まる。
    pub fn spawn_with(source: Box<dyn MetadataSource + Send>) -> Self;
    pub fn request(&self, req: FetchRequest);
    pub fn poll(&self) -> Vec<WorkerEvent>;   // ノンブロッキングで溜まった分をすべて返す
}
```

- キューは `Scope` をキーにした表。同じスコープの要求が重なったら、優先度は高い方、世代は新しい方を残す
- 1 件取得するごとに受信チャネルを掃き出してから、次に処理する要求を選ぶ。これで Demand が Crawl に割り込める
- Crawl を取り出したら 200ms 待ち、その間に届いた要求を掃き出してから選び直す
- 取得に成功したら、Event を送る前に CacheStore へ書く
- 起動時のキャッシュは仮の key で開く。仮の key は `params.role`、無ければ前回確定した既定の role（account ごとにキャッシュディレクトリへ記録）を使う。
  接続後の key と食い違えば、確定した key でストアを開き直し、Seed を送り直す

## CacheStore

```rust
pub struct CacheStore { conn: rusqlite::Connection }

impl CacheStore {
    /// `<cache_dir>/azuti/catalog/<account>__<role>.sqlite`。ファイル名に使えない文字は置き換える。
    /// スキーマの版が違うファイルは作り直す。
    pub fn open(key: &CatalogKey) -> Result<Self>;
    pub fn load_all(&self) -> Result<Vec<(Scope, SystemTime, Children)>>;
    /// 1 トランザクションでスコープの子を置き換える。消えた子の子孫（行と scopes の行）も消す。
    pub fn replace(&mut self, scope: &Scope, fetched_at: SystemTime, children: &Children) -> Result<()>;
}
```

```sql
CREATE TABLE meta      (version INTEGER NOT NULL);
CREATE TABLE scopes    (scope TEXT PRIMARY KEY, fetched_at INTEGER NOT NULL);  -- scope は名前の JSON 配列
CREATE TABLE databases (name TEXT PRIMARY KEY, comment TEXT);
CREATE TABLE schemas   (db TEXT, name TEXT, comment TEXT, PRIMARY KEY (db, name));
CREATE TABLE relations (db TEXT, schema TEXT, name TEXT, kind TEXT, rows INTEGER, bytes INTEGER,
                        comment TEXT, PRIMARY KEY (db, schema, name));
CREATE TABLE columns   (db TEXT, schema TEXT, relation TEXT, seq INTEGER, name TEXT,
                        data_type TEXT, nullable INTEGER, comment TEXT,
                        PRIMARY KEY (db, schema, relation, name));
```

`seq` は Children の Vec の順序を保存するためだけの列。

## Prefetcher

```rust
pub struct Prefetcher {
    pub crawl: CrawlRange,               // 既定 CurrentDatabase
    pub column_prefetch_max: usize,      // このテーブル数以下のスキーマはカラムまで巡回する。既定 200
}
pub enum CrawlRange { CurrentDatabase, Account }

impl Prefetcher {
    /// Connected を受けたときに呼ぶ。
    pub fn on_connected(&self, current: &CurrentScope) -> Vec<(Scope, Priority)>;
    pub fn on_applied(&self, scope: &Scope, catalog: &Catalog, current: &CurrentScope) -> Vec<(Scope, Priority)>;
    /// 入力が止まったとき。文中のテーブルのカラムを Context で要求する。
    pub fn on_editor_idle(&self, ctx: &CompletionContext, current: &CurrentScope) -> Vec<(Scope, Priority)>;
}
```

- Prefetcher は方針だけを持ち、状態を持たない。返したスコープは App が `begin_fetch` を通して要求する
- `on_connected` は Account、current DB の Database、current schema の Schema を Context で要求する（いずれも `SHOW` で warehouse 不要）

## トークナイザと CompletionContext [縦1本は Relation / Database / Schema]

```rust
pub enum TokenKind { Word, QuotedIdent, Str, Number, Dot, Comma, LParen, RParen, Semicolon, Op, Comment }
pub struct Token { pub kind: TokenKind, pub span: Range<usize>, pub terminated: bool }

pub fn tokenize(sql: &str) -> Vec<Token>;
```

- 閉じていない引用符・コメントは末尾まで 1 トークンにして `terminated: false` にする。エラーにはしない
- 位置はすべてエディタの全文に対するバイトオフセット。tui-textarea の `cursor()` は (行, 文字) なので App で変換する

```rust
pub struct CompletionContext {
    pub expect: Expect,
    pub qualifier: Vec<IdentPart>,      // 末尾の `.` より前の部分。`db.sch.` なら [db, sch]
    pub prefix: IdentPart,              // 入力中の単語の、カーソルより前の部分（照合に使う）
    pub replace: Range<usize>,          // 確定時に置き換える範囲（カーソル下の単語全体のバイト範囲）
    pub tables: Vec<TableRef>,          // 同じ文の FROM / JOIN にあるテーブル
    pub ctes: Vec<IdentPart>,
}
pub struct IdentPart { pub text: String, pub quoted: bool }
pub struct TableRef { pub path: Vec<IdentPart>, pub alias: Option<IdentPart> }

pub enum Expect {
    Relation,   // FROM / JOIN / INTO / UPDATE / TABLE の後。DESCRIBE / DESC は文頭のときだけ
    Column,     // SELECT の列リスト / WHERE / ON / GROUP BY / ORDER BY / HAVING
    Database,   // USE DATABASE の後と、修飾なしの USE の後（`USE <db>[.<schema>]`）
    Schema,     // USE SCHEMA の後
    Keyword,    // 上記以外
}

/// カーソルが文字列リテラルやコメントの中なら None。
pub fn analyze(sql: &str, cursor: usize) -> Option<CompletionContext>;
```

- [縦1本] では tables と ctes を持たない（Column の補完と一緒に足す）
- カーソルを含む文だけを見る（`;` で区切る）。コメントは読み飛ばす
- Expect は、カーソル直前の識別子チェーンの前にある最後のキーワードで決める
- tables と ctes は同じ文の全体から拾う。カーソルより後ろの `FROM` も含める（`SELECT | FROM t` でカラムを出すため）

文の副作用（縦1本の対象外）:

```rust
pub enum StatementEffect {
    Invalidate { scope: Scope, recursive: bool },
    Use(CurrentScope),   // USE DATABASE / USE SCHEMA / USE <db>.<schema>
    UseRole(Name),       // USE ROLE / USE SECONDARY ROLES
}
pub fn statement_effects(sql: &str, current: &CurrentScope) -> Vec<StatementEffect>;
```

| 文 | 無効化するスコープ |
|---|---|
| `CREATE` / `DROP` / `UNDROP` | 対象スコープ（recursive）と、その親 |
| `ALTER … ADD / DROP / RENAME COLUMN` 等 | 対象の Relation スコープ |
| `ALTER … RENAME TO <新しい名前>` | 元の対象（recursive）と元の親、移動先の親 |
| 対象の名前が解決できない DDL | current schema の Schema スコープ |

`UseRole` を受けたら、App は MetaSystem を丸ごと作り直す（新しい worker・Catalog・ストア）。

## 候補の収集と順位付け [縦1本は Relation / Database / Schema]

```rust
pub struct Candidate {
    pub label: String,       // 表示名
    pub insert: String,      // Name::to_sql
    pub kind: CandidateKind,
    pub detail: Option<String>,   // 型名、行数、コメント
    score: i64,
}
/// 宣言順が同点時の並び順（テーブルを DB より先に出す）。
pub enum CandidateKind { Table, View, Schema, Database, Column, Alias, Cte, Keyword }

pub struct Completion {
    pub candidates: Vec<Candidate>,   // 上位 50 件
    pub replace: Range<usize>,
    pub needs: Vec<Scope>,            // 候補を出すのに要るが、Fresh でないスコープ
    pub error: Option<String>,        // 一度も取れていないスコープの失敗理由
}

pub fn complete(ctx: &CompletionContext, catalog: &Catalog, current: &CurrentScope,
                now: SystemTime) -> Completion;
```

Expect::Relation での修飾子の解決:

| qualifier | 候補 |
|---|---|
| なし | current schema の relations、current DB の schemas、databases |
| 1 つ（`x.`） | x が DB なら x の schemas、current DB のスキーマなら x の relations |
| 2 つ（`x.y.`） | x.y の relations |

Expect::Database / Expect::Schema:

| 文脈 | qualifier なし | `x.` |
|---|---|---|
| `USE DATABASE` / `USE` | databases | x の schemas |
| `USE SCHEMA` | current DB の schemas、databases | x の schemas |

Relation と `USE SCHEMA` の文脈で、DB やスキーマ（まだ下の階層が続くもの）を挿入するときは、末尾に `.` を付ける。
確定したらポップアップを開き直し、そのまま次の階層を選べるようにする。直後にすでに `.` があるとき（`db.schema` の `db` を
置き換えるとき）は `.` を付けない。

Expect::Column では、qualifier が alias・テーブル名・CTE 名のどれかに一致すれば、そのテーブルの columns を候補にする。
qualifier が無ければ tables の全カラムを候補にする。

`needs` の規則:

- 親の一覧が Fresh で、そこに x が無いと分かっている場合は、x の子スコープを `needs` に入れない（存在しない名前で `SHOW` を繰り返さないため）
- 親が未取得なら、子ではなく親を `needs` に入れる
- `Failed` のスコープは入れず、`error` に理由を入れる。再試行は TTL ではなく、バックオフが明けたあとの次の入力で行う

スコア: `frizbee` のファジースコアに、先頭一致・文中に既出・current schema の加点を足す。マッチしない候補は除く。

## App への組み込み [縦1本]

```rust
struct MetaSystem {
    catalog: Option<Catalog>,     // Connected を受けるまで None。Seed は仮の key の Catalog に入れる
    worker: MetaWorker,
    prefetcher: Prefetcher,
    current: CurrentScope,
    status: MetaStatus,           // Connecting / Ready / Failed(理由)。接続中も補完を「loading…」で待たせる
}

struct CompletionPopup { completion: Completion, selected: usize, moved: bool }
```

- メインループに `poll_meta` を足す
  - `Connected`: Catalog を確定し、`on_connected` の要求を出す。接続前に開いたポップアップも再計算する
  - `Seed`: `Catalog::seed`
  - `Patch`: `apply` し、`on_applied` の要求を出す。ポップアップを開いていれば `complete` をやり直す
- `needs` は Demand として、`begin_fetch` が世代を返したものだけ要求する
- ユーザーのクエリが成功したら `statement_effects` を当てる（縦1本の対象外）
- クエリの実行時と、フォーカスが結果へ移るときはポップアップを閉じる
- 確定: `replace` のバイト範囲を (行, 文字) に戻し、`move_cursor(CursorMove::Jump(row, col))`（引数は u16）でその先頭へ移動する。
  そこから `delete_str(文字数)` で消して `insert_str(insert)` で入れる。選択中だと Jump が選択を伸ばし、
  `delete_str` が選択を消すので、先に `cancel_selection()` する
- ポップアップの位置は描画後の `rendered_cursor_position()` を使う。カーソルの下に出し、はみ出すときは上に出す

| キー | ポップアップが閉じているとき | 開いているとき |
|---|---|---|
| `Ctrl+Space` / `Ctrl+N` | 開く（macOS は `Ctrl+Space` が入力ソースの切り替えに取られることがあるため両方） | — |
| `.` / 識別子の入力（Relation・Column の文脈） | 自動で開く | 再計算する |
| `↑` `↓` / `Ctrl+P` `Ctrl+N` | 既存の動作 | 選択を移す |
| `Tab` | フォーカス切替 | 確定する（候補が 0 件なら何もしない） |
| `Enter` | 改行 | 選択を移した後なら確定、移していなければ閉じて改行 |
| `Esc` | フォーカス切替 | 閉じる |

候補が 0 件のときは、`needs` があるか接続中なら「loading…」、`error` があれば「error: …」の 1 行を出す。

## Explorer との契約

- Explorer は `Catalog::view` だけで描画する。子が要るノードは `Priority::Visible`（開いたノードは `Demand`）で要求する
- 仮想パスのカレントは `CurrentScope` そのもの。`:cd` は Explorer の位置を動かし、`USE` の SQL もユーザーのセッションへ送る
- ファジージャンプとカラム逆引きも Catalog 上の名前を `frizbee` で引く。対象は巡回済みの範囲に限られる

## 対象外（後回し）

- `SHOW … LIKE` によるサーバ検索。結果がスコープの一部だけなので、スコープ単位の置き換えモデルに乗らない。別の要求種別として設計する
- 使用頻度による加点（確定した候補の記録と永続化）
- L3（プレビュー・プロファイル）の別ストア
- `INFORMATION_SCHEMA` による差分取得

## テスト

| 対象 | 方法 |
|---|---|
| tokenize / analyze / statement_effects | 書きかけの SQL を入力にした単体テスト |
| Catalog の世代と鮮度 | 世代の合わない Patch が捨てられ in_flight が残らないこと、invalidate 後に Stale になること、seed が Stale であること、バックオフ |
| MetaWorker | 偽の MetadataSource で、優先度順の処理と重複の統合を確かめる |
| ShowSource の変換・ページング | `QueryOutput::Rows` / `Empty` の固定データからノードへ変換する。欠けのあるページが続いても全件そろうこと |
| CacheStore | 一時ファイルで replace と load_all を往復する。子孫の削除 |
| ポップアップ | `TestBackend` で描画を文字化する |

## ローカル SQL

`docs/architecture.md` の「ローカル SQL」の型と API。最初の縦の 1 本は、この節全体（列の補完は対象外）。

### モジュール

```
src/
  local.rs              LocalEngine、table_name_for
  catalog/source.rs     LocalSource を追加
  catalog/mod.rs        Folding を追加
  app.rs                実行先、ドリルダウン、結果の命名
```

追加する依存: `datafusion = "52"`（default features を切り、sql / recursive_protection / 日付・文字列・正規表現・入れ子の関数だけを有効にする）。
ファイルは DataFusion に読ませないので parquet feature は要らない。recursive_protection は深い SQL でスタックが溢れないように残す。
53 以降は arrow 58 なので、arrow を上げるまで 52 に固定する。

### 識別子の正規化

```rust
/// クォートしない識別子をどちらに揃えるか。Snowflake は大文字、DataFusion は小文字。
pub enum Folding { Upper, Lower }

impl Name {
    pub fn from_ident_with(part: &IdentPart, folding: Folding) -> Self;
    /// クォートせずに書ける名前（folding に揃った文字と数字・`_`・`$`、予約語でない）はそのまま、それ以外は "…"。
    pub fn to_sql_with(&self, folding: Folding) -> String;
}

impl Catalog {
    pub fn with_folding(key: CatalogKey, folding: Folding) -> Self;   // テストでは new(key) が Upper 版
    pub fn folding(&self) -> Folding;
}
```

- 予約語（クォートが要る語）は Upper では Snowflake の予約語、Lower では `TABLE` と `LATERAL` だけ。DataFusion のパーサはテーブル名の位置でほぼすべてのキーワードを受け付けるため

- 既存の `from_ident` / `to_sql` は `Folding::Upper` 版として残す（ShowSource が使う）
- 補完の候補の挿入文字列と、修飾子の解決は `catalog.folding()` を使う
- `WorkerEvent::Connected` に `folding` を載せ、MetaSystem が Catalog を作るときに使う。`MetadataSource::folding()` で返す

### LocalEngine

```rust
pub struct LocalEngine {
    cmd_tx: Sender<LocalCommand>,
    event_rx: Receiver<LocalEvent>,
    ctx: SessionContext,          // LocalSource に渡すための複製（内部は Arc）
    _handle: JoinHandle<()>,
}

enum LocalCommand {
    Register { name: String, batch: RecordBatch, as_last: bool },
    Query(String),
}

pub enum LocalEvent {
    /// 登録が済んだ。カタログを無効化してよい。
    Registered,
    RegisterFailed { name: String, error: String },
    Result(QueryOutcome),
}

impl LocalEngine {
    pub fn spawn() -> Self;
    /// 表示用に読み込んだ 1 バッチ（DataTable::batch）を登録する。`last` にはしない。
    pub fn register_file(&self, name: &str, batch: RecordBatch);
    /// `name` と `last` の両方に登録する。既にあれば置き換える。
    pub fn register_result(&self, name: &str, batch: RecordBatch);
    pub fn submit(&self, sql: String);
    pub fn poll(&self) -> Vec<LocalEvent>;
    pub fn context(&self) -> SessionContext;
}

/// `Sales-2024.csv` → `sales_2024`。先頭が数字なら `_`、英数字が無ければ `file`、`last` / `result_N` なら `file_` を前に付ける。
pub fn table_name_for(path: &Path) -> String;
```

- SessionContext は `datafusion.sql_parser.dialect = snowflake` で作る。スレッド内で tokio のランタイムを作り、`block_on` で回す（DataFusion の実行計画は tokio のタスクを使う）
- クエリは `sql_with_options` で DDL・DML・SET を拒否する
- `MemTable` で登録する。表示用の DataTable は 1 バッチに結合済みなので、その Arc を渡して共有し、コピーしない
- 大文字だけの列名は、スキーマだけ差し替えて小文字にする（同じ小文字の列が既にあれば変えない）。
  同じ名前が重なった列（JOIN の `SELECT *` など）は、2 つ目以降に `_2`, `_3`, … を付ける。重複があるとそのテーブルへのクエリがすべて失敗するため
- 登録に失敗した名前が今の表示（`view_name`）なら、ドリルダウンできないよう `view_name` を消す
- 置き換えは `deregister_table` → `register_table`

### LocalSource

```rust
pub struct LocalSource { ctx: SessionContext }

impl MetadataSource for LocalSource {
    /// key は (account: "local", role: "local")、current は既定のカタログ・スキーマ（datafusion.public）、folding は Lower。
    fn identify(&mut self) -> Result<(CatalogKey, CurrentScope)>;
    /// Account → カタログ名、Database → スキーマ名、Schema → テーブル名、Relation → 列（TableProvider::schema）。
    fn fetch(&mut self, scope: &Scope) -> Result<Children>;
}
```

- `MetaWorker::spawn_with(source)` を `#[cfg(test)]` から外し、ローカル用 MetaSystem の生成に使う
- `LocalEvent::Registered` を受けたら、App は `Scope::Schema(datafusion, public)` を無効化する（`MetaSystem::invalidate`）。
  invalidate はその場で Context の取得も要求するので、開いているポップアップに新しいテーブルがすぐ出る
- Relation（列）は `SchemaProvider::table` が async なので、LocalSource が持つ current_thread のランタイムで `block_on` する

### App

```rust
enum Target { Snowflake, Local }

struct App {
    // 既存のフィールドに加えて
    local: Option<LocalEngine>,
    local_meta: Option<MetaSystem>,
    target: Target,
    /// 今の表示の名前（ファイル名由来の名前か result_N）。ドリルダウンの初期文に使う。
    view_name: Option<String>,
    results: usize,
    /// 表示していない側の実行先のエディタの文。Ctrl+T で入れ替える。
    other_sql: Vec<String>,
}
```

- LocalEngine とローカル用 MetaSystem は、ファイルを開いたとき・結果を表示したときに無ければ作る
- 結果を表示したら（Snowflake でもローカルでも）、`result_{next_result}` を振って `register_result` し、`view_name` にする
- ファイルを開いたら `register_file(table_name_for(path))` し、`view_name` をその名前にする
- 補完はエディタの実行先に応じて、Snowflake 用かローカル用の MetaSystem を使う

| キー | 場所 | 動作 |
|---|---|---|
| `d` | 結果の表 | ドリルダウン。実行先をローカルにして `SELECT * FROM <view_name>` を入れ、エディタにフォーカスする。Snowflake 側の文は `other_sql` に退避する |
| `Ctrl+T` | エディタ | 実行先を切り替え、エディタの文を `other_sql` と入れ替える（Snowflake に接続しているときだけ） |
| `Ctrl+R` / `Ctrl+Enter` | エディタ | 実行先で実行する |

エディタの枠のタイトルに実行先を出す（` SQL · local ` / ` SQL · Snowflake `）。

### テスト

| 対象 | 方法 |
|---|---|
| table_name_for | ファイル名の変換（大文字、記号、先頭の数字） |
| LocalEngine | 登録してクエリ、`last` と `result_N` の両方で引ける、置き換え、大文字・重複の列名、読み取り専用 |
| LocalSource | 登録したテーブルが Schema の fetch に出る |
| Folding | 小文字の名前が Lower ではクォートされず、Upper ではクォートされる |
| App | ドリルダウンでエディタが開き、初期文と実行先が入る（`TestBackend`） |

