//! GG's preset library rebuilt from GG's own database migrations, for machines where GG is not
//! installed (Platinum's setup keeps the migrations it takes from GG's official installer).
//!
//! GG creates its Sonar database by replaying, in order, one SQL file per Sonar version
//! (`db-migrations\sonar-<version>\<timestamp>_squash.sql`, applied by goose). The presets arrive
//! through those files (`analysis/sonar/presets.md`, "migrations"): `INSERT INTO configs …`,
//! `UPDATE configs SET game_ids|name|default_data|data = … WHERE id = …`, a few renames, one
//! delete, the seeded selection of each channel (`selected_config`), and twelve "refresh the
//! factory version, and the preset too unless the user changed it" statements (`WITH const AS
//! (SELECT '<id>' AS id, '<json>' AS default_data) … UPDATE configs …`).
//!
//! This is not an SQL engine: it understands exactly these statement forms, on the two tables that
//! hold presets, and skips everything else (other tables, schema changes, which only add columns
//! with defaults, and table rebuilds that keep the rows). A statement on `configs` or
//! `selected_config` it cannot follow is reported in `Replay::ignored` rather than guessed.
//!
//! Files are applied in goose's order: by the version number that starts their file name.

use crate::json::Json;
use std::collections::BTreeMap;
use std::path::Path;

/// A value of a row, as SQLite stores it.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
}

impl Value {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(n) => Some(*n),
            Value::Real(x) if x.fract() == 0.0 => Some(*x as i64),
            Value::Text(text) => text.trim().parse().ok(),
            Value::Null => None,
            Value::Real(_) => None,
        }
    }

    /// SQL `=`: numbers by value, texts by content; NULL equals nothing.
    fn equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, _) | (_, Value::Null) => false,
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::Text(_), _) | (_, Value::Text(_)) => false,
            (a, b) => a.number() == b.number(),
        }
    }

    fn number(&self) -> f64 {
        match self {
            Value::Integer(n) => *n as f64,
            Value::Real(x) => *x,
            _ => f64::NAN,
        }
    }
}

/// A row: column names in lower case, and their values. A column a row never got (one added
/// later by `ALTER TABLE … ADD … DEFAULT`) is simply missing, and the reader uses its default.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row(pub BTreeMap<String, Value>);

impl Row {
    pub fn get(&self, column: &str) -> Option<&Value> {
        self.0.get(&column.to_ascii_lowercase())
    }

    pub fn text(&self, column: &str) -> Option<&str> {
        self.get(column).and_then(Value::as_text)
    }

    pub fn integer(&self, column: &str) -> Option<i64> {
        self.get(column).and_then(Value::as_integer)
    }

    fn id(&self) -> Option<&str> {
        self.text("id")
    }
}

/// What the migrations leave in the two preset tables.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Replay {
    /// Table `configs`, in insertion order.
    pub configs: Vec<Row>,
    /// Table `selected_config`: channel number (`vad`) and preset id.
    pub selected: Vec<(i64, String)>,
    /// Statements on `configs` or `selected_config` that could not be followed, with their file.
    pub ignored: Vec<String>,
    /// How many files were applied.
    pub files: usize,
    /// The newest Sonar version among the files' folders (`sonar-1.106.0`): the version GG would
    /// run with these migrations.
    pub sonar_version: Option<(u64, u64, u64)>,
}

/// The Sonar version of a migration file's folder (`sonar-1.106.0\…`, any case).
pub fn folder_version(name: &str) -> Option<(u64, u64, u64)> {
    name.split(['\\', '/']).find_map(|part| {
        let lower = part.to_ascii_lowercase();
        let mut numbers = lower.strip_prefix("sonar-")?.split('.').map(|n| n.parse::<u64>().ok());
        Some((numbers.next()??, numbers.next()??, numbers.next().flatten().unwrap_or(0)))
    })
}

/// The migration files under `folder` (any depth), as (file name, text), in goose's order. Files
/// whose name does not start with a version number are left out, as goose does.
pub fn read_folder(folder: &Path) -> Result<Vec<(String, String)>, String> {
    let mut found = Vec::new();
    collect(folder, &mut found).map_err(|e| format!("cannot read {}: {e}", folder.display()))?;
    if found.is_empty() {
        return Err(format!("no migration file in {}", folder.display()));
    }
    let mut files = Vec::new();
    for path in found {
        let text = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let name = path.strip_prefix(folder).unwrap_or(&path).display().to_string();
        files.push((name, String::from_utf8_lossy(&text).into_owned()));
    }
    sort_files(&mut files);
    Ok(files)
}

fn collect(folder: &Path, found: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(folder)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, found)?;
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql")) && version_of(&path.display().to_string()).is_some() {
            found.push(path);
        }
    }
    Ok(())
}

/// The version number a migration file name starts with (`20261028162350_squash.sql`).
fn version_of(name: &str) -> Option<u64> {
    let file = name.rsplit(['\\', '/']).next()?;
    let digits: String = file.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// goose's order: by version number, then by name.
pub fn sort_files(files: &mut [(String, String)]) {
    files.sort_by(|a, b| version_of(&a.0).cmp(&version_of(&b.0)).then_with(|| a.0.cmp(&b.0)));
}

/// The tables while replaying, with the column defaults of `configs`: SQLite gives a row the
/// default of every column it was not given, including columns added after it was inserted.
#[derive(Default)]
struct Tables {
    replay: Replay,
    defaults: BTreeMap<String, Value>,
    /// Defaults of `temp_configs`, which becomes `configs` when GG rebuilds the table.
    rebuilt: BTreeMap<String, Value>,
    created: bool,
}

/// Gives a row the defaults of the columns it lacks.
fn complete(defaults: &BTreeMap<String, Value>, row: &mut Row) {
    for (column, value) in defaults {
        row.0.entry(column.clone()).or_insert_with(|| value.clone());
    }
}

/// Replays the files, in the order given.
pub fn replay(files: &[(String, String)]) -> Replay {
    let mut tables = Tables::default();
    for (name, text) in files {
        for statement in statements(up_section(text)) {
            let tokens = tokenize(&statement);
            if let Err(why) = apply(&mut tables, &tokens) {
                tables.replay.ignored.push(format!("{name}: {why}: {}", abbreviate(&statement)));
            }
        }
        tables.replay.files += 1;
        tables.replay.sonar_version = tables.replay.sonar_version.max(folder_version(name));
    }
    tables.replay
}

fn abbreviate(statement: &str) -> String {
    let flat: String = statement.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 120 { format!("{}…", flat.chars().take(120).collect::<String>()) } else { flat }
}

/// The part of a goose file that builds the database (between `-- +goose Up` and
/// `-- +goose Down`); the whole text when it has no such marks.
pub fn up_section(text: &str) -> &str {
    let lower = text.to_ascii_lowercase();
    let start = lower.find("+goose up").map(|at| at + "+goose up".len()).unwrap_or(0);
    let end = lower[start..].find("+goose down").map(|at| start + at).unwrap_or(text.len());
    // Back up over the `--` that starts the Down mark's line.
    let end = text[..end].rfind('\n').filter(|nl| *nl >= start).unwrap_or(end);
    &text[start..end]
}

/// The statements of an SQL text, split at the semicolons that are not inside quotes or comments.
/// Comments are dropped.
pub fn statements(sql: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                current.push(c);
                while let Some(d) = chars.next() {
                    current.push(d);
                    if d == c {
                        if chars.peek() == Some(&c) {
                            current.push(chars.next().unwrap_or(c));
                        } else {
                            break;
                        }
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        current.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for d in chars.by_ref() {
                    if last == '*' && d == '/' {
                        break;
                    }
                    last = d;
                }
                current.push(' ');
            }
            ';' => {
                if !current.trim().is_empty() {
                    found.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        found.push(current.trim().to_string());
    }
    found
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    /// An identifier or a keyword, as written.
    Word(String),
    /// A '…' string.
    Text(String),
    /// A "…" identifier, which SQLite takes for a string when no column has that name.
    Quoted(String),
    Number(String),
    /// Punctuation and operators: ( ) , . = == != <> - + * || …
    Symbol(String),
}

fn tokenize(statement: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = statement.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' || c == '"' {
            let mut text = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == c {
                    if chars.get(i + 1) == Some(&c) {
                        text.push(c);
                        i += 2;
                        continue;
                    }
                    break;
                }
                text.push(chars[i]);
                i += 1;
            }
            i += 1;
            tokens.push(if c == '\'' { Token::Text(text) } else { Token::Quoted(text) });
        } else if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            tokens.push(Token::Number(chars[start..i].iter().collect()));
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(Token::Word(chars[start..i].iter().collect()));
        } else if c == '`' || c == '[' {
            let close = if c == '`' { '`' } else { ']' };
            let start = i + 1;
            i = start;
            while i < chars.len() && chars[i] != close {
                i += 1;
            }
            tokens.push(Token::Word(chars[start..i.min(chars.len())].iter().collect()));
            i += 1;
        } else {
            let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
            if ["==", "!=", "<>", "<=", ">=", "||"].contains(&two.as_str()) {
                tokens.push(Token::Symbol(two));
                i += 2;
            } else {
                tokens.push(Token::Symbol(c.to_string()));
                i += 1;
            }
        }
    }
    tokens
}

/// Reads tokens one at a time.
struct Cursor<'a> {
    tokens: &'a [Token],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<&'a Token> {
        let token = self.tokens.get(self.at);
        self.at += 1;
        token
    }

    fn done(&self) -> bool {
        self.at >= self.tokens.len()
    }

    /// Consumes the keyword if it comes next.
    fn keyword(&mut self, word: &str) -> bool {
        match self.peek() {
            Some(Token::Word(w)) if w.eq_ignore_ascii_case(word) => {
                self.at += 1;
                true
            }
            _ => false,
        }
    }

    fn expect_keyword(&mut self, word: &str) -> Result<(), String> {
        if self.keyword(word) { Ok(()) } else { Err(format!("{word} expected")) }
    }

    fn symbol(&mut self, symbol: &str) -> bool {
        match self.peek() {
            Some(Token::Symbol(s)) if s == symbol => {
                self.at += 1;
                true
            }
            _ => false,
        }
    }

    fn expect_symbol(&mut self, symbol: &str) -> Result<(), String> {
        if self.symbol(symbol) { Ok(()) } else { Err(format!("'{symbol}' expected")) }
    }

    /// A table or column name (bare, "quoted", `table.column` reduced to `column`), lower case.
    fn name(&mut self) -> Result<String, String> {
        let first = match self.next() {
            Some(Token::Word(w)) | Some(Token::Quoted(w)) => w.to_ascii_lowercase(),
            _ => return Err("a name expected".into()),
        };
        if self.symbol(".") {
            return self.name();
        }
        Ok(first)
    }
}

/// An operand of an expression: a column of the row, or a constant.
#[derive(Clone, Debug, PartialEq)]
enum Operand {
    Column(String),
    /// A "…" name: the column when the row has it, else the text (SQLite's rule).
    Either(String),
    Constant(Value),
}

impl Operand {
    fn value(&self, row: &Row) -> Value {
        match self {
            Operand::Column(name) => row.get(name).cloned().unwrap_or(Value::Null),
            Operand::Either(name) => row.get(name).cloned().unwrap_or_else(|| Value::Text(name.clone())),
            Operand::Constant(value) => value.clone(),
        }
    }
}

/// A WHERE clause.
#[derive(Clone, Debug, PartialEq)]
enum Condition {
    All,
    Equal(Operand, Operand),
    NotEqual(Operand, Operand),
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
    Not(Box<Condition>),
}

impl Condition {
    fn holds(&self, row: &Row) -> bool {
        match self {
            Condition::All => true,
            Condition::Equal(a, b) => a.value(row).equals(&b.value(row)),
            Condition::NotEqual(a, b) => {
                let (a, b) = (a.value(row), b.value(row));
                a != Value::Null && b != Value::Null && !a.equals(&b)
            }
            Condition::And(a, b) => a.holds(row) && b.holds(row),
            Condition::Or(a, b) => a.holds(row) || b.holds(row),
            Condition::Not(a) => !a.holds(row),
        }
    }
}

/// One operand: a literal, a column, or a date function whose result does not matter here.
fn operand(cursor: &mut Cursor) -> Result<Operand, String> {
    let token = cursor.next().ok_or("value expected")?;
    let operand = match token {
        Token::Text(text) => Operand::Constant(Value::Text(text.clone())),
        Token::Number(number) => Operand::Constant(number_value(number)?),
        Token::Symbol(s) if s == "-" => match cursor.next() {
            Some(Token::Number(number)) => Operand::Constant(match number_value(number)? {
                Value::Integer(n) => Value::Integer(-n),
                Value::Real(x) => Value::Real(-x),
                other => other,
            }),
            _ => return Err("number expected after '-'".into()),
        },
        Token::Quoted(name) => {
            if cursor.symbol(".") {
                Operand::Column(cursor.name()?)
            } else {
                Operand::Either(name.clone())
            }
        }
        Token::Word(word) => {
            let lower = word.to_ascii_lowercase();
            match lower.as_str() {
                "null" => Operand::Constant(Value::Null),
                "true" => Operand::Constant(Value::Integer(1)),
                "false" => Operand::Constant(Value::Integer(0)),
                "current_timestamp" | "current_date" | "current_time" => Operand::Constant(Value::Text(String::new())),
                _ if cursor.symbol("(") => {
                    // DATETIME('…'), DATE('now')…: only dates are computed in these statements,
                    // and the reader does not use dates. Keeps the first argument.
                    if !["datetime", "date", "time", "strftime"].contains(&lower.as_str()) {
                        return Err(format!("function {word} not supported"));
                    }
                    let first = operand(cursor)?;
                    while cursor.symbol(",") {
                        operand(cursor)?;
                    }
                    cursor.expect_symbol(")")?;
                    first
                }
                _ if cursor.symbol(".") => Operand::Column(cursor.name()?),
                _ => Operand::Column(lower),
            }
        }
        Token::Symbol(s) => return Err(format!("unexpected '{s}'")),
    };
    Ok(operand)
}

fn number_value(number: &str) -> Result<Value, String> {
    if let Ok(n) = number.parse::<i64>() {
        return Ok(Value::Integer(n));
    }
    number.parse::<f64>().map(Value::Real).map_err(|_| format!("bad number {number}"))
}

/// A WHERE clause: comparisons joined by AND, OR, NOT and parentheses.
fn condition(cursor: &mut Cursor) -> Result<Condition, String> {
    let mut left = conjunction(cursor)?;
    while cursor.keyword("or") {
        left = Condition::Or(Box::new(left), Box::new(conjunction(cursor)?));
    }
    Ok(left)
}

fn conjunction(cursor: &mut Cursor) -> Result<Condition, String> {
    let mut left = comparison(cursor)?;
    while cursor.keyword("and") {
        left = Condition::And(Box::new(left), Box::new(comparison(cursor)?));
    }
    Ok(left)
}

fn comparison(cursor: &mut Cursor) -> Result<Condition, String> {
    if cursor.keyword("not") {
        return Ok(Condition::Not(Box::new(comparison(cursor)?)));
    }
    if cursor.symbol("(") {
        let inner = condition(cursor)?;
        cursor.expect_symbol(")")?;
        return Ok(inner);
    }
    let left = operand(cursor)?;
    let equal = match cursor.next() {
        Some(Token::Symbol(s)) if s == "=" || s == "==" => true,
        Some(Token::Symbol(s)) if s == "!=" || s == "<>" => false,
        Some(Token::Word(w)) if w.eq_ignore_ascii_case("is") => !cursor.keyword("not"),
        _ => return Err("comparison not supported".into()),
    };
    let right = operand(cursor)?;
    Ok(if equal { Condition::Equal(left, right) } else { Condition::NotEqual(left, right) })
}

/// The tables this module keeps.
fn tracked(table: &str) -> bool {
    table == "configs" || table == "selected_config"
}

/// Applies one statement; `Err` when it touches a tracked table in a way not understood.
fn apply(tables: &mut Tables, tokens: &[Token]) -> Result<(), String> {
    let mut cursor = Cursor { tokens, at: 0 };
    if cursor.keyword("create") || cursor.keyword("alter") {
        return schema(tables, tokens);
    }
    let defaults = &tables.defaults;
    let replay = &mut tables.replay;
    if cursor.keyword("insert") {
        let mode = if cursor.keyword("or") {
            match cursor.next() {
                Some(Token::Word(w)) => w.to_ascii_lowercase(),
                _ => return Err("INSERT OR what?".into()),
            }
        } else {
            String::new()
        };
        cursor.expect_keyword("into")?;
        let table = cursor.name()?;
        if !tracked(&table) {
            return Ok(());
        }
        insert(replay, &mut cursor, &table, &mode, defaults)
    } else if cursor.keyword("update") {
        let table = cursor.name()?;
        if !tracked(&table) {
            return Ok(());
        }
        update(replay, &mut cursor, &table)
    } else if cursor.keyword("delete") {
        cursor.expect_keyword("from")?;
        let table = cursor.name()?;
        if !tracked(&table) {
            return Ok(());
        }
        let condition = if cursor.keyword("where") { condition(&mut cursor)? } else { Condition::All };
        if !cursor.done() {
            return Err("unexpected text after the WHERE clause".into());
        }
        match table.as_str() {
            "configs" => replay.configs.retain(|row| !condition.holds(row)),
            _ => replay.selected.retain(|(vad, id)| !condition.holds(&selection_row(*vad, id))),
        }
        Ok(())
    } else if cursor.keyword("with") {
        refresh_factory_version(replay, tokens)
    } else {
        // DROP, and the copy of the rows into a rebuilt table, which keeps them as they are.
        Ok(())
    }
}

/// `CREATE TABLE` and `ALTER TABLE` on `configs` (and `temp_configs`, which replaces it when
/// GG rebuilds the table): only the column defaults matter, since rows keep their values.
fn schema(tables: &mut Tables, tokens: &[Token]) -> Result<(), String> {
    let mut cursor = Cursor { tokens, at: 0 };
    if cursor.keyword("create") {
        if !cursor.keyword("table") {
            return Ok(());
        }
        let if_not_exists = cursor.keyword("if") && cursor.keyword("not") && cursor.keyword("exists");
        let table = cursor.name()?;
        if table != "configs" && table != "temp_configs" {
            return Ok(());
        }
        cursor.expect_symbol("(")?;
        let mut defaults = BTreeMap::new();
        // One column definition or table constraint per comma at depth 1.
        let mut depth = 1;
        let mut item: Vec<Token> = Vec::new();
        while depth > 0 {
            let token = cursor.next().ok_or("unfinished CREATE TABLE")?;
            match token {
                Token::Symbol(s) if s == "(" => depth += 1,
                Token::Symbol(s) if s == ")" => depth -= 1,
                _ => {}
            }
            if depth == 0 || (depth == 1 && matches!(token, Token::Symbol(s) if s == ",")) {
                column_default(&item, &mut defaults)?;
                item.clear();
            } else {
                item.push(token.clone());
            }
        }
        if table == "temp_configs" {
            tables.rebuilt = defaults;
        } else if !(if_not_exists && tables.created) {
            tables.defaults = defaults;
            tables.created = true;
        }
        return Ok(());
    }
    cursor.expect_keyword("alter")?;
    cursor.expect_keyword("table")?;
    let table = cursor.name()?;
    if cursor.keyword("rename") {
        cursor.expect_keyword("to")?;
        if table == "temp_configs" && cursor.name()? == "configs" {
            tables.defaults = std::mem::take(&mut tables.rebuilt);
            for row in &mut tables.replay.configs {
                complete(&tables.defaults, row);
            }
        }
        return Ok(());
    }
    if table != "configs" {
        return Ok(());
    }
    if !cursor.keyword("add") {
        return Err("ALTER TABLE configs: only ADD is supported".into());
    }
    cursor.keyword("column");
    let mut added = BTreeMap::new();
    column_default(&tokens[cursor.at..], &mut added)?;
    for (column, value) in added {
        for row in &mut tables.replay.configs {
            row.0.entry(column.clone()).or_insert_with(|| value.clone());
        }
        tables.defaults.insert(column, value);
    }
    Ok(())
}

/// The default of one column definition (`name TYPE … DEFAULT value …`); NULL without one.
/// Table constraints (`PRIMARY KEY(…)`, `UNIQUE(…)`, `FOREIGN KEY …`) are skipped.
fn column_default(item: &[Token], defaults: &mut BTreeMap<String, Value>) -> Result<(), String> {
    let Some(Token::Word(name) | Token::Quoted(name)) = item.first() else { return Ok(()) };
    let lower = name.to_ascii_lowercase();
    if ["primary", "unique", "foreign", "check", "constraint"].contains(&lower.as_str()) {
        return Ok(());
    }
    let mut value = Value::Null;
    if let Some(at) = item.iter().position(|t| matches!(t, Token::Word(w) if w.eq_ignore_ascii_case("default"))) {
        let mut cursor = Cursor { tokens: &item[at + 1..], at: 0 };
        value = match operand(&mut cursor)? {
            Operand::Constant(value) => value,
            // `DEFAULT "{}"`: a double-quoted default is a string.
            Operand::Either(text) | Operand::Column(text) => Value::Text(text),
        };
    }
    defaults.insert(lower, value);
    Ok(())
}

fn selection_row(vad: i64, id: &str) -> Row {
    let mut row = Row::default();
    row.0.insert("vad".into(), Value::Integer(vad));
    row.0.insert("config_id".into(), Value::Text(id.to_string()));
    row
}

/// `INSERT [OR REPLACE|IGNORE] INTO <table> (columns) VALUES (…), (…)`.
fn insert(replay: &mut Replay, cursor: &mut Cursor, table: &str, mode: &str, defaults: &BTreeMap<String, Value>) -> Result<(), String> {
    if !cursor.symbol("(") {
        return Err("INSERT without a column list".into());
    }
    let mut columns = vec![cursor.name()?];
    while cursor.symbol(",") {
        columns.push(cursor.name()?);
    }
    cursor.expect_symbol(")")?;
    cursor.expect_keyword("values")?;
    let mut rows = Vec::new();
    loop {
        cursor.expect_symbol("(")?;
        let mut row = Row::default();
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                cursor.expect_symbol(",")?;
            }
            let value = operand(cursor)?.value(&Row::default());
            row.0.insert(column.clone(), value);
        }
        cursor.expect_symbol(")")?;
        rows.push(row);
        if !cursor.symbol(",") {
            break;
        }
    }
    if !cursor.done() {
        return Err("unexpected text after VALUES".into());
    }
    for mut row in rows {
        if table == "configs" {
            complete(defaults, &mut row);
            let Some(id) = row.id().map(str::to_string) else { return Err("a preset without id".into()) };
            match replay.configs.iter().position(|r| r.id() == Some(id.as_str())) {
                Some(_) if mode == "ignore" => {}
                Some(at) => replay.configs[at] = row,
                None => replay.configs.push(row),
            }
        } else {
            let (Some(vad), Some(id)) = (row.integer("vad"), row.text("config_id").map(str::to_string)) else { return Err("a selection without channel or preset".into()) };
            // Both columns are UNIQUE: a new row replaces any row that shares either.
            let clashes = |(v, i): &(i64, String)| *v == vad || *i == id;
            if mode == "ignore" && replay.selected.iter().any(clashes) {
                continue;
            }
            replay.selected.retain(|entry| !clashes(entry));
            replay.selected.push((vad, id));
        }
    }
    Ok(())
}

/// `UPDATE <table> SET column = value, … [WHERE …]`. Every new value is computed from the row as
/// it was before the statement, as SQL does.
fn update(replay: &mut Replay, cursor: &mut Cursor, table: &str) -> Result<(), String> {
    cursor.expect_keyword("set")?;
    let mut assignments = Vec::new();
    loop {
        let column = cursor.name()?;
        cursor.expect_symbol("=")?;
        assignments.push((column, operand(cursor)?));
        if !cursor.symbol(",") {
            break;
        }
    }
    let condition = if cursor.keyword("where") { condition(cursor)? } else { Condition::All };
    if !cursor.done() {
        return Err("expression not supported".into());
    }
    if table == "configs" {
        for row in replay.configs.iter_mut().filter(|row| condition.holds(row)) {
            let values: Vec<(String, Value)> = assignments.iter().map(|(column, value)| (column.clone(), value.value(row))).collect();
            row.0.extend(values);
        }
    } else {
        for (vad, id) in replay.selected.iter_mut() {
            let row = selection_row(*vad, id);
            if !condition.holds(&row) {
                continue;
            }
            for (column, value) in &assignments {
                match (column.as_str(), value.value(&row)) {
                    ("config_id", Value::Text(text)) => *id = text,
                    ("vad", value) => *vad = value.as_integer().ok_or("bad channel number")?,
                    _ => return Err("selection change not supported".into()),
                }
            }
        }
    }
    Ok(())
}

/// GG's "new factory version" statement: `WITH const AS (SELECT '<id>' AS id, '<json>' AS
/// default_data), compare_user_default_data AS (…) UPDATE configs SET default_data = (SELECT
/// default_data FROM const), data = CASE WHEN (…IS_EQUAL…) = 1 THEN (SELECT default_data FROM const)
/// ELSE configs.data END WHERE configs.id = (SELECT id FROM const)`. The factory version is always
/// replaced; the preset itself too, unless it differs from the old factory version on a value
/// both have (the user changed it), as the SQL's `json_tree` comparison decides.
fn refresh_factory_version(replay: &mut Replay, tokens: &[Token]) -> Result<(), String> {
    let mut cursor = Cursor { tokens, at: 0 };
    cursor.expect_keyword("with")?;
    if !cursor.keyword("const") {
        return if mentions_tracked(tokens) { Err("WITH statement not supported".into()) } else { Ok(()) };
    }
    cursor.expect_keyword("as")?;
    cursor.expect_symbol("(")?;
    cursor.expect_keyword("select")?;
    let mut constants: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let value = match cursor.next() {
            Some(Token::Text(text)) => text.clone(),
            _ => return Err("WITH const: text expected".into()),
        };
        cursor.expect_keyword("as")?;
        constants.insert(cursor.name()?, value);
        if !cursor.symbol(",") {
            break;
        }
    }
    cursor.expect_symbol(")")?;
    let words: Vec<String> = tokens.iter().filter_map(|t| if let Token::Word(w) = t { Some(w.to_ascii_lowercase()) } else { None }).collect();
    let has = |sequence: &[&str]| words.windows(sequence.len()).any(|w| w.iter().zip(sequence).all(|(a, b)| a == b));
    let (Some(id), Some(fresh)) = (constants.remove("id"), constants.remove("default_data")) else { return Err("WITH const without id or default_data".into()) };
    if !(has(&["update", "configs", "set"]) && has(&["select", "default_data", "from", "const"]) && has(&["json_tree", "default_data"]) && has(&["else", "configs", "data", "end"])) {
        return Err("WITH const: not the factory refresh GG uses".into());
    }
    // Other constants (`game_ids` in 1.83.0) are plain `column = (SELECT column FROM const)`.
    for column in constants.keys() {
        if !has(&["select", column, "from", "const"]) {
            return Err(format!("WITH const: {column} is not assigned as expected"));
        }
    }
    if let Some(row) = replay.configs.iter_mut().find(|row| row.id() == Some(id.as_str())) {
        // `json_valid(data)` is strict JSON; `json_tree(default_data)` also reads JSON5, which
        // GG's first migrations wrote (unquoted keys).
        let unchanged = match (row.text("data").map(Json::parse), row.text("default_data").map(|old| Json::parse(&strict_json(old)))) {
            (Some(Err(_)), _) | (None, _) => true,
            (Some(Ok(data)), Some(Ok(old))) => !differs_on_shared_values(&data, &old),
            // An old factory version that is not JSON makes SQLite's json_tree fail: nothing
            // would be updated.
            (Some(Ok(_)), _) => return Err("WITH const: the old factory version is not JSON".into()),
        };
        if unchanged {
            row.0.insert("data".into(), Value::Text(fresh.clone()));
        }
        row.0.insert("default_data".into(), Value::Text(fresh));
        for (column, value) in constants {
            row.0.insert(column, Value::Text(value));
        }
    }
    Ok(())
}

/// Strict JSON from the relaxed JSON (JSON5-like) of GG's first migrations, which GG's service
/// reads with a lenient parser: bare keys get quotes, single-quoted strings double quotes, and
/// trailing commas and comments go. Strict JSON comes out unchanged.
pub fn strict_json(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 64);
    let mut i = 0;
    // The next character after blanks and comments, from `from`.
    let next_significant = |mut at: usize| -> Option<char> {
        loop {
            match chars.get(at) {
                Some(c) if c.is_whitespace() => at += 1,
                Some('/') if chars.get(at + 1) == Some(&'/') => {
                    while chars.get(at).is_some_and(|c| *c != '\n') {
                        at += 1;
                    }
                }
                Some('/') if chars.get(at + 1) == Some(&'*') => {
                    at += 2;
                    while at < chars.len() && !(chars[at] == '*' && chars.get(at + 1) == Some(&'/')) {
                        at += 1;
                    }
                    at += 2;
                }
                other => return other.copied(),
            }
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                out.push(c);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if chars[i - 1] == '"' {
                        break;
                    }
                }
            }
            '\'' => {
                out.push('"');
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    match chars[i] {
                        '\\' if chars.get(i + 1) == Some(&'\'') => {
                            out.push('\'');
                            i += 1;
                        }
                        '\\' => {
                            out.push('\\');
                            if let Some(next) = chars.get(i + 1) {
                                out.push(*next);
                            }
                            i += 1;
                        }
                        '"' => out.push_str("\\\""),
                        other => out.push(other),
                    }
                    i += 1;
                }
                out.push('"');
                i += 1;
            }
            '/' if matches!(chars.get(i + 1), Some('/') | Some('*')) => {
                let block = chars[i + 1] == '*';
                i += 2;
                while i < chars.len() {
                    if block && chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        i += 2;
                        break;
                    }
                    if !block && chars[i] == '\n' {
                        break;
                    }
                    i += 1;
                }
            }
            ',' if matches!(next_significant(i + 1), Some('}') | Some(']')) => i += 1,
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if next_significant(i) == Some(':') {
                    out.push('"');
                    out.push_str(&word);
                    out.push('"');
                } else {
                    out.push_str(&word);
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn mentions_tracked(tokens: &[Token]) -> bool {
    tokens.iter().any(|t| matches!(t, Token::Word(w) | Token::Quoted(w) if tracked(&w.to_ascii_lowercase())))
}

/// Whether `data` has, at the path of one of `old`'s plain values (not objects or arrays), a
/// different plain value. Paths missing from `data` do not count (SQL compares NULL to nothing),
/// and `true` equals 1 (SQLite's JSON functions turn booleans into integers).
pub fn differs_on_shared_values(data: &Json, old: &Json) -> bool {
    let mut leaves = Vec::new();
    collect_leaves(old, &mut Vec::new(), &mut leaves);
    leaves.iter().any(|(path, value)| at_path(data, path).is_some_and(|found| plain_differs(found, value)))
}

#[derive(Clone, Debug)]
enum Step {
    Key(String),
    Index(usize),
}

fn collect_leaves<'a>(json: &'a Json, path: &mut Vec<Step>, leaves: &mut Vec<(Vec<Step>, &'a Json)>) {
    match json {
        Json::Object(members) => {
            for (key, value) in members {
                path.push(Step::Key(key.clone()));
                collect_leaves(value, path, leaves);
                path.pop();
            }
        }
        Json::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                path.push(Step::Index(index));
                collect_leaves(value, path, leaves);
                path.pop();
            }
        }
        Json::Null => {}
        plain => leaves.push((path.clone(), plain)),
    }
}

fn at_path<'a>(json: &'a Json, path: &[Step]) -> Option<&'a Json> {
    let mut current = json;
    for step in path {
        current = match (step, current) {
            (Step::Key(key), Json::Object(_)) => current.get(key)?,
            (Step::Index(index), Json::Array(items)) => items.get(*index)?,
            _ => return None,
        };
    }
    Some(current)
}

fn plain_differs(a: &Json, b: &Json) -> bool {
    let number = |j: &Json| match j {
        Json::Number(x) => Some(*x),
        Json::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
        _ => None,
    };
    match (a, b) {
        (Json::String(x), Json::String(y)) => x != y,
        (Json::Null, _) | (_, Json::Null) => false,
        _ => match (number(a), number(b)) {
            (Some(x), Some(y)) => x != y,
            _ => true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, sql: &str) -> (String, String) {
        (name.to_string(), format!("-- +goose Up\n{sql}\n-- +goose Down\nDELETE FROM configs;\n"))
    }

    const GAME: &str = r#"{"a":1,"b":{"c":true,"d":"x"}}"#;

    #[test]
    fn statements_split_outside_quotes_and_comments() {
        let sql = "INSERT INTO t VALUES ('a;b', \"c;d\"); -- no; split\n/* nor; here */ UPDATE t SET x = 'it''s';;";
        assert_eq!(statements(sql), ["INSERT INTO t VALUES ('a;b', \"c;d\")", "UPDATE t SET x = 'it''s'"]);
        assert_eq!(up_section("-- +goose Up\nA;\n-- +goose Down\nB;"), "\nA;");
        assert_eq!(up_section("A;"), "A;");
    }

    #[test]
    fn files_go_in_goose_order() {
        let mut files = vec![("sonar-1.10.0\\20221013_squash.sql".to_string(), String::new()), ("Sonar-1.4.0\\20220601_squash.sql".to_string(), String::new()), ("sonar-0.2.0\\20210922_squash.sql".to_string(), String::new())];
        sort_files(&mut files);
        let names: Vec<&str> = files.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, ["sonar-0.2.0\\20210922_squash.sql", "Sonar-1.4.0\\20220601_squash.sql", "sonar-1.10.0\\20221013_squash.sql"]);
        assert_eq!(folder_version("Sonar-1.4.0\\20220601_squash.sql"), Some((1, 4, 0)));
        assert_eq!(folder_version("db-migrations/sonar-1.106.0/x.sql"), Some((1, 106, 0)));
        assert_eq!(folder_version("20220601_squash.sql"), None);
        assert_eq!(replay(&files).sonar_version, Some((1, 10, 0)));
    }

    #[test]
    fn inserts_updates_renames_and_deletes() {
        let replay = replay(&[
            file("1_a.sql", &format!("CREATE TABLE IF NOT EXISTS configs (id TEXT);\nINSERT INTO configs (id,name,vad,data,schema_version,created_at,updated_at) VALUES ('g1','Flat',1,'{GAME}',5,DATETIME('2021-09-22 09:13:24'),DATETIME('now')), ('g2','Old',1,'{{}}',5,DATETIME('now'),DATETIME('now'));\nINSERT INTO key_value VALUES ('a', 'b');")),
            file("2_b.sql", "ALTER TABLE configs ADD is_preset INTEGER DEFAULT 0;\nUPDATE configs SET default_data = data WHERE id='g1';\nUPDATE configs SET name = \"Renamed\", updated_at = DATETIME('now') WHERE configs.id = 'g1' and name='Flat';\nUPDATE configs SET game_ids = '1_42, 7_x' WHERE id = 'g1';\nDELETE FROM configs WHERE id = 'g2';"),
            file("3_c.sql", "INSERT INTO selected_config (vad, config_id) VALUES (1, 'g1');\nINSERT OR REPLACE INTO selected_config (vad, config_id) VALUES (1, 'g3');\nUPDATE selected_config SET config_id = 'g1' WHERE config_id='g3';\nUPDATE configs SET is_preset = 1 WHERE is_preset = 0 AND (vad = 1 OR vad = 2);"),
        ]);
        assert!(replay.ignored.is_empty(), "{:?}", replay.ignored);
        assert_eq!(replay.files, 3);
        assert_eq!(replay.configs.len(), 1);
        let row = &replay.configs[0];
        assert_eq!(row.text("name"), Some("Renamed"));
        assert_eq!(row.text("default_data"), Some(GAME));
        assert_eq!(row.text("game_ids"), Some("1_42, 7_x"));
        assert_eq!(row.integer("vad"), Some(1));
        // Added after the row was inserted, the column takes its default for it, as in SQLite,
        // so the last UPDATE of file 3 reaches it.
        assert_eq!(row.integer("is_preset"), Some(1));
        assert_eq!(row.integer("schema_version"), Some(5));
        assert_eq!(replay.selected, [(1, "g1".to_string())]);
    }

    #[test]
    fn the_where_clause_is_honoured() {
        let replay = replay(&[file("1.sql", "INSERT INTO configs (id,vad,is_preset,name) VALUES ('a',1,1,'x'),('b',2,1,'x'),('c',3,0,'x'),('d',4,1,'x');\nUPDATE configs SET name='y' WHERE is_preset = 1 AND (vad = 1 OR vad = 4);\nUPDATE configs SET name='z' WHERE id != 'a' AND NOT vad == 3 AND name = 'x';")]);
        let names: Vec<&str> = replay.configs.iter().map(|r| r.text("name").unwrap_or("")).collect();
        assert_eq!(names, ["y", "z", "x", "y"]);
        assert!(replay.ignored.is_empty());
    }

    #[test]
    fn insert_or_replace_and_or_ignore() {
        let replay = replay(&[file("1.sql", "INSERT INTO configs (id,name) VALUES ('a','first');\nINSERT OR IGNORE INTO configs (id,name) VALUES ('a','ignored');\nINSERT OR REPLACE INTO configs (id,name,vad) VALUES ('a','replaced',-1);")]);
        assert_eq!(replay.configs.len(), 1);
        assert_eq!(replay.configs[0].text("name"), Some("replaced"));
        assert_eq!(replay.configs[0].integer("vad"), Some(-1));
    }

    #[test]
    fn statements_not_understood_are_reported_not_guessed() {
        let replay = replay(&[file("1.sql", "INSERT INTO configs (id,name) VALUES ('a','x');\nUPDATE configs SET name = name || '!' WHERE id = 'a';\nUPDATE configs SET name = (SELECT 1) WHERE id = 'a';\nINSERT INTO temp_configs SELECT * FROM configs;\nUPDATE key_value SET value = value + 1 where key = 'v';")]);
        assert_eq!(replay.configs[0].text("name"), Some("x"));
        assert_eq!(replay.ignored.len(), 2, "{:?}", replay.ignored);
        assert!(replay.ignored[0].starts_with("1.sql: "));
    }

    fn refresh(id: &str, json: &str) -> String {
        format!(
            "WITH const AS ( SELECT '{id}' AS id, '{json}' AS default_data ), compare_user_default_data AS ( SELECT valid_configs.id, CASE WHEN SUM(json_extract(data, default_data_json.fullkey) != value) = 0 THEN 1 ELSE 0 END AS IS_EQUAL FROM ( SELECT id, data FROM configs WHERE json_valid(data) = 1 ) AS valid_configs JOIN json_tree(default_data) AS default_data_json WHERE valid_configs.id = (SELECT id FROM const) AND default_data_json.type NOT IN ('object', 'array') GROUP BY valid_configs.id UNION ALL SELECT id, 1 AS IS_EQUAL FROM configs WHERE json_valid(data) = 0 AND id = (SELECT id FROM const) ) UPDATE configs SET default_data = (SELECT default_data from const), data = CASE WHEN (SELECT IS_EQUAL from compare_user_default_data) = 1 THEN (SELECT default_data from const) ELSE configs.data END WHERE configs.id = (SELECT id FROM const);"
        )
    }

    #[test]
    fn the_factory_refresh_keeps_a_changed_preset() {
        let edited = r#"{"a":2,"b":{"c":true,"d":"x"}}"#;
        let same_but_written_differently = r#"{"b":{"d":"x","c":1},"a":1.0,"extra":5}"#;
        let fresh = r#"{"a":3}"#;
        let replay = replay(&[file(
            "1.sql",
            &format!(
                "INSERT INTO configs (id,data,default_data) VALUES ('kept','{edited}','{GAME}'), ('moved','{same_but_written_differently}','{GAME}'), ('broken','not json','{GAME}');\n{}\n{}\n{}\n{}",
                refresh("kept", fresh),
                refresh("moved", fresh),
                refresh("broken", fresh),
                refresh("absent", fresh)
            ),
        )]);
        assert!(replay.ignored.is_empty(), "{:?}", replay.ignored);
        let row = |id: &str| replay.configs.iter().find(|r| r.text("id") == Some(id)).unwrap();
        assert_eq!(row("kept").text("data"), Some(edited));
        assert_eq!(row("kept").text("default_data"), Some(fresh));
        assert_eq!(row("moved").text("data"), Some(fresh));
        assert_eq!(row("broken").text("data"), Some(fresh));
        assert_eq!(replay.configs.len(), 3);
    }

    #[test]
    fn relaxed_json_becomes_strict() {
        let relaxed = "{\n  bassBoostState: { enabled: false, value: 0, },\n  'name': 'it\\'s \"x\"', // comment\n  list: [1, 2,],\n  /* block */ \"kept\": \"a, }\" }";
        let strict = strict_json(relaxed);
        let json = Json::parse(&strict).unwrap_or_else(|e| panic!("{e}: {strict}"));
        assert_eq!(json.get("bassBoostState").and_then(|b| b.get("enabled")), Some(&Json::Bool(false)));
        assert_eq!(json.get("name").and_then(Json::as_str), Some("it's \"x\""));
        assert_eq!(json.get("list").and_then(Json::as_array).map(<[Json]>::len), Some(2));
        assert_eq!(json.get("kept").and_then(Json::as_str), Some("a, }"));
        let already = r#"{"a":[1,{"b":"c:d"}],"e":true}"#;
        assert_eq!(strict_json(already), already);
    }

    #[test]
    fn shared_values_compare_as_sqlite_does() {
        let j = |t: &str| Json::parse(t).unwrap();
        assert!(!differs_on_shared_values(&j(r#"{"a":1,"b":[1,{"c":"x"}]}"#), &j(r#"{"b":[1.0,{"c":"x"}],"a":true}"#)));
        assert!(differs_on_shared_values(&j(r#"{"b":[1,{"c":"y"}]}"#), &j(r#"{"b":[1,{"c":"x"}]}"#)));
        assert!(!differs_on_shared_values(&j(r#"{}"#), &j(r#"{"a":1}"#)));
        assert!(differs_on_shared_values(&j(r#"{"a":"1"}"#), &j(r#"{"a":1}"#)));
    }
}
