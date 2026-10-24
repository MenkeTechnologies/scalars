//! `java.util.Locale`'s identity, text forms and English display names,
//! ported from JDK 21: `java.util.Locale`, and `sun.util.locale`'s
//! `BaseLocale`, `LanguageTag`, `InternalLocaleBuilder`, `LocaleExtensions`
//! and `UnicodeLocaleExtension`.
//!
//! A locale is its base (language, script, region, variant) plus its
//! extensions. The extensions are what the legacy constructor adds for the two
//! compatibility locales — `new Locale("ja", "JP", "JP")` carries
//! `u-ca-japanese` and `new Locale("th", "TH", "TH")` `u-nu-thai` — and what a
//! BCP 47 tag spells out, and they show in `toString`
//! (`ja_JP_JP_#u-ca-japanese`), `toLanguageTag`, `equals` and the display name
//! (`Japanese (Japan, JP, Japanese Calendar)`).

use crate::locale_names as names;
use std::collections::BTreeMap;

/// A `java.util.Locale`. The fields hold what the JDK stores: the language
/// lower-cased and mapped off the obsolete ISO codes, the script title-cased,
/// the region upper-cased, the variant as given, and each extension's value
/// lower-cased under its lower-cased singleton, in singleton order — `x`
/// (private use) with its `lvariant` part already moved into the variant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JLocale {
    pub language: String,
    pub script: String,
    pub region: String,
    pub variant: String,
    pub extensions: BTreeMap<char, String>,
}

/// The private-use subtag that introduces a JDK variant inside a tag.
const LVARIANT: &str = "lvariant";

fn is_alpha(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphabetic())
}

fn is_alnum(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn is_language(s: &str) -> bool {
    (2..=8).contains(&s.len()) && is_alpha(s)
}

fn is_extlang(s: &str) -> bool {
    s.len() == 3 && is_alpha(s)
}

fn is_script(s: &str) -> bool {
    s.len() == 4 && is_alpha(s)
}

fn is_region(s: &str) -> bool {
    (s.len() == 2 && is_alpha(s)) || (s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()))
}

fn is_variant(s: &str) -> bool {
    match s.len() {
        5..=8 => is_alnum(s),
        4 => s.as_bytes()[0].is_ascii_digit() && is_alnum(s),
        _ => false,
    }
}

fn is_extension_singleton(s: &str) -> bool {
    s.len() == 1 && is_alpha(s) && !s.eq_ignore_ascii_case("x")
}

fn is_extension_subtag(s: &str) -> bool {
    (2..=8).contains(&s.len()) && is_alnum(s)
}

fn is_privateuse_subtag(s: &str) -> bool {
    (1..=8).contains(&s.len()) && is_alnum(s)
}

fn is_unicode_attribute(s: &str) -> bool {
    (3..=8).contains(&s.len()) && is_alnum(s)
}

fn is_unicode_key(s: &str) -> bool {
    s.len() == 2 && is_alnum(s)
}

/// `LocaleUtils.toTitleString`.
fn title(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut cs = lower.chars();
    match cs.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + cs.as_str(),
        None => String::new(),
    }
}

/// `BaseLocale.convertOldISOCodes`: the current code for one that changed.
fn convert_old_iso(language: &str) -> String {
    match language {
        "he" | "iw" => "he",
        "id" | "in" => "id",
        "yi" | "ji" => "yi",
        l => l,
    }
    .to_string()
}

/// `Locale.getCompatibilityExtensions`: the Unicode extension the legacy
/// `ja_JP_JP` and `th_TH_TH` locales imply.
fn compat_extensions(
    lang: &str,
    script: &str,
    region: &str,
    variant: &str,
) -> BTreeMap<char, String> {
    let mut ext = BTreeMap::new();
    if script.is_empty() {
        if lang.eq_ignore_ascii_case("ja") && region.eq_ignore_ascii_case("jp") && variant == "JP" {
            ext.insert('u', "ca-japanese".to_string());
        } else if lang.eq_ignore_ascii_case("th")
            && region.eq_ignore_ascii_case("th")
            && variant == "TH"
        {
            ext.insert('u', "nu-thai".to_string());
        }
    }
    ext
}

/// `UnicodeLocaleExtension`'s value from its parts: the attributes, then each
/// key with its type, all lower-cased and in sorted order.
fn unicode_value(attributes: &[String], keywords: &[(String, String)]) -> String {
    let attrs: std::collections::BTreeSet<String> =
        attributes.iter().map(|a| a.to_ascii_lowercase()).collect();
    let kws: BTreeMap<String, String> = keywords
        .iter()
        .map(|(k, t)| (k.to_ascii_lowercase(), t.to_ascii_lowercase()))
        .collect();
    let mut parts: Vec<String> = attrs.into_iter().collect();
    for (k, t) in kws {
        parts.push(k);
        if !t.is_empty() {
            parts.push(t);
        }
    }
    parts.join("-")
}

/// A Unicode extension's attributes and its `(key, type)` keywords.
type UnicodeParts = (Vec<String>, Vec<(String, String)>);

/// `InternalLocaleBuilder.setUnicodeLocaleExtension`: the attributes, then the
/// keywords — a key and the type subtags up to the next key, a repeated key
/// (compared without case) ignored with its type.
fn parse_unicode(subtags: &str) -> UnicodeParts {
    let toks: Vec<&str> = subtags.split('-').collect();
    let mut i = 0;
    let mut attributes: Vec<String> = Vec::new();
    while i < toks.len() && is_unicode_attribute(toks[i]) {
        if !attributes.iter().any(|a| a.eq_ignore_ascii_case(toks[i])) {
            attributes.push(toks[i].to_string());
        }
        i += 1;
    }
    let mut keywords: Vec<(String, String)> = Vec::new();
    let mut key: Option<&str> = None;
    let mut ty: Vec<&str> = Vec::new();
    let seen =
        |kws: &[(String, String)], k: &str| kws.iter().any(|(x, _)| x.eq_ignore_ascii_case(k));
    while i < toks.len() {
        let t = toks[i];
        if is_unicode_key(t) {
            if let Some(k) = key.take() {
                keywords.push((k.to_string(), ty.join("-")));
            }
            ty.clear();
            key = (!seen(&keywords, t)).then_some(t);
        } else if key.is_some() {
            ty.push(t);
        }
        i += 1;
    }
    if let Some(k) = key {
        keywords.push((k.to_string(), ty.join("-")));
    }
    (attributes, keywords)
}

/// `InternalLocaleBuilder.removePrivateuseVariant`: a private-use value with
/// its `lvariant` part cut off — `None` when nothing else is left. A trailing
/// `lvariant` with no subtags after it is kept.
fn remove_privateuse_variant(value: &str) -> Option<String> {
    let toks: Vec<&str> = value.split('-').collect();
    let at = toks.iter().position(|t| t.eq_ignore_ascii_case(LVARIANT));
    match at {
        Some(i) if i + 1 < toks.len() => (i > 0).then(|| toks[..i].join("-")),
        _ => Some(value.to_string()),
    }
}

/// The subtags of a parsed BCP 47 tag (`LanguageTag`).
#[derive(Default)]
struct LanguageTag {
    language: String,
    extlangs: Vec<String>,
    script: String,
    region: String,
    variants: Vec<String>,
    extensions: Vec<String>,
    privateuse: String,
}

/// `LanguageTag.LEGACY`: the grandfathered tags and what they parse as.
fn legacy_tag(tag: &str) -> Option<&'static str> {
    Some(match tag.to_ascii_lowercase().as_str() {
        "art-lojban" => "jbo",
        "cel-gaulish" => "xtg-x-cel-gaulish",
        "en-gb-oed" => "en-GB-x-oed",
        "i-ami" => "ami",
        "i-bnn" => "bnn",
        "i-default" => "en-x-i-default",
        "i-enochian" => "und-x-i-enochian",
        "i-hak" => "hak",
        "i-klingon" => "tlh",
        "i-lux" => "lb",
        "i-mingo" => "see-x-i-mingo",
        "i-navajo" => "nv",
        "i-pwn" => "pwn",
        "i-tao" => "tao",
        "i-tay" => "tay",
        "i-tsu" => "tsu",
        "no-bok" => "nb",
        "no-nyn" => "nn",
        "sgn-be-fr" => "sfb",
        "sgn-be-nl" => "vgt",
        "sgn-ch-de" => "sgg",
        "zh-guoyu" => "cmn",
        "zh-hakka" => "hak",
        "zh-min" => "nan-x-zh-min",
        "zh-min-nan" => "nan",
        "zh-xiang" => "hsn",
        _ => return None,
    })
}

/// `LanguageTag.parse`: as many well-formed subtags as lead the tag, in
/// order; parsing stops at the first one out of place, and an extension or
/// private use with no subtags after its singleton ends it.
fn parse_tag(input: &str) -> LanguageTag {
    let source = legacy_tag(input).unwrap_or(input);
    let toks: Vec<&str> = source.split('-').collect();
    let mut i = 0;
    let mut tag = LanguageTag::default();
    let mut error = false;
    let cur = |i: usize| toks.get(i).copied();
    if let Some(s) = cur(i).filter(|s| is_language(s)) {
        tag.language = s.to_string();
        i += 1;
        while let Some(s) = cur(i).filter(|s| is_extlang(s)) {
            tag.extlangs.push(s.to_string());
            i += 1;
            if tag.extlangs.len() == 3 {
                break;
            }
        }
        if let Some(s) = cur(i).filter(|s| is_script(s)) {
            tag.script = s.to_string();
            i += 1;
        }
        if let Some(s) = cur(i).filter(|s| is_region(s)) {
            tag.region = s.to_string();
            i += 1;
        }
        while let Some(s) = cur(i).filter(|s| is_variant(s)) {
            tag.variants.push(s.to_string());
            i += 1;
        }
        while let Some(singleton) = cur(i).filter(|s| is_extension_singleton(s)) {
            let mut ext = singleton.to_string();
            i += 1;
            let mut subtags = 0;
            while let Some(s) = cur(i).filter(|s| is_extension_subtag(s)) {
                ext.push('-');
                ext.push_str(s);
                subtags += 1;
                i += 1;
            }
            if subtags == 0 {
                error = true;
                break;
            }
            tag.extensions.push(ext);
        }
    }
    if !error {
        if let Some(x) = cur(i).filter(|s| s.eq_ignore_ascii_case("x")) {
            let mut pu = x.to_string();
            i += 1;
            let mut subtags = 0;
            while let Some(s) = cur(i).filter(|s| is_privateuse_subtag(s)) {
                pu.push('-');
                pu.push_str(s);
                subtags += 1;
                i += 1;
            }
            if subtags > 0 {
                tag.privateuse = pu;
            }
        }
    }
    tag
}

impl JLocale {
    /// `new Locale(language, country, variant)` / `Locale.of(…)`: no
    /// syntax checks, the compatibility extensions added.
    pub fn of(language: &str, country: &str, variant: &str) -> JLocale {
        JLocale {
            language: convert_old_iso(&language.to_ascii_lowercase()),
            script: String::new(),
            region: country.to_ascii_uppercase(),
            variant: variant.to_string(),
            extensions: compat_extensions(language, "", country, variant),
        }
    }

    /// `Locale.forLanguageTag`: the tag parsed and built as
    /// `InternalLocaleBuilder.setLanguageTag` builds it — an extlang standing
    /// for the language, `und` for none, the variants joined with `_`, a
    /// private-use `lvariant` sequence appended to the variant.
    pub fn for_language_tag(input: &str) -> JLocale {
        let tag = parse_tag(input);
        let language = match tag.extlangs.first() {
            Some(e) => e.clone(),
            None if tag.language.eq_ignore_ascii_case("und") => String::new(),
            None => tag.language.clone(),
        };
        let mut variant = tag.variants.join("_");
        // `setExtensions`: the first of a repeated singleton wins; `u`'s value
        // is canonicalized, every other one kept as written.
        let mut others: Vec<(char, String)> = Vec::new();
        let mut unicode: Option<UnicodeParts> = None;
        for ext in &tag.extensions {
            let key = ext.as_bytes()[0].to_ascii_lowercase() as char;
            if key == 'u' {
                if unicode.is_none() {
                    unicode = Some(parse_unicode(&ext[2..]));
                }
            } else if !others.iter().any(|(k, _)| *k == key) {
                others.push((key, ext[2..].to_string()));
            }
        }
        if !tag.privateuse.is_empty() {
            let pu = &tag.privateuse[2..];
            // `getBaseLocale`: what follows `lvariant` is the JDK variant.
            let toks: Vec<&str> = pu.split('-').collect();
            if let Some(i) = toks.iter().position(|t| t.eq_ignore_ascii_case(LVARIANT)) {
                if i + 1 < toks.len() {
                    if !variant.is_empty() {
                        variant.push('_');
                    }
                    variant.push_str(&toks[i + 1..].join("_"));
                }
            }
            others.push(('x', pu.to_string()));
        }
        let mut extensions = BTreeMap::new();
        for (k, v) in others {
            let v = if k == 'x' {
                match remove_privateuse_variant(&v) {
                    Some(v) => v,
                    None => continue,
                }
            } else {
                v
            };
            extensions.insert(k, v.to_ascii_lowercase());
        }
        if let Some((attrs, kws)) = unicode {
            let v = unicode_value(&attrs, &kws);
            if !v.is_empty() {
                extensions.insert('u', v);
            }
        }
        let language = language.to_ascii_lowercase();
        let mut loc = JLocale {
            language: if language.is_empty() {
                language
            } else {
                convert_old_iso(&language)
            },
            script: title(&tag.script),
            region: tag.region.to_ascii_uppercase(),
            variant,
            extensions,
        };
        if loc.extensions.is_empty() && !loc.variant.is_empty() {
            loc.extensions =
                compat_extensions(&loc.language, &loc.script, &loc.region, &loc.variant);
        }
        loc
    }

    /// `LocaleExtensions.getID`: each extension as `singleton-value` in
    /// singleton order, private use last.
    pub fn extension_id(&self) -> String {
        let mut parts: Vec<String> = self
            .extensions
            .iter()
            .filter(|(k, _)| **k != 'x')
            .map(|(k, v)| format!("{k}-{v}"))
            .collect();
        if let Some(v) = self.extensions.get(&'x') {
            parts.push(format!("x-{v}"));
        }
        parts.join("-")
    }

    /// `Locale.toString`.
    pub fn to_jstring(&self) -> String {
        let l = !self.language.is_empty();
        let s = !self.script.is_empty();
        let r = !self.region.is_empty();
        let v = !self.variant.is_empty();
        let ext = self.extension_id();
        let e = !ext.is_empty();
        let mut out = self.language.clone();
        if r || (l && (v || s || e)) {
            out.push('_');
            out.push_str(&self.region);
        }
        if v && (l || r) {
            out.push('_');
            out.push_str(&self.variant);
        }
        if s && (l || r) {
            out.push_str("_#");
            out.push_str(&self.script);
        }
        if e && (l || r) {
            out.push('_');
            if !s {
                out.push('#');
            }
            out.push_str(&ext);
        }
        out
    }

    /// `Locale.toLanguageTag`, through `LanguageTag.parseLocale`: the
    /// well-formed subtags in place, an ill-formed variant moved into private
    /// use behind `lvariant`, `und` for a missing language, and `no_NO_NY`
    /// spelled `nn-NO`.
    pub fn to_language_tag(&self) -> String {
        let mut language = String::new();
        if is_language(&self.language) {
            language = match self.language.as_str() {
                "iw" => "he",
                "ji" => "yi",
                "in" => "id",
                l => l,
            }
            .to_string();
        }
        let mut has_subtag = false;
        let script = if is_script(&self.script) {
            has_subtag = true;
            title(&self.script)
        } else {
            String::new()
        };
        let region = if is_region(&self.region) {
            has_subtag = true;
            self.region.to_ascii_uppercase()
        } else {
            String::new()
        };
        let mut variant = self.variant.as_str();
        if language == "no" && region == "NO" && variant == "NY" {
            language = "nn".to_string();
            variant = "";
        }
        let mut variants: Vec<&str> = Vec::new();
        let mut privuse_var: Option<String> = None;
        if !variant.is_empty() {
            let toks: Vec<&str> = variant.split('_').collect();
            let mut i = 0;
            while i < toks.len() && is_variant(toks[i]) {
                variants.push(toks[i]);
                i += 1;
            }
            if !variants.is_empty() {
                has_subtag = true;
            }
            let rest: Vec<&str> = toks[i..]
                .iter()
                .copied()
                .take_while(|t| is_privateuse_subtag(t))
                .collect();
            if !rest.is_empty() {
                privuse_var = Some(rest.join("-"));
            }
        }
        let mut extensions: Vec<String> = Vec::new();
        let mut privateuse: Option<String> = None;
        for (k, v) in &self.extensions {
            if *k == 'x' {
                privateuse = Some(v.clone());
            } else {
                extensions.push(format!("{k}-{v}"));
            }
        }
        if !extensions.is_empty() {
            has_subtag = true;
        }
        if let Some(pv) = privuse_var {
            privateuse = Some(match privateuse {
                None => format!("{LVARIANT}-{pv}"),
                Some(p) => format!("{p}-{LVARIANT}-{}", pv.replace('_', "-")),
            });
        }
        if language.is_empty() && (has_subtag || privateuse.is_none()) {
            language = "und".to_string();
        }
        let mut parts: Vec<String> = Vec::new();
        if !language.is_empty() {
            parts.push(language.to_ascii_lowercase());
        }
        if !script.is_empty() {
            parts.push(script);
        }
        if !region.is_empty() {
            parts.push(region);
        }
        parts.extend(variants.iter().map(|v| v.to_string()));
        parts.extend(extensions.iter().map(|e| e.to_ascii_lowercase()));
        if let Some(p) = privateuse {
            parts.push(format!("x-{p}"));
        }
        parts.join("-")
    }

    /// `getExtension(key)`: the value under the singleton `key`.
    pub fn extension(&self, key: char) -> Option<&str> {
        self.extensions
            .get(&key.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// The Unicode extension's attributes and its keywords in key order.
    fn unicode_parts(&self) -> UnicodeParts {
        match self.extensions.get(&'u') {
            Some(v) => parse_unicode(v),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// `getUnicodeLocaleType(key)`.
    pub fn unicode_type(&self, key: &str) -> Option<String> {
        let (_, kws) = self.unicode_parts();
        kws.into_iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, t)| t)
    }

    /// Whether display names in this locale are the English ones
    /// [`crate::locale_names`] holds: `en` and `en_US`, the locales the JDK's
    /// root and `en` CLDR bundles serve.
    pub fn displays_in_english(&self) -> bool {
        self.language == "en"
            && (self.region.is_empty() || self.region == "US")
            && self.script.is_empty()
            && self.variant.is_empty()
    }

    pub fn display_language(&self) -> String {
        name_of(names::LANGUAGES, &self.language)
    }

    pub fn display_script(&self) -> String {
        name_of(names::SCRIPTS, &self.script)
    }

    pub fn display_country(&self) -> String {
        name_of(names::REGIONS, &self.region)
    }

    /// `getDisplayVariantArray`: each `_`-separated variant subtag's name
    /// (`StringTokenizer` skips empty ones), or the subtag itself.
    fn display_variants(&self) -> Vec<String> {
        self.variant
            .split('_')
            .filter(|t| !t.is_empty())
            .map(|t| name_of(names::VARIANTS, t))
            .collect()
    }

    pub fn display_variant(&self) -> String {
        self.display_variants().join(", ")
    }

    /// `getDisplayName` in English: the language — or, without one, the
    /// script, then the region — followed in parentheses by the rest, then
    /// the variants, the Unicode attributes and each Unicode keyword
    /// (`Japanese Calendar`, or `key: type` when the type has no name).
    pub fn display_name(&self) -> String {
        let (lang, script, country) = (
            self.display_language(),
            self.display_script(),
            self.display_country(),
        );
        let variants = self.display_variants();
        if lang.is_empty() && script.is_empty() && country.is_empty() {
            return variants.join(", ");
        }
        let mut parts: Vec<String> = [lang, script, country]
            .into_iter()
            .filter(|n| !n.is_empty())
            .collect();
        parts.extend(variants);
        let (attrs, kws) = self.unicode_parts();
        let mut attrs: Vec<String> = attrs.iter().map(|a| a.to_ascii_lowercase()).collect();
        attrs.sort();
        parts.extend(attrs.iter().map(|a| name_of(names::UEXT_KEYS, a)));
        for (key, ty) in kws {
            let named = name_of(names::UEXT_TYPES, &format!("{key}.{ty}"));
            if !ty.is_empty() && named != format!("{key}.{ty}") {
                parts.push(named);
                continue;
            }
            // `cu` names the currency, `rg` the region its first two letters
            // spell.
            let shown = if key == "cu" {
                name_of(names::CURRENCIES, &ty.to_ascii_lowercase())
            } else if key == "rg" && ty.len() == 6 && ty[2..].eq_ignore_ascii_case("zzzz") {
                name_of(names::REGIONS, &ty[..2].to_ascii_uppercase())
            } else {
                ty.clone()
            };
            parts.push(format!("{}: {shown}", name_of(names::UEXT_KEYS, &key)));
        }
        let main = parts.remove(0);
        if parts.is_empty() {
            main
        } else {
            format!("{main} ({})", parts.join(", "))
        }
    }
}

/// The extensions an [`JLocale::extension_id`] spells, back as a map: a
/// one-character subtag opens the next extension, except inside private use,
/// which is last and whose subtags may be one character long.
pub fn parse_extension_id(id: &str) -> BTreeMap<char, String> {
    let mut out = BTreeMap::new();
    if id.is_empty() {
        return out;
    }
    let mut key: Option<char> = None;
    let mut value: Vec<&str> = Vec::new();
    for tok in id.split('-') {
        let opens = tok.len() == 1 && key != Some('x');
        if opens {
            if let Some(k) = key {
                out.insert(k, value.join("-"));
            }
            key = tok.chars().next();
            value.clear();
        } else {
            value.push(tok);
        }
    }
    if let Some(k) = key {
        out.insert(k, value.join("-"));
    }
    out
}

/// The name `table` gives `code`, the code itself when it has none, and the
/// empty string for an empty code (`getDisplayString`).
fn name_of(table: &[(&str, &str)], code: &str) -> String {
    if code.is_empty() {
        return String::new();
    }
    match table.binary_search_by(|(k, _)| (*k).cmp(code)) {
        Ok(i) => table[i].1.to_string(),
        Err(_) => code.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_tables_are_sorted_for_binary_search() {
        for t in [
            names::LANGUAGES,
            names::SCRIPTS,
            names::REGIONS,
            names::VARIANTS,
            names::UEXT_KEYS,
            names::UEXT_TYPES,
            names::CURRENCIES,
        ] {
            assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
        }
    }

    #[test]
    fn a_compatibility_locale_round_trips_through_its_tag() {
        let ja = JLocale::of("ja", "JP", "JP");
        assert_eq!(ja.to_jstring(), "ja_JP_JP_#u-ca-japanese");
        let tag = ja.to_language_tag();
        assert_eq!(tag, "ja-JP-u-ca-japanese-x-lvariant-JP");
        assert_eq!(JLocale::for_language_tag(&tag), ja);
        let nn = JLocale::of("no", "NO", "NY");
        assert_eq!(nn.to_language_tag(), "nn-NO");
    }
}
