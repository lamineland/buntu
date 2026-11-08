import Foundation
import CndCoreBridge

/// The three ways to browse a catalogue's categories (PRD-33 § 8.1). The engine's `CategoryAxis`
/// (country / theme / provider) is the data side; this adds the stored choice and the wording.
extension CategoryAxis: CaseIterable, Identifiable {
    public static var allCases: [CategoryAxis] { [.country, .theme, .provider] }
    public var id: String { storageValue }

    /// Value kept in `@AppStorage`.
    public var storageValue: String {
        switch self {
        case .country: return "country"
        case .theme: return "theme"
        case .provider: return "provider"
        }
    }

    public init(storageValue: String, fallback: CategoryAxis) {
        self = Self.allCases.first { $0.storageValue == storageValue } ?? fallback
    }

    var title: String {
        switch self {
        case .country: return "Pays"
        case .theme: return "Thèmes"
        case .provider: return "Fournisseur"
        }
    }

    /// The axis shown as chips when this one is the column.
    var other: CategoryAxis? {
        switch self {
        case .country: return .theme
        case .theme: return .country
        case .provider: return nil
        }
    }
}

/// Wording, icons and ordering of countries and themes. Pure functions, so they are testable.
enum CategoryText {
    static let unknownCountryKey = ""
    static let internationalKey = "INT"

    // MARK: Countries

    static func countryName(_ code: String, locale: Locale = .current) -> String {
        switch code {
        case unknownCountryKey: return "Pays inconnu"
        case internationalKey: return "International"
        default: return locale.localizedString(forRegionCode: code) ?? code
        }
    }

    /// Regional-indicator flag of an ISO code; a globe for `INT` and a white flag for "unknown".
    static func flag(_ code: String) -> String {
        switch code {
        case unknownCountryKey: return "🏳️"
        case internationalKey: return "🌐"
        default:
            let upper = code.uppercased()
            guard upper.count == 2, upper.unicodeScalars.allSatisfy({ ("A"..."Z").contains(Character($0)) }) else { return "🏳️" }
            return String(upper.unicodeScalars.compactMap { UnicodeScalar(127_397 + $0.value) }.map(Character.init))
        }
    }

    static func countryLabel(_ code: String, locale: Locale = .current) -> String {
        "\(flag(code)) \(countryName(code, locale: locale))"
    }

    /// Device country first, then alphabetical by the localised name; "International" and "unknown" last.
    static func sortedCountries(_ facets: [CategoryFacet], region: String? = Locale.current.region?.identifier, locale: Locale = .current) -> [CategoryFacet] {
        func rank(_ key: String) -> Int {
            if key == unknownCountryKey { return 3 }
            if key == internationalKey { return 2 }
            if let region, key == region.uppercased() { return 0 }
            return 1
        }
        return facets.sorted { a, b in
            let (ra, rb) = (rank(a.key), rank(b.key))
            if ra != rb { return ra < rb }
            return countryName(a.key, locale: locale).compare(countryName(b.key, locale: locale), locale: locale) == .orderedAscending
        }
    }

    /// Every country a correction can pick, by localised name, with "International" first.
    static func allCountryCodes(locale: Locale = .current) -> [String] {
        let codes = Locale.Region.isoRegions.map(\.identifier).filter { $0.count == 2 && $0.allSatisfy(\.isLetter) }
        let named = codes.filter { locale.localizedString(forRegionCode: $0) != nil }
        let sorted = named.sorted { countryName($0, locale: locale).compare(countryName($1, locale: locale), locale: locale) == .orderedAscending }
        return [internationalKey] + sorted
    }

    /// Themes a user can assign by hand: `adult` is derived from the provider's label only and `other`
    /// means "no theme".
    static let assignableThemes = [
        "general", "news", "sports", "entertainment", "movies", "series", "kids", "music",
        "documentary", "lifestyle", "education", "religious", "institutional", "shopping",
    ]

    // MARK: Themes

    static func themeName(_ id: String) -> String {
        switch id {
        case "general": return "Généralistes"
        case "news": return "Infos"
        case "sports": return "Sport"
        case "entertainment": return "Divertissement"
        case "movies": return "Cinéma"
        case "series": return "Séries"
        case "kids": return "Jeunesse"
        case "music": return "Musique"
        case "documentary": return "Documentaires"
        case "lifestyle": return "Art de vivre"
        case "education": return "Éducation"
        case "religious": return "Religion"
        case "institutional": return "Institutionnel"
        case "shopping": return "Téléachat"
        case "adult": return "Adultes"
        case "other": return "Non classé"
        default: return id
        }
    }

    static func themeIcon(_ id: String) -> String {
        switch id {
        case "general": return "tv"
        case "news": return "newspaper"
        case "sports": return "sportscourt"
        case "entertainment": return "theatermasks"
        case "movies": return "film"
        case "series": return "play.rectangle.on.rectangle"
        case "kids": return "teddybear"
        case "music": return "music.note"
        case "documentary": return "globe.europe.africa"
        case "lifestyle": return "leaf"
        case "education": return "graduationcap"
        case "religious": return "building.columns"
        case "institutional": return "building.2"
        case "shopping": return "bag"
        case "adult": return "18.circle"
        default: return "square.grid.2x2"
        }
    }

    static func subthemeName(_ id: String) -> String {
        switch id {
        case "business": return "Économie"
        case "weather": return "Météo"
        case "politics": return "Politique"
        case "football": return "Football"
        case "motorsport": return "Sport auto"
        case "combat": return "Sports de combat"
        case "outdoor": return "Plein air"
        case "comedy": return "Humour"
        case "reality": return "Télé-réalité"
        case "interactive": return "Interactif"
        case "classic": return "Classiques"
        case "animation": return "Animation"
        case "family": return "Famille"
        case "relax": return "Détente"
        case "science": return "Sciences"
        case "culture": return "Culture"
        case "nature": return "Nature"
        case "history": return "Histoire"
        case "cooking": return "Cuisine"
        case "travel": return "Voyage"
        case "auto": return "Auto"
        case "fashion": return "Mode"
        case "legislative": return "Parlement"
        case "public": return "Service public"
        // Genres of films and series (PRD-33 lot 33.5).
        case "action": return "Action"
        case "adventure": return "Aventure"
        case "crime": return "Policier"
        case "drama": return "Drame"
        case "fantasy": return "Fantastique"
        case "horror": return "Horreur"
        case "mystery": return "Mystère"
        case "romance": return "Romance"
        case "scifi": return "Science-fiction"
        case "thriller": return "Thriller"
        case "war": return "Guerre"
        case "western": return "Western"
        case "biography": return "Biographie"
        default: return id
        }
    }

    /// Most populated first; "unclassified" always last.
    static func sortedThemes(_ facets: [CategoryFacet]) -> [CategoryFacet] {
        facets.sorted { a, b in
            if (a.key == "other") != (b.key == "other") { return b.key == "other" }
            if a.count != b.count { return a.count > b.count }
            return a.key < b.key
        }
    }

    // MARK: Facets as the column shows them

    /// Display label of a key on `axis` (provider labels stay as the provider wrote them).
    static func label(_ key: String, axis: CategoryAxis, locale: Locale = .current) -> String {
        switch axis {
        case .country: return countryLabel(key, locale: locale)
        case .theme: return themeName(key)
        case .provider: return key
        }
    }

    static func icon(_ key: String, axis: CategoryAxis) -> String? {
        axis == .theme ? themeIcon(key) : nil
    }

    static func sorted(_ facets: [CategoryFacet], axis: CategoryAxis) -> [CategoryFacet] {
        switch axis {
        case .country: return sortedCountries(facets)
        case .theme: return sortedThemes(facets)
        case .provider: return facets
        }
    }

    static func asFacetCounts(_ facets: [CategoryFacet]) -> [FacetCount] {
        facets.map { FacetCount(value: $0.key, count: $0.count) }
    }

    /// "Sport · France": what the current selection means, for the screen title line.
    static func selectionTitle(country: String?, theme: String?, subtheme: String?, locale: Locale = .current) -> String? {
        var parts: [String] = []
        if let theme {
            parts.append(themeName(theme) + (subtheme.map { " (\(subthemeName($0)))" } ?? ""))
        }
        if let country { parts.append(countryName(country, locale: locale)) }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }
}
