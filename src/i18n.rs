//! Platinum's texts in the user's language: French (the source), English, Spanish, German,
//! Italian and Portuguese.
//!
//! Every text is written in French in the code, and that French text is its key: `tr("Arrêter
//! Platinum")` looks it up in the catalog of the current language and falls back to the French when
//! the catalog has no entry for it, so a text added without its translations still shows. Texts
//! with values use named placeholders, filled after the lookup: `trf!("Micro : {name}", name = …)`.
//!
//! The catalogs are gettext `.po` files embedded in the program (`lang/<code>.po` for the library
//! and the daemon; the window adds its own with `add_catalogs`), read with the small subset of the
//! format they use: `msgid` and `msgstr` with continuation lines, `#` comments, the `fuzzy` flag,
//! `msgctxt` for the rare word with two meanings. The window's catalogs,
//! `ui/lang/<code>/LC_MESSAGES/platinum-ui.po`, hold the texts of its .slint files (bundled by
//! Slint, see ui/build.rs) and of its Rust code.
//!
//! Adding a language: a `Language` variant (code, own name, Windows identifier, decimal mark),
//! `lang/<code>.po` added to `LIBRARY`, `ui/lang/<code>/LC_MESSAGES/platinum-ui.po` added to the
//! window's `WINDOW_CATALOGS`; the coverage tests list what is missing. The command line and
//! daemon.log stay in English: they are for diagnostics.
//!
//! The language is process-wide (the daemon's threads and the window's engine all follow it). It
//! starts as French, so that code that never sets it (the tests) sees the source texts.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// A language Platinum speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Fr,
    En,
    Es,
    De,
    It,
    Pt,
}

impl Language {
    pub const ALL: [Language; 6] = [Language::Fr, Language::En, Language::Es, Language::De, Language::It, Language::Pt];

    /// Its ISO 639-1 code, as platinum.ini and the catalogs' folders name it.
    pub fn code(self) -> &'static str {
        match self {
            Language::Fr => "fr",
            Language::En => "en",
            Language::Es => "es",
            Language::De => "de",
            Language::It => "it",
            Language::Pt => "pt",
        }
    }

    /// The language a code names: "de", "DE", "de-AT", "pt_BR".
    pub fn from_code(code: &str) -> Option<Language> {
        let base = code.trim().split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
        Language::ALL.into_iter().find(|language| language.code() == base)
    }

    /// Its name in itself, as the language menu shows it whatever the current language.
    pub fn name(self) -> &'static str {
        match self {
            Language::Fr => "Français",
            Language::En => "English",
            Language::Es => "Español",
            Language::De => "Deutsch",
            Language::It => "Italiano",
            Language::Pt => "Português",
        }
    }

    /// The language of a Windows language identifier (`LANGID`), by its primary language (the low
    /// ten bits): any French, English, Spanish, German, Italian or Portuguese.
    pub fn from_windows(langid: u16) -> Option<Language> {
        match langid & 0x3ff {
            0x0c => Some(Language::Fr),
            0x09 => Some(Language::En),
            0x0a => Some(Language::Es),
            0x07 => Some(Language::De),
            0x10 => Some(Language::It),
            0x16 => Some(Language::Pt),
            _ => None,
        }
    }

    /// Whether numbers are written with a decimal comma ("3,5 dB"); only English uses a point.
    pub fn decimal_comma(self) -> bool {
        self != Language::En
    }

    fn index(self) -> usize {
        Language::ALL.iter().position(|language| *language == self).unwrap_or(0)
    }
}

/// platinum.ini's `language`: Windows' display language (when Platinum speaks it, else English),
/// or one language.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Choice {
    #[default]
    Auto,
    Fixed(Language),
}

impl Choice {
    /// "auto", or a language's code.
    pub fn key(self) -> &'static str {
        match self {
            Choice::Auto => "auto",
            Choice::Fixed(language) => language.code(),
        }
    }

    pub fn parse(text: &str) -> Option<Choice> {
        match text.trim() {
            "" => Some(Choice::Auto),
            text if text.eq_ignore_ascii_case("auto") => Some(Choice::Auto),
            text => Language::from_code(text).map(Choice::Fixed),
        }
    }

    /// The language to speak.
    pub fn resolve(self) -> Language {
        match self {
            Choice::Auto => windows_language().unwrap_or(Language::En),
            Choice::Fixed(language) => language,
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

/// Windows' display language for this user, when Platinum speaks it.
pub fn windows_language() -> Option<Language> {
    Language::from_windows(unsafe { GetUserDefaultUILanguage() })
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Makes `language` the one every later `tr` speaks, in this whole process.
pub fn set_language(language: Language) {
    CURRENT.store(language.index() as u8, Ordering::SeqCst);
}

pub fn language() -> Language {
    Language::ALL.get(CURRENT.load(Ordering::SeqCst) as usize).copied().unwrap_or(Language::Fr)
}

/// The catalogs of a program part: one `.po` text per language (French has none: it is the source).
pub type Sources = &'static [(Language, &'static str)];

/// The library's and the daemon's catalogs.
pub const LIBRARY: Sources = &[
    (Language::En, include_str!("../lang/en.po")),
    (Language::Es, include_str!("../lang/es.po")),
    (Language::De, include_str!("../lang/de.po")),
    (Language::It, include_str!("../lang/it.po")),
    (Language::Pt, include_str!("../lang/pt.po")),
];

/// Catalogs a program adds to the library's (the window's own texts), looked up first.
static EXTRA: OnceLock<Sources> = OnceLock::new();

/// Adds a program's own catalogs; the first call wins (a program has one set).
pub fn add_catalogs(sources: Sources) {
    let _ = EXTRA.set(sources);
}

type Catalog = HashMap<&'static str, &'static str>;

/// Each catalog parsed on first use, per language. The texts are kept for the life of the process.
fn catalog(slots: &'static [OnceLock<Catalog>; 6], sources: Sources, language: Language) -> &'static Catalog {
    slots[language.index()].get_or_init(|| {
        let text = sources.iter().find(|(l, _)| *l == language).map_or("", |(_, text)| *text);
        parse_po(text).into_iter().map(|(id, text)| (&*Box::leak(id.into_boxed_str()), &*Box::leak(text.into_boxed_str()))).collect()
    })
}

static LIBRARY_PARSED: [OnceLock<Catalog>; 6] = [const { OnceLock::new() }; 6];
static EXTRA_PARSED: [OnceLock<Catalog>; 6] = [const { OnceLock::new() }; 6];

/// `source` in the current language: its translation, or `source` itself when there is none.
pub fn tr(source: &str) -> &str {
    tr_in(language(), source)
}

/// `source` in `language`.
pub fn tr_in(language: Language, source: &str) -> &str {
    if language == Language::Fr {
        return source;
    }
    if let Some(extra) = EXTRA.get() {
        if let Some(text) = catalog(&EXTRA_PARSED, extra, language).get(source) {
            return text;
        }
    }
    catalog(&LIBRARY_PARSED, LIBRARY, language).get(source).copied().unwrap_or(source)
}

/// Marks a text for translation where it cannot be translated yet (a constant): it is translated
/// with `tr` where it is shown, and the catalogs' coverage test finds it.
pub const fn mark(source: &'static str) -> &'static str {
    source
}

/// A template with its `{name}` placeholders filled; a placeholder without a value stays as it is.
pub fn fill(template: &str, values: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}').map(|close| (&after[..close], close)) {
            Some((name, close)) if values.iter().any(|(n, _)| *n == name) => {
                if let Some((_, value)) = values.iter().find(|(n, _)| *n == name) {
                    out.push_str(&value.to_string());
                }
                rest = &after[close + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `tr` then `fill`: `trf!("Micro : {name}", name = device)`.
#[macro_export]
macro_rules! trf {
    ($source:literal $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::i18n::fill($crate::i18n::tr($source), &[$((stringify!($name), &$value as &dyn ::std::fmt::Display)),*])
    };
}

/// A message the daemon wrote in French (status.txt), in the current language: a known text, or a
/// known text, " : ", then a detail (a device's name, a technical error) kept as it is. Anything
/// else stays as it was written.
pub fn tr_message(message: &str) -> String {
    let whole = tr(message);
    if whole != message || language() == Language::Fr {
        return whole.to_string();
    }
    match message.split_once(" : ") {
        Some((head, detail)) if tr(head) != head => fill(tr("{text} : {detail}"), &[("text", &tr(head)), ("detail", &detail)]),
        _ => message.to_string(),
    }
}

/// A label as it reads inside a sentence, after the first item of a list ("Effets du micro et
/// volume du micro"): its first letter in lower case, except in German, whose nouns keep theirs.
pub fn in_sentence(label: &str) -> String {
    if language() == Language::De {
        return label.to_string();
    }
    let mut chars = label.chars();
    chars.next().map_or_else(String::new, |first| first.to_lowercase().chain(chars).collect())
}

/// A number written by Rust ("3.5") as the current language writes it ("3,5").
pub fn decimal(text: String) -> String {
    if language().decimal_comma() { text.replace('.', ",") } else { text }
}

/// The translated entries of a `.po` text, as (source, translation): entries without a
/// translation, marked fuzzy, or with plural forms are left out, as is the header.
pub fn parse_po(text: &str) -> Vec<(String, String)> {
    /// The entry being read: its context, source and translation, whether it is fuzzy or has
    /// plural forms, and which part the continuation lines belong to (1 context, 2 source,
    /// 3 translation).
    #[derive(Default)]
    struct Entry {
        context: Option<String>,
        id: Option<String>,
        text: Option<String>,
        fuzzy: bool,
        plural: bool,
        field: u8,
    }
    let mut entries = Vec::new();
    let mut entry = Entry::default();
    let mut finish = |entry: &mut Entry| {
        let done = std::mem::take(entry);
        if let (Some(source), Some(translation)) = (done.id, done.text) {
            if !source.is_empty() && !translation.is_empty() && !done.fuzzy && !done.plural {
                entries.push((with_context(done.context.as_deref(), &source), translation));
            }
        }
    };
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            if entry.id.is_some() || entry.text.is_some() || entry.context.is_some() {
                finish(&mut entry);
            }
            if comment.starts_with(',') && comment.contains("fuzzy") {
                entry.fuzzy = true;
            }
            entry.field = 0;
            continue;
        }
        if let Some(rest) = line.strip_prefix("msgctxt ") {
            if entry.id.is_some() || entry.text.is_some() {
                finish(&mut entry);
            }
            entry.context = Some(unquote(rest));
            entry.field = 1;
        } else if let Some(rest) = line.strip_prefix("msgid ") {
            // An entry right after another, with no comment between them: the flag was the
            // previous entry's.
            if entry.id.is_some() || entry.text.is_some() {
                finish(&mut entry);
            }
            entry.id = Some(unquote(rest));
            entry.field = 2;
        } else if let Some(rest) = line.strip_prefix("msgstr ") {
            entry.text = Some(unquote(rest));
            entry.field = 3;
        } else if line.starts_with("msgid_plural") || line.starts_with("msgstr[") {
            // Not used by Platinum's catalogs: the entry is skipped.
            entry.plural = true;
            entry.field = 0;
        } else if line.starts_with('"') {
            let more = unquote(line);
            match entry.field {
                1 => entry.context.get_or_insert_with(String::new).push_str(&more),
                2 => entry.id.get_or_insert_with(String::new).push_str(&more),
                3 => entry.text.get_or_insert_with(String::new).push_str(&more),
                _ => {}
            }
        }
    }
    finish(&mut entry);
    entries
}

/// The key of a text with a context (gettext's `msgctxt`, Slint's `@tr("context" => "text")`):
/// the context, the EOT character, then the text, as gettext itself joins them. Only the window's
/// .slint texts use contexts, for one French word with two meanings ("Enregistrer": save, or
/// record).
pub fn with_context(context: Option<&str>, text: &str) -> String {
    match context {
        Some(context) => format!("{context}\u{4}{text}"),
        None => text.to_string(),
    }
}

/// A `.po` string's content: the quotes taken off, the escapes read.
fn unquote(quoted: &str) -> String {
    let inner = quoted.trim();
    let inner = inner.strip_prefix('"').unwrap_or(inner);
    let inner = inner.strip_suffix('"').unwrap_or(inner);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// A text as a `.po` string, quotes included.
pub fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The placeholders of a text, sorted: `{name}` by name, Slint's `{}` and `{0}` by position (the
/// n-th `{}` counts as `{n}`), so that a translation may reorder them. `{{` and `}}` are literal
/// braces.
pub fn placeholders(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut next = 0;
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        if after.starts_with('{') {
            rest = &after[1..];
            continue;
        }
        let Some(close) = after.find('}') else { break };
        let name = &after[..close];
        if name.is_empty() {
            found.push(next.to_string());
            next += 1;
        } else {
            found.push(name.to_string());
        }
        rest = &after[close + 1..];
    }
    found.sort();
    found
}

/// The texts marked for translation in source code: the string literal right after each of
/// `markers` (`tr("`, `trf!("`, `@tr("`…), with Rust's and Slint's escapes read. For the catalogs'
/// coverage tests of the library and the window. Slint's `@tr("context" => "text")` gives the
/// text with its context (`with_context`).
pub fn marked_texts(code: &str, markers: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    for marker in markers {
        let mut rest = code;
        while let Some(at) = rest.find(marker) {
            // `xtr("` is not `tr("`, nor is Slint's `@tr("`: the marker must start a word.
            let starts_word = rest[..at].chars().next_back().is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '@'));
            rest = &rest[at + marker.len()..];
            if !starts_word {
                continue;
            }
            let Some((mut text, after)) = read_literal(rest) else { break };
            rest = after;
            let context = rest.trim_start().strip_prefix("=>").map(str::trim_start).and_then(|r| r.strip_prefix('"')).and_then(read_literal);
            if let Some((inner, after)) = context {
                text = with_context(Some(&text), &inner);
                rest = after;
            }
            if !found.contains(&text) {
                found.push(text);
            }
        }
    }
    found
}

/// A string literal's content, its opening quote already passed, with Rust's and Slint's escapes
/// read, and the code after its closing quote; `None` when it does not close.
fn read_literal(code: &str) -> Option<(String, &str)> {
    let mut text = String::new();
    let mut chars = code.char_indices();
    while let Some((index, c)) = chars.next() {
        match c {
            '"' => return Some((text, &code[index + 1..])),
            '\\' => match chars.next().map(|(_, c)| c) {
                Some('n') => text.push('\n'),
                Some('t') => text.push('\t'),
                // A Rust line continuation: the line break and the next line's indentation are
                // not part of the text.
                Some('\n') => {
                    let skipped = chars.clone().take_while(|(_, c)| c.is_whitespace()).count();
                    for _ in 0..skipped {
                        chars.next();
                    }
                }
                Some(other) => text.push(other),
                None => {}
            },
            other => text.push(other),
        }
    }
    None
}

/// What is wrong with a catalog for these source texts: the ones it does not translate, and the
/// ones whose placeholders differ from the source's.
pub fn catalog_problems(sources: &[String], catalog_text: &str) -> (Vec<String>, Vec<String>) {
    let entries: HashMap<String, String> = parse_po(catalog_text).into_iter().collect();
    let mut missing = Vec::new();
    let mut mismatched = Vec::new();
    for source in sources {
        match entries.get(source) {
            None => missing.push(source.clone()),
            Some(translation) if placeholders(translation) != placeholders(source) => mismatched.push(format!("{source} -> {translation}")),
            Some(_) => {}
        }
    }
    (missing, mismatched)
}

/// The catalog entries whose placeholders differ from their source's, whatever the code uses.
pub fn placeholder_problems(catalog_text: &str) -> Vec<String> {
    parse_po(catalog_text).into_iter().filter(|(source, text)| placeholders(source) != placeholders(text)).map(|(source, text)| format!("{source} -> {text}")).collect()
}

/// Missing entries written as a `.po` fragment, ready to be translated (for a failing test's
/// message).
pub fn po_fragment(missing: &[String]) -> String {
    missing
        .iter()
        .map(|source| match source.split_once('\u{4}') {
            Some((context, text)) => format!("msgctxt {}\nmsgid {}\nmsgstr \"\"\n\n", quote(context), quote(text)),
            None => format!("msgid {}\nmsgstr \"\"\n\n", quote(source)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_have_codes_names_and_windows_ids() {
        for language in Language::ALL {
            assert_eq!(Language::from_code(language.code()), Some(language));
            assert_eq!(Choice::parse(language.code()), Some(Choice::Fixed(language)));
            assert_eq!(Choice::parse(Choice::Fixed(language).key()), Some(Choice::Fixed(language)));
        }
        assert_eq!(Language::from_code("pt_BR"), Some(Language::Pt));
        assert_eq!(Language::from_code("DE-at"), Some(Language::De));
        assert_eq!(Language::from_code("nl"), None);
        assert_eq!(Choice::parse(" Auto "), Some(Choice::Auto));
        assert_eq!(Choice::parse(""), Some(Choice::Auto));
        assert_eq!(Choice::parse("klingon"), None);
        assert_eq!(Choice::default().key(), "auto");
        // fr-FR, fr-CA, en-US, en-GB, es-ES, es-MX, de-DE, de-CH, it-IT, pt-BR, pt-PT; ja-JP, nl-NL.
        let ids = [(0x040c, Some(Language::Fr)), (0x0c0c, Some(Language::Fr)), (0x0409, Some(Language::En)), (0x0809, Some(Language::En)), (0x0c0a, Some(Language::Es)), (0x080a, Some(Language::Es)), (0x0407, Some(Language::De)), (0x0807, Some(Language::De)), (0x0410, Some(Language::It)), (0x0416, Some(Language::Pt)), (0x0816, Some(Language::Pt)), (0x0411, None), (0x0413, None)];
        for (id, language) in ids {
            assert_eq!(Language::from_windows(id), language, "{id:#06x}");
        }
        assert_eq!(Choice::Fixed(Language::It).resolve(), Language::It);
        assert!(Language::Fr.decimal_comma() && Language::De.decimal_comma() && !Language::En.decimal_comma());
        assert_eq!(Language::ALL.map(Language::name), ["Français", "English", "Español", "Deutsch", "Italiano", "Português"]);
    }

    #[test]
    fn po_entries_are_read_with_their_escapes() {
        let text = "# header\nmsgid \"\"\nmsgstr \"\"\n\"Content-Type: text/plain; charset=UTF-8\\n\"\n\n#: src/tray.rs\nmsgid \"Ouvrir Platinum\"\nmsgstr \"Open Platinum\"\n\nmsgid \"\"\n\"Deux \"\n\"lignes\"\nmsgstr \"Two \"\n\"lines\"\n\n#, fuzzy\nmsgid \"Douteux\"\nmsgstr \"Doubtful\"\n\nmsgid \"Vide\"\nmsgstr \"\"\n\nmsgid \"Dit « \\\"oui\\\" »\\n\"\nmsgstr \"Says \\\"yes\\\"\\n\"\n";
        let entries = parse_po(text);
        assert_eq!(entries, [("Ouvrir Platinum".to_string(), "Open Platinum".to_string()), ("Deux lignes".into(), "Two lines".into()), ("Dit « \"oui\" »\n".into(), "Says \"yes\"\n".into())]);
        assert_eq!(unquote(&quote("a \"b\" \\ c\n")), "a \"b\" \\ c\n");
        assert!(parse_po("").is_empty());
    }

    #[test]
    fn templates_are_filled_by_name() {
        let n = 3;
        assert_eq!(fill("{device} : {count} canaux", &[("device", &"Arena 7"), ("count", &n)]), "Arena 7 : 3 canaux");
        assert_eq!(fill("{a}{a} {b}", &[("a", &1)]), "11 {b}");
        assert_eq!(fill("ni { ni }", &[("x", &1)]), "ni { ni }");
        assert_eq!(fill("", &[]), "");
        // The macro, in French (the language every test sees).
        let name = "Seiren";
        assert_eq!(crate::trf!("Micro : {name}", name = name), "Micro : Seiren");
    }

    #[test]
    fn placeholders_match_by_name_and_position() {
        assert_eq!(placeholders("{name} et {count}"), ["count", "name"]);
        assert_eq!(placeholders("{} puis {}"), ["0", "1"]);
        assert_eq!(placeholders("{1} then {0}"), placeholders("{} puis {}"));
        assert_eq!(placeholders("{{littéral}}"), Vec::<String>::new());
        assert!(placeholders("aucun").is_empty());
    }

    #[test]
    fn marked_texts_are_found_in_code() {
        let code = concat!("let a = tr(\"Un\");\nlet b = trf!(\"Deux {x}\", x = 1);\nlet c = xtr(\"Pas moi\");\nlet d = tr(\"Dit \\\"oui\\\"\\n\");\n", "text: @tr(\"Trois\");\nlet e = tr(\"Un\");\n");
        assert_eq!(marked_texts(code, &["tr(\"", "trf!(\""]), ["Un", "Dit \"oui\"\n", "Deux {x}"]);
        assert_eq!(marked_texts(code, &["@tr(\""]), ["Trois"]);
        let catalog = "msgid \"Un\"\nmsgstr \"One\"\n\nmsgid \"Deux {x}\"\nmsgstr \"Two {y}\"\n";
        let (missing, mismatched) = catalog_problems(&["Un".into(), "Deux {x}".into(), "Trois".into()], catalog);
        assert_eq!(missing, ["Trois"]);
        assert_eq!(mismatched, ["Deux {x} -> Two {y}"]);
        assert_eq!(po_fragment(&missing), "msgid \"Trois\"\nmsgstr \"\"\n\n");
        // One French word, two meanings: Slint's context tells them apart, as gettext's msgctxt.
        let slint = "text: @tr(\"Enregistrer\");\ntext: phase == 1 ? @tr(\"test du micro\" => \"Arrêter\") : @tr(\"test du micro\" =>\n    \"Enregistrer\");\n";
        let texts = marked_texts(slint, &["@tr(\""]);
        assert_eq!(texts, ["Enregistrer", "test du micro\u{4}Arrêter", "test du micro\u{4}Enregistrer"]);
        let catalog = "msgid \"Enregistrer\"\nmsgstr \"Save\"\n\nmsgctxt \"test du micro\"\nmsgid \"Enregistrer\"\nmsgstr \"Record\"\n\n#, fuzzy\nmsgctxt \"test du micro\"\nmsgid \"Arrêter\"\nmsgstr \"Stop\"\n";
        assert_eq!(parse_po(catalog), [("Enregistrer".to_string(), "Save".to_string()), ("test du micro\u{4}Enregistrer".into(), "Record".into())]);
        let (missing, _) = catalog_problems(&texts, catalog);
        assert_eq!(po_fragment(&missing), "msgctxt \"test du micro\"\nmsgid \"Arrêter\"\nmsgstr \"\"\n\n");
    }

    #[test]
    fn french_is_the_source_and_the_fallback() {
        assert_eq!(tr_in(Language::Fr, "Ouvrir Platinum"), "Ouvrir Platinum");
        assert_eq!(tr_in(Language::De, "Un texte que personne n'a traduit"), "Un texte que personne n'a traduit");
        // Nothing in the tests sets the language: every other test reads French.
        assert_eq!(language(), Language::Fr);
    }

    /// Every text the library and the daemon mark is in every catalog, with the same placeholders.
    #[test]
    fn every_catalog_translates_every_marked_text() {
        let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&folder).unwrap().flatten() {
            let path = entry.path();
            // This module's own examples are not texts of Platinum.
            if path.extension().is_some_and(|e| e == "rs") && path.file_name().is_some_and(|n| n != "i18n.rs") {
                let code = std::fs::read_to_string(&path).unwrap();
                for text in marked_texts(&code, &["tr(\"", "trf!(\"", "mark(\""]) {
                    if !sources.contains(&text) {
                        sources.push(text);
                    }
                }
            }
        }
        assert!(sources.len() > 20, "{} texts found", sources.len());
        let mut report = Vec::new();
        for (language, text) in LIBRARY {
            let (missing, mismatched) = catalog_problems(&sources, text);
            let wrong = placeholder_problems(text);
            if !missing.is_empty() || !mismatched.is_empty() || !wrong.is_empty() {
                report.push(format!("lang/{}.po: {} of {} texts missing, placeholders differ in {:?}\n{}", language.code(), missing.len(), sources.len(), wrong, po_fragment(&missing)));
            }
        }
        assert!(report.is_empty(), "{}", report.join("\n"));
    }
}
