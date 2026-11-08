//! Normalised categories: a country and themes of a closed list for every catalogue item (PRD-33).
//!
//! Pure functions, no SQLite: the same input always gives the same result. Raise
//! `TAXONOMY_VERSION` whenever the data or the rules change, so stored items get reclassified.

pub mod country;
pub mod data;
pub mod theme;
pub mod tokenize;

#[cfg(test)]
mod tests;

pub use country::{CountryMatch, CountrySource};
pub use theme::{theme_from_epg_categories, ThemeSource, ThemeTag};

/// Version of the rules and the dictionaries (PRD-33 § 4.3).
pub const TAXONOMY_VERSION: u32 = 3;

/// The closed list of themes (PRD-33 § 3). Stored by `as_str`, never by position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Theme {
    General,
    News,
    Sports,
    Entertainment,
    Movies,
    Series,
    Kids,
    Music,
    Documentary,
    Lifestyle,
    Education,
    Religious,
    Institutional,
    Shopping,
    Adult,
    Other,
}

impl Theme {
    pub const ALL: [Theme; 16] = [
        Theme::General,
        Theme::News,
        Theme::Sports,
        Theme::Entertainment,
        Theme::Movies,
        Theme::Series,
        Theme::Kids,
        Theme::Music,
        Theme::Documentary,
        Theme::Lifestyle,
        Theme::Education,
        Theme::Religious,
        Theme::Institutional,
        Theme::Shopping,
        Theme::Adult,
        Theme::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Theme::General => "general",
            Theme::News => "news",
            Theme::Sports => "sports",
            Theme::Entertainment => "entertainment",
            Theme::Movies => "movies",
            Theme::Series => "series",
            Theme::Kids => "kids",
            Theme::Music => "music",
            Theme::Documentary => "documentary",
            Theme::Lifestyle => "lifestyle",
            Theme::Education => "education",
            Theme::Religious => "religious",
            Theme::Institutional => "institutional",
            Theme::Shopping => "shopping",
            Theme::Adult => "adult",
            Theme::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Option<Theme> {
        Theme::ALL.into_iter().find(|d| d.as_str() == value)
    }
}

/// What the classifier reads from an item. Nothing else: no URL, no network.
#[derive(Debug, Clone, Copy)]
pub struct ClassifyInput<'a> {
    /// `live`, `movie`, `episode` or `series`.
    pub kind: &'a str,
    pub group_title: Option<&'a str>,
    pub tvg_id: Option<&'a str>,
    pub name: &'a str,
    /// `tvg-country` / `country` attribute of the M3U line.
    pub country_attr: Option<&'a str>,
}

/// A correction made by the user (PRD-33 § 7). `None` fields keep the computed value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaxonomyOverride {
    /// ISO code or `INT`.
    pub country: Option<String>,
    pub themes: Option<Vec<Theme>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub country: Option<CountryMatch>,
    /// Never empty; `other` when nothing matched.
    pub themes: Vec<ThemeTag>,
}

impl Classification {
    pub fn primary_theme(&self) -> Theme {
        theme::primary_theme(&self.themes)
    }

    pub fn country_code(&self) -> Option<&str> {
        self.country.as_ref().map(|c| c.code.as_str())
    }

    pub fn has_theme(&self, theme: Theme) -> bool {
        self.themes.iter().any(|t| t.theme == theme)
    }

    /// Country and theme names, indexed for search (PRD-33 lot 33.4): "france sport infos news…".
    pub fn search_terms(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(country) = &self.country {
            parts.push(country::country_terms(&country.code));
        }
        for tag in &self.themes {
            if let Some((_, terms)) = data::themes::THEME_TERMS.iter().find(|(t, _)| *t == tag.theme) {
                parts.push((*terms).to_string());
            }
        }
        parts.retain(|p| !p.is_empty());
        parts.join(" ")
    }
}

/// Search words of one theme (used when a theme is added after the first classification).
pub fn theme_terms(theme: Theme) -> &'static str {
    data::themes::THEME_TERMS.iter().find(|(t, _)| *t == theme).map_or("", |(_, terms)| terms)
}

pub fn classify(input: &ClassifyInput<'_>) -> Classification {
    Classification { country: country::resolve_country(input), themes: theme::resolve_themes(input) }
}

/// `classify`, then the user's correction on top. A correction never removes the `adult` theme
/// found by the parental rule (PRD-33 § 11).
pub fn classify_with_override(input: &ClassifyInput<'_>, correction: Option<&TaxonomyOverride>) -> Classification {
    let mut result = classify(input);
    let Some(correction) = correction else { return result };
    if let Some(code) = &correction.country {
        result.country = Some(CountryMatch { code: code.clone(), source: CountrySource::User });
    }
    if let Some(themes) = correction.themes.as_ref().filter(|d| !d.is_empty()) {
        let adult = result.has_theme(Theme::Adult);
        result.themes = themes
            .iter()
            .map(|d| ThemeTag { theme: *d, subtheme: None, source: ThemeSource::User })
            .collect();
        if adult && !result.has_theme(Theme::Adult) {
            result.themes.push(ThemeTag { theme: Theme::Adult, subtheme: None, source: ThemeSource::Group });
        }
    }
    result
}
