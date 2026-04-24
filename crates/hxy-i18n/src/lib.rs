//! Fluent-based localization for hxy.
//!
//! Translations live under `crates/hxy-i18n/translations/<lang>/*.ftl` and are
//! embedded at compile time via [`fluent_templates::static_loader`]. The
//! current language is stored in a process-wide [`RwLock`] and can be
//! switched at runtime; on first call, [`init_from_system_locale`] picks the
//! best supported match for the OS locale, falling back to `en-US`.

#![forbid(unsafe_code)]

use std::sync::OnceLock;
use std::sync::RwLock;

use fluent_templates::LanguageIdentifier;
use fluent_templates::Loader;
use unic_langid::langid;

fluent_templates::static_loader! {
    static LOCALES = {
        locales: "./translations",
        fallback_language: "en-US",
    };
}

/// Languages shipped with this build. The first entry is the fallback.
pub const SUPPORTED: &[Language] = &[Language { id: langid!("en-US"), name: "English" }];

/// Metadata for a single supported language.
#[derive(Clone, Debug)]
pub struct Language {
    pub id: LanguageIdentifier,
    pub name: &'static str,
}

fn current_cell() -> &'static RwLock<LanguageIdentifier> {
    static CURRENT: OnceLock<RwLock<LanguageIdentifier>> = OnceLock::new();
    CURRENT.get_or_init(|| RwLock::new(SUPPORTED[0].id.clone()))
}

/// Current language. Defaults to `en-US` until [`set_language`] or
/// [`init_from_system_locale`] is called.
pub fn current() -> LanguageIdentifier {
    current_cell().read().expect("i18n lock poisoned").clone()
}

/// Set the current language. Returns `true` if the language was recognised
/// and applied; `false` otherwise (and the current language is unchanged).
pub fn set_language(id: &LanguageIdentifier) -> bool {
    if !SUPPORTED.iter().any(|lang| &lang.id == id) {
        return false;
    }
    *current_cell().write().expect("i18n lock poisoned") = id.clone();
    true
}

/// Pick the best supported language for the system locale and apply it.
///
/// Returns the language that was selected. Falls back to the first supported
/// entry (`en-US`) when no match is found.
pub fn init_from_system_locale() -> LanguageIdentifier {
    let picked = sys_locale::get_locale()
        .and_then(|tag| tag.parse::<LanguageIdentifier>().ok())
        .and_then(|requested| match_supported(&requested))
        .unwrap_or_else(|| SUPPORTED[0].id.clone());
    set_language(&picked);
    picked
}

/// Exact or language-only match against [`SUPPORTED`].
fn match_supported(requested: &LanguageIdentifier) -> Option<LanguageIdentifier> {
    if let Some(exact) = SUPPORTED.iter().find(|lang| &lang.id == requested) {
        return Some(exact.id.clone());
    }
    SUPPORTED.iter().find(|lang| lang.id.language == requested.language).map(|lang| lang.id.clone())
}

/// Translate `key` using the current language.
pub fn t(key: &str) -> String {
    LOCALES.lookup(&current(), key)
}

/// Translate `key` using a specific language.
pub fn t_in(lang: &LanguageIdentifier, key: &str) -> String {
    LOCALES.lookup(lang, key)
}

/// Translate `key` with Fluent variable interpolation -- e.g.
/// `t_args("palette-delete-template", &[("name", "PNG.bt")])`
/// against an entry like `palette-delete-template = Delete { $name }`.
pub fn t_args(key: &str, args: &[(&str, &str)]) -> String {
    let mut map: std::collections::HashMap<
        std::borrow::Cow<'static, str>,
        fluent_templates::fluent_bundle::FluentValue<'_>,
    > = std::collections::HashMap::new();
    for (k, v) in args {
        map.insert(
            std::borrow::Cow::Owned((*k).to_owned()),
            fluent_templates::fluent_bundle::FluentValue::from((*v).to_owned()),
        );
    }
    LOCALES.lookup_with_args(&current(), key, &map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_language_loads() {
        let s = t("app-name");
        assert_eq!(s, "hxy");
    }

    #[test]
    fn set_language_rejects_unknown() {
        let unknown = langid!("xx-YY");
        assert!(!set_language(&unknown));
    }
}
