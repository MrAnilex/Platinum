//! A read-only reader for SQLite database files, enough for GG's preset database.
//!
//! Platinum has no dependency, so this reads the file format directly (sqlite.org/fileformat.html):
//! the 100-byte header, table b-trees (interior and leaf pages), overflow chains and records. It
//! reads rowid tables only, in UTF-8 databases; index b-trees are never needed, because a full scan
//! of a few hundred rows is enough.
//!
//! The whole file is read into memory and nothing is ever written: GG owns the file, and a real
//! SQLite library opened by mistake in read-write mode could create a journal next to it. Instead
//! of taking SQLite's locks, `open` reads the file twice and keeps it only when both reads agree and
//! no journal shows a write in progress (`analysis/sonar/presets.md`, section 4.5).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A value as SQLite stores it.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlValue {
    pub fn as_i64(&self) -> Option<i64> {
        if let SqlValue::Integer(n) = self { Some(*n) } else { None }
    }

    /// Reals, and integers converted.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            SqlValue::Real(x) => Some(*x),
            SqlValue::Integer(n) => Some(*n as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let SqlValue::Text(text) = self { Some(text) } else { None }
    }

    pub fn is_null(&self) -> bool {
        *self == SqlValue::Null
    }
}

/// One row of `sqlite_schema` (also called `sqlite_master`).
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaEntry {
    /// "table", "index", "view" or "trigger".
    pub kind: String,
    pub name: String,
    pub table: String,
    /// First page of its b-tree; 0 for views and triggers.
    pub root: u32,
    pub sql: String,
}

/// A table read whole: its column names in declaration order and its rows in rowid order.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub rowid: i64,
    /// One value per column of the table, defaults filled in.
    pub values: Vec<SqlValue>,
    /// How many columns the record really holds: rows written before an `ALTER TABLE ADD COLUMN`
    /// hold fewer, and the declared defaults stand for the rest.
    pub stored: usize,
}

/// A row seen with its table's column names.
#[derive(Clone, Copy, Debug)]
pub struct RowRef<'a> {
    table: &'a Table,
    row: &'a Row,
}

static NULL: SqlValue = SqlValue::Null;

impl Table {
    /// Index of a column; SQL names ignore ASCII case.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
    }

    pub fn rows(&self) -> impl Iterator<Item = RowRef<'_>> {
        self.rows.iter().map(move |row| RowRef { table: self, row })
    }
}

impl<'a> RowRef<'a> {
    pub fn rowid(&self) -> i64 {
        self.row.rowid
    }

    /// The value of a column; NULL for a column the table does not have.
    pub fn get(&self, column: &str) -> &'a SqlValue {
        self.table.column(column).and_then(|i| self.row.values.get(i)).unwrap_or(&NULL)
    }

    pub fn text(&self, column: &str) -> Option<&'a str> {
        self.get(column).as_str()
    }

    pub fn integer(&self, column: &str) -> Option<i64> {
        self.get(column).as_i64()
    }
}

/// A database file held in memory.
pub struct Database {
    bytes: Vec<u8>,
    page_size: usize,
    usable: usize,
    page_count: u32,
    schema: Vec<SchemaEntry>,
}

impl std::fmt::Debug for Database {
    /// The shape only: the file's bytes would drown everything else.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").field("page_size", &self.page_size).field("pages", &self.page_count).field("tables", &self.table_names()).finish()
    }
}

/// How many times `open` reads the file before giving up on a write in progress.
const ATTEMPTS: u32 = 5;

impl Database {
    /// Reads a database file. The file is opened for reading only and shared with its writer
    /// (Rust's default on Windows shares read, write and delete), so GG is never blocked.
    pub fn open(path: &Path) -> Result<Database, String> {
        let shown = path.display();
        let mut busy = false;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
            if journal_is_hot(path) {
                busy = true;
                continue;
            }
            let first = std::fs::read(path).map_err(|e| format!("cannot read {shown}: {e}"))?;
            let second = std::fs::read(path).map_err(|e| format!("cannot read {shown}: {e}"))?;
            if first != second || journal_is_hot(path) {
                busy = true;
                continue;
            }
            if is_wal(&first) && wal_has_frames(path) {
                return Err(format!("{shown} is in WAL mode with changes not yet merged into it; this reader cannot see them"));
            }
            return Database::from_bytes(first).map_err(|e| format!("{shown}: {e}"));
        }
        let why = if busy { "it was being written each time" } else { "unknown reason" };
        Err(format!("cannot read {shown} in a consistent state after {ATTEMPTS} tries ({why}); try again"))
    }

    /// Reads a database already in memory.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Database, String> {
        if bytes.len() < 100 || &bytes[..16] != b"SQLite format 3\0" {
            return Err("not an SQLite 3 database".into());
        }
        let page_size = match u16::from_be_bytes([bytes[16], bytes[17]]) {
            1 => 65_536,
            n if n >= 512 && n.is_power_of_two() => usize::from(n),
            n => return Err(format!("invalid page size {n}")),
        };
        let usable = page_size - usize::from(bytes[20]);
        if usable < 480 {
            return Err("invalid reserved space per page".into());
        }
        // 0 is what a database with no table yet holds; anything but UTF-8 is refused.
        let encoding = be32(&bytes, 56)?;
        if encoding > 1 {
            return Err(format!("text encoding {encoding} (UTF-16) is not supported"));
        }
        let in_file = (bytes.len() / page_size) as u32;
        // The header's page count is valid only when written by the same change as the counter.
        let declared = be32(&bytes, 28)?;
        let page_count = if declared != 0 && be32(&bytes, 92)? == be32(&bytes, 24)? { declared } else { in_file };
        if page_count == 0 || page_count > in_file {
            return Err(format!("the file is truncated ({in_file} pages, {page_count} expected)"));
        }
        let mut database = Database { bytes, page_size, usable, page_count, schema: Vec::new() };
        database.schema = database.read_schema()?;
        Ok(database)
    }

    /// The schema rows: tables, indexes, views and triggers.
    pub fn schema(&self) -> &[SchemaEntry] {
        &self.schema
    }

    /// The names of the tables.
    pub fn table_names(&self) -> Vec<&str> {
        self.schema.iter().filter(|e| e.kind == "table").map(|e| e.name.as_str()).collect()
    }

    /// The file change counter: it changes with every committed write, so a caller can tell
    /// whether a new read would differ.
    pub fn change_counter(&self) -> u32 {
        be32(&self.bytes, 24).unwrap_or(0)
    }

    /// Reads a whole table, found by name (ignoring ASCII case) in the schema.
    pub fn table(&self, name: &str) -> Result<Table, String> {
        let entry = self
            .schema
            .iter()
            .find(|e| e.kind == "table" && e.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("no table named {name}"))?;
        let shape = parse_create_table(&entry.sql).map_err(|e| format!("table {}: {e}", entry.name))?;
        if shape.without_rowid {
            return Err(format!("table {} is a WITHOUT ROWID table, which this reader does not read", entry.name));
        }
        let mut rows = Vec::new();
        for (rowid, payload) in self.cells(entry.root)? {
            let mut values = decode_record(&payload).map_err(|e| format!("table {}, row {rowid}: {e}", entry.name))?;
            let stored = values.len();
            values.truncate(shape.columns.len());
            values.extend(shape.columns[values.len()..].iter().map(|c| c.default.clone()));
            for (value, column) in values.iter_mut().zip(&shape.columns) {
                if column.real {
                    // A REAL column stores whole numbers as integers on disk; SQLite reads them back as reals.
                    if let SqlValue::Integer(n) = *value {
                        *value = SqlValue::Real(n as f64);
                    }
                }
            }
            if let Some(alias) = shape.rowid_alias {
                values[alias] = SqlValue::Integer(rowid);
            }
            rows.push(Row { rowid, values, stored });
        }
        Ok(Table { name: entry.name.clone(), columns: shape.columns.into_iter().map(|c| c.name).collect(), rows })
    }

    fn read_schema(&self) -> Result<Vec<SchemaEntry>, String> {
        let mut entries = Vec::new();
        for (rowid, payload) in self.cells(1)? {
            let values = decode_record(&payload).map_err(|e| format!("schema row {rowid}: {e}"))?;
            let text = |i: usize| values.get(i).and_then(SqlValue::as_str).unwrap_or("").to_string();
            let root = values.get(3).and_then(SqlValue::as_i64).unwrap_or(0);
            entries.push(SchemaEntry { kind: text(0), name: text(1), table: text(2), root: u32::try_from(root).unwrap_or(0), sql: text(4) });
        }
        Ok(entries)
    }

    /// Page `number` (from 1).
    fn page(&self, number: u32) -> Result<&[u8], String> {
        if number == 0 || number > self.page_count {
            return Err(format!("page {number} is outside the file ({} pages)", self.page_count));
        }
        let start = (number as usize - 1) * self.page_size;
        Ok(&self.bytes[start..start + self.page_size])
    }

    /// Every cell of a table b-tree in rowid order, as (rowid, whole payload).
    fn cells(&self, root: u32) -> Result<Vec<(i64, Vec<u8>)>, String> {
        let mut cells = Vec::new();
        let mut stack = vec![root];
        let mut seen = HashSet::new();
        while let Some(number) = stack.pop() {
            if !seen.insert(number) {
                return Err(format!("page {number} is reached twice: the file is damaged"));
            }
            let page = self.page(number)?;
            // Page 1 starts with the file header.
            let header = if number == 1 { 100 } else { 0 };
            let count = be16(page, header + 3)?;
            match page[header] {
                0x05 => {
                    let mut children = Vec::with_capacity(count + 1);
                    for i in 0..count {
                        children.push(be32(page, be16(page, header + 12 + 2 * i)?)?);
                    }
                    children.push(be32(page, header + 8)?);
                    // Last pushed is visited first: push right to left.
                    stack.extend(children.into_iter().rev());
                }
                0x0D => {
                    for i in 0..count {
                        cells.push(self.leaf_cell(page, be16(page, header + 8 + 2 * i)?)?);
                    }
                }
                0x02 | 0x0A => return Err(format!("page {number} belongs to an index, not to a table")),
                kind => return Err(format!("page {number} has an unknown type {kind:#04x}")),
            }
        }
        Ok(cells)
    }

    /// One leaf cell: its rowid and its payload, with the overflow pages appended.
    fn leaf_cell(&self, page: &[u8], at: usize) -> Result<(i64, Vec<u8>), String> {
        let (length, used) = varint(page, at)?;
        let (rowid, used2) = varint(page, at + used)?;
        let length = usize::try_from(length).map_err(|_| "payload too large".to_string())?;
        let start = at + used + used2;
        let local = self.local_size(length);
        let mut payload = slice(page, start, local)?.to_vec();
        if local < length {
            let mut next = be32(page, start + local)?;
            let mut hops = 0;
            while payload.len() < length {
                hops += 1;
                if next == 0 || hops > self.page_count {
                    return Err("an overflow chain ends early or loops: the file is damaged".into());
                }
                let overflow = self.page(next)?;
                let take = (length - payload.len()).min(self.usable - 4);
                payload.extend_from_slice(slice(overflow, 4, take)?);
                next = be32(overflow, 0)?;
            }
        }
        Ok((rowid as i64, payload))
    }

    /// How many payload bytes a table leaf keeps on its own page (the file format's rule).
    fn local_size(&self, length: usize) -> usize {
        let usable = self.usable;
        let most = usable - 35;
        if length <= most {
            return length;
        }
        let least = (usable - 12) * 32 / 255 - 23;
        let local = least + (length - least) % (usable - 4);
        if local <= most { local } else { least }
    }
}

/// Whether a rollback journal next to the file shows a write in progress (or a crashed one that
/// SQLite would roll back first). An empty or zeroed journal header means nothing pending.
fn journal_is_hot(path: &Path) -> bool {
    let journal = sibling(path, "-journal");
    match std::fs::read(&journal) {
        Ok(bytes) => bytes.iter().take(28).any(|b| *b != 0),
        Err(_) => false,
    }
}

fn is_wal(bytes: &[u8]) -> bool {
    bytes.len() > 19 && (bytes[18] == 2 || bytes[19] == 2)
}

fn wal_has_frames(path: &Path) -> bool {
    std::fs::metadata(sibling(path, "-wal")).is_ok_and(|m| m.len() > 32)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn slice(bytes: &[u8], at: usize, length: usize) -> Result<&[u8], String> {
    at.checked_add(length).and_then(|end| bytes.get(at..end)).ok_or_else(|| "a record runs past its page: the file is damaged".to_string())
}

fn be16(bytes: &[u8], at: usize) -> Result<usize, String> {
    let b = slice(bytes, at, 2)?;
    Ok(usize::from(u16::from_be_bytes([b[0], b[1]])))
}

fn be32(bytes: &[u8], at: usize) -> Result<u32, String> {
    let b = slice(bytes, at, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// A variable-length integer: 7 bits per byte, high bit set when more follow, and all 8 bits of
/// the ninth byte. Returns the value and the number of bytes read.
fn varint(bytes: &[u8], at: usize) -> Result<(u64, usize), String> {
    let mut value = 0u64;
    for i in 0..9 {
        let byte = *bytes.get(at + i).ok_or("a number runs past its page: the file is damaged")?;
        if i == 8 {
            return Ok(((value << 8) | u64::from(byte), 9));
        }
        value = (value << 7) | u64::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    unreachable!()
}

/// Splits a record into its values: a header of serial types, then the values one after another.
fn decode_record(payload: &[u8]) -> Result<Vec<SqlValue>, String> {
    let (header_size, mut at) = varint(payload, 0)?;
    let header_size = usize::try_from(header_size).map_err(|_| "bad record header".to_string())?;
    if header_size > payload.len() {
        return Err("the record header runs past the record".into());
    }
    let mut body = header_size;
    let mut values = Vec::new();
    while at < header_size {
        let (serial, used) = varint(payload, at)?;
        at += used;
        let (value, size) = decode_value(serial, payload, body)?;
        values.push(value);
        body += size;
    }
    Ok(values)
}

/// One value of serial type `serial` starting at `at`; returns it with its size in bytes.
fn decode_value(serial: u64, payload: &[u8], at: usize) -> Result<(SqlValue, usize), String> {
    let integer = |size: usize| -> Result<(SqlValue, usize), String> {
        let bytes = slice(payload, at, size)?;
        // Sign-extend from the first byte.
        let mut value: i64 = if bytes[0] & 0x80 != 0 { -1 } else { 0 };
        for byte in bytes {
            value = (value << 8) | i64::from(*byte);
        }
        Ok((SqlValue::Integer(value), size))
    };
    match serial {
        0 => Ok((SqlValue::Null, 0)),
        1..=4 => integer(serial as usize),
        5 => integer(6),
        6 => integer(8),
        7 => {
            let b = slice(payload, at, 8)?;
            Ok((SqlValue::Real(f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])), 8))
        }
        8 => Ok((SqlValue::Integer(0), 0)),
        9 => Ok((SqlValue::Integer(1), 0)),
        10 | 11 => Err(format!("reserved serial type {serial}")),
        n => {
            let size = usize::try_from((n - 12) / 2).map_err(|_| "value too large".to_string())?;
            let bytes = slice(payload, at, size)?;
            if n % 2 == 0 {
                Ok((SqlValue::Blob(bytes.to_vec()), size))
            } else {
                // SQLite does not check what it stores; a damaged character is shown, not refused.
                Ok((SqlValue::Text(String::from_utf8_lossy(bytes).into_owned()), size))
            }
        }
    }
}

/// What the reader needs from a `CREATE TABLE` statement.
#[derive(Debug, PartialEq)]
struct Shape {
    columns: Vec<ColumnShape>,
    /// The INTEGER PRIMARY KEY column, which the record stores as NULL: its value is the rowid.
    rowid_alias: Option<usize>,
    without_rowid: bool,
}

#[derive(Debug, PartialEq)]
struct ColumnShape {
    name: String,
    /// The declared DEFAULT, for records written before the column was added.
    default: SqlValue,
    /// REAL affinity (declared type containing REAL, FLOA or DOUB).
    real: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    /// A bare word: a keyword or an identifier.
    Word(String),
    /// An identifier in double quotes, backquotes or brackets.
    Quoted(String),
    /// A string literal in single quotes.
    Str(String),
    Number(String),
    Punct(char),
}

impl Token {
    fn is_word(&self, word: &str) -> bool {
        matches!(self, Token::Word(w) if w.eq_ignore_ascii_case(word))
    }

    fn name(&self) -> Option<&str> {
        match self {
            Token::Word(s) | Token::Quoted(s) | Token::Str(s) => Some(s),
            _ => None,
        }
    }
}

fn tokenize(sql: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    let quoted = |i: &mut usize, close: char, doubled: bool| -> Result<String, String> {
        let mut text = String::new();
        *i += 1;
        loop {
            match chars.get(*i) {
                None => return Err("unterminated quote in the table's SQL".to_string()),
                Some(&c) if c == close => {
                    if doubled && chars.get(*i + 1) == Some(&close) {
                        text.push(close);
                        *i += 2;
                    } else {
                        *i += 1;
                        return Ok(text);
                    }
                }
                Some(&c) => {
                    text.push(c);
                    *i += 1;
                }
            }
        }
    };
    while let Some(&c) = chars.get(i) {
        match c {
            c if c.is_whitespace() => i += 1,
            '-' if chars.get(i + 1) == Some(&'-') => {
                while chars.get(i).is_some_and(|c| *c != '\n') {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            '\'' => tokens.push(Token::Str(quoted(&mut i, '\'', true)?)),
            '"' => tokens.push(Token::Quoted(quoted(&mut i, '"', true)?)),
            '`' => tokens.push(Token::Quoted(quoted(&mut i, '`', true)?)),
            '[' => tokens.push(Token::Quoted(quoted(&mut i, ']', false)?)),
            c if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) => {
                let start = i;
                while chars.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '.' || ((*c == '+' || *c == '-') && matches!(chars[i - 1], 'e' | 'E'))) {
                    i += 1;
                }
                tokens.push(Token::Number(chars[start..i].iter().collect()));
            }
            c if c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii() => {
                let start = i;
                while chars.get(i).is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '$' || !c.is_ascii()) {
                    i += 1;
                }
                tokens.push(Token::Word(chars[start..i].iter().collect()));
            }
            c => {
                tokens.push(Token::Punct(c));
                i += 1;
            }
        }
    }
    Ok(tokens)
}

/// Words that end a column's type and start its constraints.
const CONSTRAINT_WORDS: [&str; 11] = ["CONSTRAINT", "PRIMARY", "NOT", "NULL", "UNIQUE", "CHECK", "DEFAULT", "COLLATE", "REFERENCES", "GENERATED", "AS"];
/// Words that start a table constraint instead of a column.
const TABLE_CONSTRAINTS: [&str; 5] = ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];

fn parse_create_table(sql: &str) -> Result<Shape, String> {
    let tokens = tokenize(sql)?;
    let open = tokens.iter().position(|t| *t == Token::Punct('(')).ok_or("no column list in the table's SQL")?;
    // Split the column list on its top-level commas.
    let mut groups: Vec<Vec<Token>> = vec![Vec::new()];
    let mut depth = 0;
    let mut close = None;
    for (i, token) in tokens.iter().enumerate().skip(open + 1) {
        match token {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') if depth == 0 => {
                close = Some(i);
                break;
            }
            Token::Punct(')') => depth -= 1,
            Token::Punct(',') if depth == 0 => {
                groups.push(Vec::new());
                continue;
            }
            _ => {}
        }
        groups.last_mut().unwrap().push(token.clone());
    }
    let close = close.ok_or("unbalanced parentheses in the table's SQL")?;
    let tail = &tokens[close + 1..];
    let without_rowid = tail.windows(2).any(|w| w[0].is_word("WITHOUT") && w[1].is_word("ROWID"));

    let mut columns = Vec::new();
    let mut declared_types = Vec::new();
    let mut alias = None;
    let mut table_key: Option<Vec<String>> = None;
    for group in groups.iter().filter(|g| !g.is_empty()) {
        if TABLE_CONSTRAINTS.iter().any(|w| group[0].is_word(w)) {
            if let Some(at) = group.windows(2).position(|w| w[0].is_word("PRIMARY") && w[1].is_word("KEY")) {
                let names = group[at + 2..].iter().take_while(|t| **t != Token::Punct(')')).filter_map(Token::name).map(str::to_string).collect();
                table_key = Some(names);
            }
            continue;
        }
        let name = group[0].name().ok_or("a column has no name")?.to_string();
        let declared: Vec<String> = group[1..].iter().take_while(|t| matches!(t, Token::Word(_)) && !CONSTRAINT_WORDS.iter().any(|w| t.is_word(w))).filter_map(Token::name).map(str::to_ascii_uppercase).collect();
        let declared = declared.join(" ");
        if declared == "INTEGER" && group.windows(2).any(|w| w[0].is_word("PRIMARY") && w[1].is_word("KEY")) {
            alias = Some(columns.len());
        }
        let real = ["REAL", "FLOA", "DOUB"].iter().any(|w| declared.contains(w)) && !declared.contains("INT");
        let default = match group.iter().position(|t| t.is_word("DEFAULT")) {
            Some(at) => default_value(&group[at + 1..]),
            None => SqlValue::Null,
        };
        columns.push(ColumnShape { name, default, real });
        declared_types.push(declared);
    }
    if columns.is_empty() {
        return Err("the table has no column".into());
    }
    // PRIMARY KEY(x) on a single INTEGER column also makes it the rowid.
    if let Some([key]) = table_key.as_deref() {
        if let Some(i) = columns.iter().position(|c| c.name.eq_ignore_ascii_case(key)) {
            if declared_types[i] == "INTEGER" {
                alias = Some(i);
            }
        }
    }
    Ok(Shape { columns, rowid_alias: alias, without_rowid })
}

/// The value after DEFAULT. Expressions and the CURRENT_* times are evaluated by SQLite when a row
/// is inserted, which a reader cannot redo: they read as NULL.
fn default_value(tokens: &[Token]) -> SqlValue {
    let (negative, rest) = match tokens.first() {
        Some(Token::Punct('-')) => (true, &tokens[1..]),
        Some(Token::Punct('+')) => (false, &tokens[1..]),
        _ => (false, tokens),
    };
    match rest.first() {
        Some(Token::Number(text)) => {
            let sign = if negative { -1 } else { 1 };
            let lower = text.to_ascii_lowercase();
            if let Some(hex) = lower.strip_prefix("0x") {
                i64::from_str_radix(hex, 16).map_or(SqlValue::Null, |n| SqlValue::Integer(sign * n))
            } else if let Ok(n) = text.parse::<i64>() {
                SqlValue::Integer(sign * n)
            } else {
                text.parse::<f64>().map_or(SqlValue::Null, |x| SqlValue::Real(sign as f64 * x))
            }
        }
        Some(Token::Str(text)) | Some(Token::Quoted(text)) => SqlValue::Text(text.clone()),
        Some(t) if t.is_word("TRUE") => SqlValue::Integer(1),
        Some(t) if t.is_word("FALSE") => SqlValue::Integer(0),
        _ => SqlValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/synthetic.db");

    fn fixture() -> Database {
        Database::from_bytes(FIXTURE.to_vec()).expect("the synthetic database opens")
    }

    /// Same text as `pattern` in tests/fixtures/make-synthetic-db.mjs.
    fn pattern(length: usize, seed: usize) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        (0..length).map(|k| if k % 100 == 99 { 'é' } else { ALPHABET[(k * 7 + seed) % 36] as char }).collect()
    }

    fn page_kind(db: &Database, number: u32) -> u8 {
        let header = if number == 1 { 100 } else { 0 };
        db.page(number).unwrap()[header]
    }

    fn root(db: &Database, name: &str) -> u32 {
        db.schema().iter().find(|e| e.name == name).unwrap().root
    }

    #[test]
    fn the_fixture_has_the_shapes_under_test() {
        let db = fixture();
        assert_eq!(db.page_size, 4096);
        // The schema spans several pages, so page 1 is an interior page; so are the two big tables' roots.
        assert_eq!(page_kind(&db, 1), 0x05);
        assert_eq!(page_kind(&db, root(&db, "items")), 0x05);
        assert_eq!(page_kind(&db, root(&db, "documents")), 0x05);
        assert_eq!(db.table_names(), ["items", "documents", "pairs", "empty_one", "no_rowid"]);
        assert_eq!(db.schema().iter().filter(|e| e.kind == "view").count(), 30);
        assert!(db.change_counter() > 0);
    }

    #[test]
    fn reads_every_row_with_its_values() {
        let items = fixture().table("ITEMS").unwrap();
        assert_eq!(items.columns, ["id", "label", "amount", "ratio", "flag", "raw", "tag", "rank", "note", "score"]);
        assert_eq!(items.rows.len(), 160);
        assert!(items.rows.windows(2).all(|w| w[0].rowid < w[1].rowid));
        for row in items.rows().take(150) {
            let i = row.rowid();
            assert_eq!(row.integer("id"), Some(i), "the INTEGER PRIMARY KEY reads as the rowid");
            let mut label = format!("item-{i}-{}", "x".repeat(40));
            if i % 10 == 0 {
                label.push_str(" é™ō🎵");
            }
            assert_eq!(row.text("label"), Some(label.as_str()));
            let amount = if i % 50 == 0 { (1 << 40) + i } else if i % 2 == 1 { -i * i } else { i * i };
            assert_eq!(row.integer("amount"), Some(amount));
            assert_eq!(row.get("ratio"), &SqlValue::Real(i as f64 / 4.0), "row {i}");
            assert_eq!(row.integer("flag"), Some(i64::from(i % 3 == 0)));
            let raw = if i % 7 == 0 { SqlValue::Blob(vec![i as u8, i as u8 + 1, i as u8 + 2]) } else { SqlValue::Null };
            assert_eq!(row.get("raw"), &raw);
        }
        assert_eq!(items.rows().next().unwrap().get("no such column"), &SqlValue::Null);
    }

    #[test]
    fn short_records_take_the_declared_defaults() {
        let items = fixture().table("items").unwrap();
        let first = items.rows().next().unwrap();
        assert_eq!(items.rows[0].stored, 6, "row 1 was written before the four added columns");
        assert_eq!(first.text("tag"), Some("none"));
        assert_eq!(first.integer("rank"), Some(-1));
        assert_eq!(first.get("note"), &SqlValue::Null);
        assert_eq!(first.get("score"), &SqlValue::Real(2.5));
        // An updated old row is written again in full.
        let fifth = items.rows().nth(4).unwrap();
        assert_eq!((items.rows[4].stored, fifth.text("tag")), (10, Some("updated")));
        let last = items.rows().last().unwrap();
        assert_eq!(items.rows[159].stored, 10);
        assert_eq!((last.text("tag"), last.integer("rank"), last.text("note")), (Some("tag-160"), Some(10), Some("note 160")));
        assert_eq!(last.get("score"), &SqlValue::Real(-1.25));
        assert_eq!(last.get("ratio"), &SqlValue::Real(0.5));
    }

    #[test]
    fn follows_overflow_chains() {
        let documents = fixture().table("documents").unwrap();
        assert_eq!(documents.columns, ["id", "name", "kind", "body", "extra"]);
        let rows: Vec<RowRef> = documents.rows().collect();
        assert_eq!(rows.len(), 4);
        let expected = [("doc-1", 50, 1), ("doc-2", 5000, 2), ("doc-3", 20000, 3), ("doc-4", 300, 4)];
        for (row, (id, length, seed)) in rows.iter().zip(expected) {
            assert_eq!(row.text("id"), Some(id));
            assert_eq!(row.integer("kind"), Some(seed as i64));
            let body = row.text("body").unwrap();
            assert_eq!(body.chars().count(), length, "{id}");
            assert_eq!(body, pattern(length, seed), "{id}");
        }
        assert_eq!(rows[0].text("extra"), Some(""), "default of a column added later");
        assert_eq!(documents.rows[0].stored, 4);
        assert_eq!(rows[3].text("extra"), Some("x"));

        let pairs = fixture().table("pairs").unwrap();
        let slots: Vec<(i64, &str)> = pairs.rows().map(|r| (r.integer("slot").unwrap(), r.text("doc").unwrap())).collect();
        assert_eq!(slots, [(1, "doc-3"), (2, "doc-1"), (3, "doc-4")]);
        assert!(fixture().table("empty_one").unwrap().rows.is_empty());
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        let db = fixture();
        assert!(db.table("no_rowid").unwrap_err().contains("WITHOUT ROWID"));
        assert!(db.table("padding_view_0").is_err(), "a view is not a table");
        assert!(db.table("missing").is_err());
        assert!(Database::from_bytes(b"not a database at all".to_vec()).is_err());
        let mut utf16 = FIXTURE.to_vec();
        utf16[59] = 2;
        assert!(Database::from_bytes(utf16).unwrap_err().contains("UTF-16"));
        let truncated = FIXTURE[..FIXTURE.len() - 4096].to_vec();
        assert!(Database::from_bytes(truncated).unwrap_err().contains("truncated"));
    }

    #[test]
    fn reads_create_table_statements() {
        let shape = parse_create_table(
            "CREATE TABLE \"configs\" (\n  id TEXT NOT NULL UNIQUE, vad INTEGER NOT NULL, image TEXT NOT NULL DEFAULT '',\n  is_favorite BOOL NOT NULL DEFAULT false, favorite_position INTEGER NOT NULL DEFAULT -1,\n  [odd name] VARCHAR(10) DEFAULT 'it''s', ratio DOUBLE DEFAULT +.5, at DATETIME DEFAULT CURRENT_TIMESTAMP,\n  hex INTEGER DEFAULT 0x10, release_version TEXT DEFAULT NULL, PRIMARY KEY(id))",
        )
        .unwrap();
        let names: Vec<&str> = shape.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "vad", "image", "is_favorite", "favorite_position", "odd name", "ratio", "at", "hex", "release_version"]);
        let defaults: Vec<&SqlValue> = shape.columns.iter().map(|c| &c.default).collect();
        assert_eq!(
            defaults,
            [
                &SqlValue::Null,
                &SqlValue::Null,
                &SqlValue::Text(String::new()),
                &SqlValue::Integer(0),
                &SqlValue::Integer(-1),
                &SqlValue::Text("it's".into()),
                &SqlValue::Real(0.5),
                &SqlValue::Null,
                &SqlValue::Integer(16),
                &SqlValue::Null
            ]
        );
        assert_eq!(shape.rowid_alias, None, "a TEXT primary key is not the rowid");
        assert!(shape.columns[6].real && !shape.columns[1].real);
        let keyed = parse_create_table("CREATE TABLE t (n INTEGER, -- comment, with a comma\n v TEXT, PRIMARY KEY (n)) /* tail */").unwrap();
        assert_eq!((keyed.rowid_alias, keyed.columns.len()), (Some(0), 2));
        assert!(parse_create_table("CREATE TABLE w (k TEXT PRIMARY KEY) WITHOUT ROWID").unwrap().without_rowid);
    }

    #[test]
    fn varints_and_serial_types() {
        assert_eq!(varint(&[0x7F], 0).unwrap(), (127, 1));
        assert_eq!(varint(&[0x81, 0x00], 0).unwrap(), (128, 2));
        assert_eq!(varint(&[0xFF; 9], 0).unwrap(), (u64::MAX, 9));
        assert!(varint(&[0x81], 0).is_err());
        assert_eq!(decode_value(1, &[0xFF], 0).unwrap(), (SqlValue::Integer(-1), 1));
        assert_eq!(decode_value(3, &[0x01, 0x00, 0x00], 0).unwrap(), (SqlValue::Integer(65536), 3));
        assert_eq!(decode_value(5, &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE], 0).unwrap(), (SqlValue::Integer(-2), 6));
        assert_eq!(decode_value(13 + 2 * 3, b"abc", 0).unwrap(), (SqlValue::Text("abc".into()), 3));
        assert!(decode_value(10, &[], 0).is_err());
        assert!(decode_value(13 + 2 * 5, b"abc", 0).is_err());
    }

    /// Reads GG's own database when it is installed: `cargo test --release -- --ignored`.
    #[test]
    #[ignore]
    fn reads_ggs_database_when_present() {
        let path = Path::new(crate::library::DEFAULT_PATH);
        if !path.is_file() {
            eprintln!("{} is absent: nothing to read", path.display());
            return;
        }
        let db = Database::open(path).unwrap();
        let configs = db.table("configs").unwrap();
        let selected = db.table("selected_config").unwrap();
        let short = configs.rows.iter().filter(|r| r.stored < configs.columns.len()).count();
        eprintln!(
            "tables {:?}; configs: {} rows, {} columns, {} short records; selected_config: {} rows",
            db.table_names(),
            configs.rows.len(),
            configs.columns.len(),
            short,
            selected.rows.len()
        );
        assert!(!configs.rows.is_empty());
        assert!(configs.rows().all(|r| r.text("id").is_some() && r.text("data").is_some() && r.text("game_ids").is_some()));
    }
}
