use anyhow::{Result, ensure};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub fn validate_language(language: &str) -> Result<()> {
    ensure!(
        matches!(language, "en" | "pl"),
        "Choose --language en or --language pl."
    );
    Ok(())
}

pub fn language(language: &str) -> Result<&str> {
    validate_language(language)?;
    Ok(language)
}

fn polish() -> &'static BTreeMap<String, String> {
    static CATALOG: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut catalog = BTreeMap::new();
        for source in [
            include_str!("../content/locales/pl.json"),
            include_str!("../content/locales/engine.pl.json"),
            include_str!("../content/locales/native.pl.json"),
        ] {
            let value: Value =
                serde_json::from_str(source).expect("Validated embedded localization catalog");
            for (key, value) in value
                .as_object()
                .expect("Localization catalog must be an object")
            {
                catalog.insert(
                    key.clone(),
                    value
                        .as_str()
                        .expect("Localization values must be text")
                        .to_owned(),
                );
            }
        }
        catalog
    })
}

/// Translate authored templates. Inserted speech, names and descriptions remain literal.
pub fn tr(language: &str, source: &str, args: &[String]) -> String {
    let template = if language == "pl" {
        polish().get(source).map(String::as_str).unwrap_or(source)
    } else {
        source
    };
    let mut rendered = String::with_capacity(template.len());
    let mut cursor = 0;
    while let Some(offset) = template[cursor..].find('{') {
        let start = cursor + offset;
        rendered.push_str(&template[cursor..start]);
        let Some(close) = template[start + 1..].find('}') else {
            rendered.push_str(&template[start..]);
            return rendered;
        };
        let end = start + 1 + close;
        match template[start + 1..end]
            .parse::<usize>()
            .ok()
            .and_then(|index| args.get(index))
        {
            Some(value) => rendered.push_str(value),
            None => rendered.push_str(&template[start..=end]),
        }
        cursor = end + 1;
    }
    rendered.push_str(&template[cursor..]);
    rendered
}

pub fn instructions(language: &str, listener: bool) -> String {
    let output = if listener {
        "the summary"
    } else {
        "all dialogue, summaries, goals, and newly written memories"
    };
    let language = if language == "pl" {
        "natural Polish with correct Polish diacritics"
    } else {
        "natural English"
    };
    format!(
        "Server language: {language}. Write {output} in {language}. Keep JSON keys, action kinds, enums, entity IDs and tool names exactly as specified. Preserve names. Older memories and player speech may use another language; understand them without changing the server language."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitution_is_literal_and_supports_polish() {
        assert_eq!(
            tr("pl", "Saved {0}.", &["{1} Łucja".into()]),
            "Zapisano {1} Łucja."
        );
        assert_eq!(tr("en", "{0} {1}", &["A".into()]), "A {1}");
        assert_eq!(tr("pl", "{0}", &["Bolesław".into()]), "Bolesław");
        assert!(validate_language("de").is_err());
    }
}
