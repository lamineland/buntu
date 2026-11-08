import XCTest
import CndCoreBridge
@testable import CndIptvKit

/// Country / theme categories (PRD-33 lot 33.3): wording and ordering, then the Live TV model
/// against a real in-memory engine.
final class CategoryTextTests: XCTestCase {
    private let french = Locale(identifier: "fr_FR")

    private func facet(_ key: String, _ count: UInt32 = 1) -> CategoryFacet {
        CategoryFacet(key: key, count: count, subkeys: [])
    }

    func testFlagsAndNames() {
        XCTAssertEqual(CategoryText.flag("FR"), "🇫🇷")
        XCTAssertEqual(CategoryText.flag("GB"), "🇬🇧")
        XCTAssertEqual(CategoryText.flag("INT"), "🌐")
        XCTAssertEqual(CategoryText.flag(""), "🏳️")
        XCTAssertEqual(CategoryText.flag("france"), "🏳️", "never builds a flag from something that is not a code")
        XCTAssertEqual(CategoryText.countryName("FR", locale: french), "France")
        XCTAssertEqual(CategoryText.countryName("GB", locale: french), "Royaume-Uni")
        XCTAssertEqual(CategoryText.countryName("", locale: french), "Pays inconnu")
        XCTAssertEqual(CategoryText.countryName("INT", locale: french), "International")
        XCTAssertEqual(CategoryText.countryLabel("IT", locale: french), "🇮🇹 Italie")
    }

    func testCountriesListDeviceCountryFirstThenAlphabeticalWithInternationalAndUnknownLast() {
        let sorted = CategoryText.sortedCountries(
            [facet(""), facet("DE"), facet("INT"), facet("GB"), facet("FR"), facet("ES")],
            region: "FR", locale: french
        )
        XCTAssertEqual(sorted.map(\.key), ["FR", "DE", "ES", "GB", "INT", ""])
        let noRegion = CategoryText.sortedCountries([facet("GB"), facet("DE")], region: nil, locale: french)
        XCTAssertEqual(noRegion.map(\.key), ["DE", "GB"])
    }

    func testThemesAreSortedByCountWithUnclassifiedLast() {
        let sorted = CategoryText.sortedThemes([facet("other", 99), facet("news", 5), facet("sports", 8), facet("kids", 5)])
        XCTAssertEqual(sorted.map(\.key), ["sports", "kids", "news", "other"])
    }

    func testEveryThemeOfTheClosedListHasAFrenchNameAndASymbol() {
        let themes = ["general", "news", "sports", "entertainment", "movies", "series", "kids", "music",
                      "documentary", "lifestyle", "education", "religious", "institutional", "shopping", "adult", "other"]
        for id in themes {
            XCTAssertNotEqual(CategoryText.themeName(id), id, id)
            if id != "other" { XCTAssertNotEqual(CategoryText.themeIcon(id), "square.grid.2x2", id) }
        }
    }

    func testSubThemesHaveFrenchNames() {
        for id in ["business", "weather", "football", "motorsport", "combat", "comedy", "animation", "family", "classic", "cooking", "travel",
                   "action", "adventure", "crime", "drama", "fantasy", "horror", "mystery", "romance", "scifi", "thriller", "war", "western", "biography"] {
            XCTAssertNotEqual(CategoryText.subthemeName(id), id, id)
        }
    }

    func testLabelsFollowTheAxisAndProviderLabelsAreUntouched() {
        XCTAssertEqual(CategoryText.label("sports", axis: .theme), "Sport")
        XCTAssertEqual(CategoryText.label("FR", axis: .country, locale: french), "🇫🇷 France")
        XCTAssertEqual(CategoryText.label("Animation;Kids;Music", axis: .provider), "Animation;Kids;Music")
        XCTAssertNil(CategoryText.icon("FR", axis: .country))
        XCTAssertEqual(CategoryText.icon("sports", axis: .theme), "sportscourt")
    }

    func testSelectionTitle() {
        XCTAssertNil(CategoryText.selectionTitle(country: nil, theme: nil, subtheme: nil))
        XCTAssertEqual(CategoryText.selectionTitle(country: "FR", theme: "sports", subtheme: nil, locale: french), "Sport · France")
        XCTAssertEqual(CategoryText.selectionTitle(country: nil, theme: "sports", subtheme: "football"), "Sport (Football)")
        XCTAssertEqual(CategoryText.selectionTitle(country: "", theme: nil, subtheme: nil, locale: french), "Pays inconnu")
    }

    func testAxisStorageAndPairing() {
        for axis in CategoryAxis.allCases {
            XCTAssertEqual(CategoryAxis(storageValue: axis.storageValue, fallback: .provider), axis)
        }
        XCTAssertEqual(CategoryAxis(storageValue: "nonsense", fallback: .theme), .theme)
        XCTAssertEqual(CategoryAxis.country.other, .theme)
        XCTAssertEqual(CategoryAxis.theme.other, .country)
        XCTAssertNil(CategoryAxis.provider.other)
        XCTAssertEqual(CategoryAxis.allCases.map(\.title), ["Pays", "Thèmes", "Fournisseur"])
    }
}

@MainActor
final class LiveTvTaxonomyModelTests: XCTestCase {
    private var directory: URL!
    private var engine: CndEngine!

    override func setUp() async throws {
        directory = try TestFixtures.makeTempDirectory()
        engine = try CndEngine.inMemory()
        // 30 channels: Sport (index % 3 == 1), Kids (== 2), News (== 0), all with an `.fr` tvg-id.
        try await TestFixtures.importCatalog(into: engine, directory: directory, count: 30)
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: directory)
    }

    func testThemeColumnListsThemesAndCountriesBecomeChips() async {
        let model = LiveTvModel(engine: engine)
        model.axis = .theme
        await model.reloadTaxonomyFacets()
        XCTAssertEqual(Set(model.columnFacets.map(\.key)), ["news", "sports", "kids"])
        XCTAssertEqual(model.columnFacets.map(\.count), [10, 10, 10])
        XCTAssertEqual(model.crossFacets.map(\.key), ["FR"])
        XCTAssertEqual(model.crossFacets.first?.count, 30)
        XCTAssertNil(model.allCount, "no country picked: the screen shows the catalogue total")
    }

    func testPickingAThemeFiltersTheChannelsAndNarrowsTheCountryChips() async {
        let model = LiveTvModel(engine: engine)
        model.axis = .theme
        model.filter.theme = "sports"
        await model.reload()
        await model.reloadTaxonomyFacets()
        XCTAssertEqual(model.channels.count, 10)
        XCTAssertEqual(model.crossFacets.map(\.key), ["FR"])
        XCTAssertEqual(model.crossFacets.first?.count, 10, "chips count what the theme leaves")
        XCTAssertEqual(model.columnFacets.count, 3, "the theme column is not narrowed by a theme")
    }

    func testCountryColumnIsNarrowedByTheChosenTheme() async {
        let model = LiveTvModel(engine: engine)
        model.axis = .country
        model.filter.theme = "kids"
        await model.reload()
        await model.reloadTaxonomyFacets()
        XCTAssertEqual(model.channels.count, 10)
        XCTAssertEqual(model.columnFacets.map(\.key), ["FR"])
        XCTAssertEqual(model.allCount, 10, "country column: the total is what the theme leaves")
        XCTAssertEqual(Set(model.crossFacets.map(\.key)), ["news", "sports", "kids"], "theme chips are not narrowed by the theme itself")
    }

    func testCountryTotalIsShownOnTheThemeColumnOnceACountryIsPicked() async {
        let model = LiveTvModel(engine: engine)
        model.axis = .theme
        model.filter.country = "FR"
        await model.reloadTaxonomyFacets()
        XCTAssertEqual(model.allCount, 30)
        model.filter.country = "GB"
        await model.reload()
        await model.reloadTaxonomyFacets()
        XCTAssertEqual(model.allCount, 0)
        XCTAssertTrue(model.channels.isEmpty)
        XCTAssertTrue(model.columnFacets.isEmpty)
    }

    func testTheProviderAxisKeepsTheProvidersOwnGroups() async {
        let model = LiveTvModel(engine: engine)
        model.axis = .provider
        model.filter.group = "Sport"
        await model.reload()
        await model.reloadTaxonomyFacets()
        XCTAssertEqual(model.channels.count, 10)
        XCTAssertEqual(Set(model.groups.map(\.value)), ["News", "Sport", "Kids"])
        XCTAssertTrue(model.columnFacets.isEmpty && model.crossFacets.isEmpty)
    }

    func testASearchIgnoresTheCountryAndThemeSelectionAndSaysSo() async {
        let model = LiveTvModel(engine: engine)
        model.axis = .theme
        model.filter.theme = "sports"
        await model.reload()
        XCTAssertEqual(model.channels.count, 10)
        model.filter.text = "Chaine"
        model.filter.commit()
        await model.reload()
        XCTAssertEqual(model.channels.count, 30, "search looks in every category")
        XCTAssertTrue(model.searchIgnoresCategory)
        XCTAssertFalse(model.showsSections)
    }

    func testResetFiltersClearsTheTaxonomySelectionAndTheSignatureTracksIt() {
        let filter = SearchFilterState()
        let before = filter.signature
        filter.country = "FR"
        filter.theme = "sports"
        filter.subtheme = "football"
        XCTAssertTrue(filter.hasActiveFilter)
        XCTAssertNotEqual(filter.signature, before)
        filter.resetFilters()
        XCTAssertNil(filter.country)
        XCTAssertNil(filter.theme)
        XCTAssertNil(filter.subtheme)
        XCTAssertEqual(filter.signature, before)
    }

    func testTheEngineRecordedTheCountryAndThemesOfTheImportedChannels() throws {
        let status = try engine.taxonomyStatus()
        XCTAssertEqual(status.liveTotal, 30)
        XCTAssertEqual(status.liveWithoutCountry, 0)
        XCTAssertEqual(status.liveWithoutTheme, 0)
    }
}
