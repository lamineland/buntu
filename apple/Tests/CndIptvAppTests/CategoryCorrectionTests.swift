import XCTest
import CndCoreBridge
@testable import CndIptvKit

/// Corrections, hidden countries and themes, kids themes and the reset (PRD-33 lot 33.4), against a
/// real in-memory engine. The catalogue: 30 channels in the groups News / Sport / Kids (10 each), all
/// with an `.fr` tvg-id.
@MainActor
final class CategoryCorrectionTests: XCTestCase {
    private var directory: URL!
    private var engine: CndEngine!

    override func setUp() async throws {
        directory = try TestFixtures.makeTempDirectory()
        engine = try CndEngine.inMemory()
        try await TestFixtures.importCatalog(into: engine, directory: directory, count: 30)
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: directory)
    }

    private func firstChannel(in group: String) throws -> IptvItemRecord {
        try XCTUnwrap(try engine.queryCatalog(filter: filter(group: group)).first)
    }

    private func filter(group: String? = nil, query: String = "") -> CatalogFilter {
        CatalogFilter(
            kind: "live", sourceId: nil, query: query, groupTitle: group, host: nil, favoritesOnly: false,
            includeBroken: true, includeHidden: false, sortBy: .name, sortAsc: true, limit: 100, offset: 0
        )
    }

    private func names(country: String? = nil, theme: String? = nil) throws -> [String] {
        try engine.queryCatalogByTaxonomy(
            filter: filter(), taxonomy: TaxonomyFilter(country: country, theme: theme, subtheme: nil)
        ).map(\.name)
    }

    // MARK: Corrections

    func testCorrectingAChannelChangesItsCountryAndThemesAndCanBeUndone() throws {
        let item = try firstChannel(in: "Sport")
        let model = TaxonomyCorrectionModel(engine: engine, target: .channel(stableHash: item.stableHash, name: item.name))
        XCTAssertFalse(model.canSave, "nothing chosen and nothing to undo")
        XCTAssertFalse(model.hasCorrection)
        XCTAssertEqual(model.currentDescription?.contains("Sport"), true, model.currentDescription ?? "")

        model.country = "BE"
        model.toggle(theme: "music")
        model.toggle(theme: "kids")
        XCTAssertTrue(model.canSave)
        XCTAssertTrue(model.save())

        let stored = try XCTUnwrap(try engine.getItemTaxonomy(stableHash: item.stableHash))
        XCTAssertEqual(stored.country, "BE")
        XCTAssertTrue(stored.corrected)
        XCTAssertEqual(Set(stored.themes), ["kids", "music"])
        XCTAssertTrue(try names(country: "BE", theme: "music").contains(item.name))
        // The list is stored in the order of the closed list, whatever order the user ticked.
        XCTAssertEqual(try engine.getTaxonomyOverride(stableHash: item.stableHash)?.themes, ["kids", "music"])

        // Reopening the sheet shows what was saved, and "Rétablir" removes it.
        let reopened = TaxonomyCorrectionModel(engine: engine, target: .channel(stableHash: item.stableHash, name: item.name))
        XCTAssertTrue(reopened.hasCorrection)
        XCTAssertEqual(reopened.country, "BE")
        XCTAssertEqual(reopened.themes, ["kids", "music"])
        XCTAssertTrue(reopened.restoreAutomatic())
        XCTAssertFalse(reopened.hasCorrection)
        let back = try XCTUnwrap(try engine.getItemTaxonomy(stableHash: item.stableHash))
        XCTAssertEqual(back.country, "FR")
        XCTAssertEqual(back.themes, ["sports"])
        XCTAssertFalse(back.corrected)
    }

    func testCorrectingAProviderCategoryClassifiesAllItsChannels() throws {
        let model = TaxonomyCorrectionModel(engine: engine, target: .providerCategory("Kids"))
        XCTAssertEqual(model.target.title, "Classer la catégorie « Kids »")
        model.toggle(theme: "education")
        XCTAssertTrue(model.save())
        XCTAssertEqual(try names(theme: "education").count, 10)
        XCTAssertEqual(try engine.getTaxonomyGroupOverride(groupTitle: "Kids")?.themes, ["education"])
        XCTAssertEqual(try engine.taxonomyOverrideSummary().groups, 1)

        let one = try firstChannel(in: "Kids")
        XCTAssertTrue(try XCTUnwrap(try engine.getItemTaxonomy(stableHash: one.stableHash)).categoryRule)

        let reopened = TaxonomyCorrectionModel(engine: engine, target: .providerCategory("Kids"))
        XCTAssertEqual(reopened.themes, ["education"])
        XCTAssertTrue(reopened.restoreAutomatic())
        XCTAssertEqual(try names(theme: "education").count, 0)
        XCTAssertEqual(try names(theme: "kids").count, 10)
    }

    func testAnUnknownCountryIsRefusedAndTheMessageIsShown() throws {
        let item = try firstChannel(in: "News")
        let model = TaxonomyCorrectionModel(engine: engine, target: .channel(stableHash: item.stableHash, name: item.name))
        model.country = "France"
        XCTAssertFalse(model.save())
        XCTAssertNotNil(model.errorMessage)
        XCTAssertFalse(try XCTUnwrap(try engine.getItemTaxonomy(stableHash: item.stableHash)).corrected)
    }

    func testTheCountryAndThemeListsOfTheSheet() {
        let codes = CategoryText.allCountryCodes(locale: Locale(identifier: "fr_FR"))
        XCTAssertEqual(codes.first, "INT")
        XCTAssertTrue(codes.contains("FR") && codes.contains("GB"))
        XCTAssertEqual(Set(codes).count, codes.count, "no duplicate")
        XCTAssertGreaterThan(codes.count, 150)
        XCTAssertFalse(CategoryText.assignableThemes.contains("adult"))
        XCTAssertFalse(CategoryText.assignableThemes.contains("other"))
        XCTAssertEqual(CategoryText.assignableThemes.count, 14)
    }

    // MARK: Hidden countries and themes

    func testAHiddenCountryOrThemeDisappearsFromTheColumnAndTheEditorListsIt() throws {
        let profile = engine.activeProfileId()
        try engine.hideGroup(profileId: profile, kind: "theme:live", group: "sports")
        let themes = try engine.getCategoryFacets(kind: "live", axis: .theme, sourceId: nil, filter: TaxonomyFilter(country: nil, theme: nil, subtheme: nil))
        XCTAssertFalse(themes.contains { $0.key == "sports" })
        XCTAssertEqual(try names().count, 20, "the sport channels are hidden")

        let editor = BouquetEditorModel(engine: engine, kind: "live")
        XCTAssertEqual(editor.hiddenThemes, ["sports"])
        XCTAssertTrue(editor.hiddenCountries.isEmpty)
        editor.unhideTheme("sports")
        XCTAssertTrue(editor.hiddenThemes.isEmpty)
        XCTAssertEqual(try names().count, 30)

        try engine.hideGroup(profileId: profile, kind: "country:live", group: "FR")
        XCTAssertEqual(try names().count, 0)
        let countryEditor = BouquetEditorModel(engine: engine, kind: "live")
        XCTAssertEqual(countryEditor.hiddenCountries, ["FR"], "shown as an ISO code, not the stored lower-case key")
        countryEditor.unhideCountry("FR")
        XCTAssertEqual(try names().count, 30)
    }

    // MARK: Kids

    func testAKidsProfileCanBeGivenAWholeThemeButNeverTheAdultOne() throws {
        let kid = try engine.createProfile(name: "Léa", avatar: "star", isKids: true)
        let model = KidsAllowListModel(engine: engine, profile: kid)
        XCTAssertEqual(Set(model.themes.map(\.key)), ["kids", "news", "sports"])
        XCTAssertFalse(model.themes.contains { $0.key == "adult" || $0.key == "other" })
        XCTAssertTrue(model.allowedThemes.isEmpty)

        let kids = try XCTUnwrap(model.themes.first { $0.key == "kids" })
        model.setThemeAllowed(kids, true)
        XCTAssertTrue(model.isThemeAllowed(kids))
        XCTAssertEqual(try engine.listAllowedGroups(profileId: kid.id, kind: "theme:live"), ["kids"])
        XCTAssertNil(model.errorMessage)

        engine.setActiveProfileId(id: kid.id)
        XCTAssertEqual(try names().count, 10, "only the kids theme is visible to the profile")
        engine.setActiveProfileId(id: 1)
        XCTAssertEqual(try names().count, 30)

        model.setThemeAllowed(kids, false)
        XCTAssertFalse(model.isThemeAllowed(kids))
        XCTAssertThrowsError(try engine.allowGroup(profileId: kid.id, kind: "theme:live", group: "adult"))
    }

    func testThemesAreOnlyOfferedForLiveChannels() throws {
        let kid = try engine.createProfile(name: "Léa", avatar: "star", isKids: true)
        let model = KidsAllowListModel(engine: engine, profile: kid)
        XCTAssertFalse(model.themes.isEmpty)
        model.kind = .movie
        XCTAssertTrue(model.themes.isEmpty)
        XCTAssertTrue(model.allowedThemes.isEmpty)
    }

    // MARK: Search and reset

    func testSearchFindsChannelsByTheNamesOfTheirCountryAndTheme() throws {
        let found = try engine.queryCatalog(filter: filter(query: "sport france")).map(\.name)
        XCTAssertEqual(found.count, 10)
        XCTAssertTrue(try engine.queryCatalog(filter: filter(query: "sport belgique")).isEmpty)
        let item = try firstChannel(in: "Sport")
        try engine.setTaxonomyOverride(stableHash: item.stableHash, country: "BE", themes: nil)
        XCTAssertEqual(try engine.queryCatalog(filter: filter(query: "sport belgique")).map(\.name), [item.name])
    }

    func testResettingTheCorrectionsFromTheSettings() throws {
        let defaults = UserDefaults(suiteName: "cnd-corrections-\(UUID().uuidString)")!
        let settings = SettingsModel(
            engine: engine,
            keychain: KeychainStore(service: "cnd-tests-\(UUID().uuidString)"),
            directory: DownloadDirectoryManager(defaults: defaults, platformDefault: directory.appendingPathComponent("default")),
            defaults: defaults
        )
        settings.refreshCategoryCorrections()
        XCTAssertEqual(settings.categoryCorrections.items + settings.categoryCorrections.groups, 0)
        XCTAssertEqual(settings.resetCategoryCorrections(), 0)

        var reloads = 0
        settings.onCategoriesChanged = { reloads += 1 }
        let item = try firstChannel(in: "News")
        try engine.setTaxonomyOverride(stableHash: item.stableHash, country: "DE", themes: nil)
        try engine.setTaxonomyGroupOverride(groupTitle: "Kids", country: nil, themes: ["education"])
        settings.refreshCategoryCorrections()
        XCTAssertEqual(settings.categoryCorrections.items, 1)
        XCTAssertEqual(settings.categoryCorrections.groups, 1)

        XCTAssertEqual(settings.resetCategoryCorrections(), 2)
        XCTAssertEqual(reloads, 1)
        XCTAssertEqual(settings.categoryCorrections.items + settings.categoryCorrections.groups, 0)
        XCTAssertEqual(try names(country: "FR").count, 30, "the computed classification is back")
        XCTAssertEqual(try names(theme: "education").count, 0)
    }
}
