use std::sync::OnceLock;
use windows::Win32::Globalization::GetUserDefaultUILanguage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Language {
    English,
    Japanese,
}

fn from_language_id(id: u16) -> Language {
    if id & 0x03ff == 0x11 {
        Language::Japanese
    } else {
        Language::English
    }
}

pub(crate) fn language() -> Language {
    static LANGUAGE: OnceLock<Language> = OnceLock::new();
    *LANGUAGE.get_or_init(|| from_language_id(unsafe { GetUserDefaultUILanguage() }))
}

pub(crate) fn japanese() -> bool {
    language() == Language::Japanese
}

macro_rules! tr {
    ($en:literal, $ja:literal $(,)?) => {
        if $crate::i18n::japanese() { $ja } else { $en }
    };
    ($en:literal, $ja:literal, $($args:tt)+) => {
        if $crate::i18n::japanese() { format!($ja, $($args)+) } else { format!($en, $($args)+) }
    };
}

// These variants also support captured named arguments in a format string.
macro_rules! message {
    ($en:literal, $ja:literal $(, $($args:tt)+)?) => {
        if $crate::i18n::japanese() { format!($ja $(, $($args)+)?) } else { format!($en $(, $($args)+)?) }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn japanese_ui_language_and_english_fallback() {
        assert_eq!(from_language_id(0x0411), Language::Japanese);
        assert_eq!(from_language_id(0x0811), Language::Japanese);
        assert_eq!(from_language_id(0x0409), Language::English);
        assert_eq!(from_language_id(0x040c), Language::English);
        assert_eq!(from_language_id(0), Language::English);
    }
}
