//! Turning a Roblox display name into something a filesystem will accept.
//!
//! Roblox lets a game pass, badge or asset be called anything. Windows does
//! not: `? * : " < > | \ /` are illegal in a filename, and a name carrying one
//! fails at `std::fs::write` with `os error 123`, which reads as "La syntaxe du
//! nom de fichier … est incorrecte" and names neither the file nor the
//! character.
//!
//! That is not hypothetical. `rbx import` against a real universe stopped on a
//! game pass called `Auto collect?`: a perfectly ordinary name, and a question
//! mark is common in pass names because they are usually questions. The import
//! reported "shop skipped" and carried on, so the failure looked like a
//! permissions problem rather than a filename one.
//!
//! **CI could not have caught it.** This workspace's CI is Linux-only, on
//! purpose, to keep runner minutes affordable, and on Linux `?` is a legal
//! filename character, so the same code path passes there and fails on the
//! machine most Roblox developers use.

/// A display name reduced to something safe as one path component.
///
/// Keeps alphanumerics (Unicode-aware, so accented letters survive) plus
/// spaces, dashes and underscores, and trims the result. Everything else goes,
/// which covers the Windows-illegal set without having to enumerate it and
/// without inventing an escaping scheme nobody would be able to read back.
///
/// Returns an empty string when nothing survives (a name that is entirely
/// punctuation or emoji). Callers must have something to fall back on (an id,
/// usually) rather than writing a file with no stem.
pub fn safe_component(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name that actually broke an import.
    #[test]
    fn a_question_mark_is_removed() {
        assert_eq!(safe_component("Auto collect?"), "Auto collect");
    }

    #[test]
    fn every_character_windows_forbids_is_removed() {
        for bad in ['?', '*', ':', '"', '<', '>', '|', '\\', '/'] {
            let out = safe_component(&format!("a{bad}b"));
            assert_eq!(out, "ab", "{bad:?} survived");
        }
    }

    #[test]
    fn ordinary_names_are_left_alone() {
        assert_eq!(safe_component("Extendo Grabber"), "Extendo Grabber");
        assert_eq!(safe_component("VIP_pass-2"), "VIP_pass-2");
    }

    /// Unicode letters are alphanumeric, so a name in French or Japanese keeps
    /// its characters rather than being reduced to ASCII.
    #[test]
    fn accented_and_non_latin_letters_survive() {
        assert_eq!(safe_component("Café"), "Café");
        assert_eq!(safe_component("ピザ"), "ピザ");
    }

    /// Windows also trims trailing spaces and dots from filenames, so a name
    /// ending in one would silently become a different file.
    #[test]
    fn surrounding_whitespace_goes() {
        assert_eq!(safe_component("  spaced  "), "spaced");
        assert_eq!(safe_component("trailing."), "trailing");
    }

    /// Nothing survivable is a real answer, and the caller has to notice: a
    /// file named `pass-123-.png` is worse than one named `pass-123.png`.
    #[test]
    fn a_name_with_nothing_keepable_comes_back_empty() {
        assert_eq!(safe_component("???"), "");
        assert_eq!(safe_component("  "), "");
    }
}
