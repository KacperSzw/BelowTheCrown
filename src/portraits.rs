//! Static, monochrome Braille artwork. These cosmetic keys never affect the world.
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Library {
    actors: BTreeMap<String, String>,
    portraits: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        serde_json::from_str(include_str!("../content/portraits/baked.json"))
            .expect("Validated embedded Braille library")
    })
}

pub fn keys() -> Vec<String> {
    library().portraits.keys().cloned().collect()
}

pub fn key_for(id: u16) -> String {
    portrait_key(id, &[])
}

pub fn portrait_key(id: u16, used: &[String]) -> String {
    let library = library();
    if let Some(curated) = library.actors.get(&id.to_string()) {
        return curated.clone();
    }
    let faces: Vec<_> = library.portraits.keys().collect();
    if faces.is_empty() {
        return String::new();
    }
    let used: BTreeSet<_> = used.iter().collect();
    // Curated NPC faces are held back so newly arriving humans look equally varied.
    let reserved: BTreeSet<_> = library.actors.values().collect();
    let offset = usize::from(id) % faces.len();
    let candidates = (0..faces.len()).map(|index| faces[(offset + index) % faces.len()]);
    candidates
        .clone()
        .find(|key| !used.contains(key) && !reserved.contains(key))
        .or_else(|| candidates.clone().find(|key| !used.contains(key)))
        .unwrap_or(faces[offset])
        .clone()
}

/// Select the largest complete portrait fitting both bounds; never crop a face.
pub fn render(key: &str, width: u16, height: u16) -> Vec<String> {
    let Some(portrait) = library().portraits.get(key) else {
        return Vec::new();
    };
    for (variant, columns, rows) in [("detailed", 21, 9), ("standard", 15, 7), ("compact", 11, 5)] {
        if width >= columns && height >= rows {
            return portrait.get(variant).cloned().unwrap_or_default();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn every_face_is_monochrome_braille_with_exact_bounds() {
        let library = library();
        assert!(library.portraits.len() >= 33);
        for (key, variants) in &library.portraits {
            for (variant, width, height) in
                [("compact", 11, 5), ("standard", 15, 7), ("detailed", 21, 9)]
            {
                let rows = &variants[variant];
                assert_eq!(rows.len(), height, "{key}/{variant}");
                assert!(
                    rows.iter()
                        .flat_map(|row| row.chars())
                        .any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)),
                    "{key}/{variant} is blank"
                );
                for row in rows {
                    assert_eq!(
                        UnicodeWidthStr::width(row.as_str()),
                        width,
                        "{key}/{variant}"
                    );
                    assert!(
                        row.chars()
                            .all(|c| c == ' ' || ('\u{2800}'..='\u{28ff}').contains(&c))
                    );
                }
            }
        }
        for key in library.actors.values() {
            assert!(library.portraits.contains_key(key));
        }
    }

    #[test]
    fn portrait_identity_is_stable_and_size_bounded() {
        let first = portrait_key(1028, &[]);
        assert_eq!(first, portrait_key(1028, &[]));
        let second = portrait_key(1029, std::slice::from_ref(&first));
        assert_ne!(first, second);
        assert_eq!(key_for(104), "captain");
        assert_eq!(render(&first, 11, 5).len(), 5);
        assert_eq!(render(&first, 80, 7).len(), 7);
        assert_eq!(render(&first, 21, 9).len(), 9);
        assert!(render(&first, 10, 8).is_empty());
        assert!(render("retired-key", 80, 20).is_empty());
    }
}
