import XCTest
import CndCoreBridge
@testable import CndIptvKit

// MARK: - Pure helpers

final class DownloadPlannerTests: XCTestCase {
    func testSanitizeRemovesPathSeparatorsAndEdgeDots() {
        XCTAssertEqual(DownloadPlanner.sanitize("a/b:c*d?e\"f<g>h|i"), "a_b_c_d_e_f_g_h_i")
        let hostile = DownloadPlanner.sanitize("../../etc/passwd")
        XCTAssertFalse(hostile.contains("/"))
        XCTAssertFalse(hostile.hasPrefix("."))
        XCTAssertEqual(DownloadPlanner.sanitize("   "), "video")
        XCTAssertEqual(DownloadPlanner.sanitize("..", fallback: "x"), "x")
        XCTAssertEqual(DownloadPlanner.sanitize("a   b\tc"), "a b c")
        XCTAssertLessThanOrEqual(DownloadPlanner.sanitize(String(repeating: "é", count: 500)).count, 120)
    }

    func testTimeFormat() {
        XCTAssertEqual(TimeFormat.hms(milliseconds: 42 * 60_000 + 15_000), "42:15")
        XCTAssertEqual(TimeFormat.hms(milliseconds: 3_723_000), "1:02:03")
        XCTAssertEqual(TimeFormat.hms(milliseconds: -5), "0:00")
    }

    func testEpisodeTitleStripsSeriesNameAndMarker() {
        XCTAssertEqual(DownloadPlanner.episodeTitle("Breaking Bad S01E02 Cat's in the Bag", seriesTitle: "Breaking Bad"), "Cat's in the Bag")
        XCTAssertEqual(DownloadPlanner.episodeTitle("Breaking Bad S01E01", seriesTitle: "Breaking Bad"), "")
        XCTAssertEqual(DownloadPlanner.episodeTitle("breaking bad - Saison 2 Episode 4 - Salud", seriesTitle: "Breaking Bad"), "Salud")
        XCTAssertEqual(DownloadPlanner.episodeTitle("Pilot", seriesTitle: "Dark"), "Pilot")
    }

    func testSeasonDirectoryIsSafeAndPadded() {
        let base = URL(fileURLWithPath: "/tmp/dl")
        XCTAssertEqual(
            DownloadPlanner.seasonDirectory(base: base, seriesTitle: "AC/DC: Live", season: 3).path,
            "/tmp/dl/AC_DC_ Live/Season 03"
        )
        XCTAssertEqual(DownloadPlanner.seasonDirectory(base: base, seriesTitle: "Dark", season: 12).lastPathComponent, "Season 12")
    }
}

// MARK: - Engine-backed

@MainActor
final class PlaybackRecorder {
    var played: [PlayableMedia] = []
    var queues: [(items: [PlayableMedia], index: Int)] = []
    var downloadsOpened = 0
    var favoriteChanges = 0
}

@MainActor
class VodTestCase: XCTestCase {
    var directory: URL!
    var engine: CndEngine!
    var keychain: KeychainStore!
    var recorder: PlaybackRecorder!
    var toasts: ToastCenter!
    var downloadBase: URL!
    var vod: VodContext!

    override func setUp() async throws {
        directory = try TestFixtures.makeTempDirectory()
        downloadBase = directory.appendingPathComponent("downloads")
        engine = try CndEngine.inMemory()
        keychain = KeychainStore(service: "com.cnd.iptv.tests.\(UUID().uuidString)")
        recorder = PlaybackRecorder()
        toasts = ToastCenter(duration: .milliseconds(150))
        let recorder = self.recorder!
        let base = downloadBase!
        vod = VodContext(
            engine: engine,
            keychain: keychain,
            toasts: toasts,
            downloadBase: { base },
            playMedia: { recorder.played.append($0) },
            playQueue: { recorder.queues.append(($0, $1)) },
            openDownloads: { recorder.downloadsOpened += 1 },
            onFavoriteChanged: { recorder.favoriteChanges += 1 }
        )
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: directory)
    }

    func movies() throws -> [IptvItemRecord] {
        try engine.queryCatalog(filter: CatalogFilter(
            kind: "movie", sourceId: nil, query: "", groupTitle: nil, host: nil, favoritesOnly: false,
            includeBroken: false, includeHidden: false,
            sortBy: .name, sortAsc: true, limit: 500, offset: 0
        ))
    }

    func allSeries() throws -> [SeriesRecord] {
        try engine.querySeries(filter: CatalogFilter(
            kind: "series", sourceId: nil, query: "", groupTitle: nil, host: nil, favoritesOnly: false,
            includeBroken: false, includeHidden: false,
            sortBy: .name, sortAsc: true, limit: 100, offset: 0
        ))
    }
}

final class MoviesVodTests: VodTestCase {
    func testMovieCatalogPagesAndExposesGroups() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 130)
        let model = VodCatalogModel<IptvItemRecord>(kind: "movie", source: .movies(engine: engine))

        await model.reload()
        XCTAssertEqual(model.entries.count, 60)
        XCTAssertTrue(model.hasMore)
        XCTAssertEqual(model.groups.map(\.value).sorted(), ["Action", "Drame"])
        XCTAssertTrue(model.entries.allSatisfy { $0.kind == "movie" })

        await model.loadNextPage()
        await model.loadNextPage()
        XCTAssertEqual(model.entries.count, 130)
        XCTAssertFalse(model.hasMore)
        XCTAssertEqual(Set(model.entries.map(\.id)).count, 130)

        model.filter.group = "Action"
        await model.reload()
        XCTAssertEqual(model.entries.count, 60)
        XCTAssertTrue(model.hasMore)
        XCTAssertFalse(model.showsSections)
        await model.loadNextPage()
        XCTAssertEqual(model.entries.count, 65)
        XCTAssertTrue(model.entries.allSatisfy { $0.groupTitle == "Action" })
    }

    func testSortByRatingAndSearch() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 12)
        let model = VodCatalogModel<IptvItemRecord>(kind: "movie", source: .movies(engine: engine))
        model.filter.text = "Film 007"
        model.filter.commit()
        await model.reload()
        XCTAssertEqual(model.entries.count, 1)
        XCTAssertEqual(model.entries.first?.nameClean, "Film 007")

        model.filter.text = ""
        model.filter.commit()
        model.filter.sort = .rating
        model.filter.ascending = false
        await model.reload()
        XCTAssertEqual(model.entries.count, 12, "sorting by rating must not drop unrated films")
    }

    func testContinueWatchingListsOnlyStartedUnfinishedMovies() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 6)
        let all = try movies()
        try engine.recordWatchProgress(stableHash: all[0].stableHash, positionMs: 120_000, durationMs: 600_000)
        try engine.recordWatchProgress(stableHash: all[1].stableHash, positionMs: 580_000, durationMs: 600_000)
        try engine.recordWatchProgress(stableHash: all[2].stableHash, positionMs: 0, durationMs: 0)

        let model = VodCatalogModel<IptvItemRecord>(kind: "movie", source: .movies(engine: engine))
        await model.reload()
        XCTAssertEqual(model.resume.map(\.stableHash), [all[0].stableHash], "finished and never-started titles are excluded")
        XCTAssertEqual(model.resume.first?.resumePositionMs, 120_000)
    }

    func testFavoritesSectionFollowsToggles() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 5)
        let model = VodCatalogModel<IptvItemRecord>(kind: "movie", source: .movies(engine: engine))
        await model.reload()
        let target = model.entries[2]

        let state = await vod.toggleFavorite(stableHash: target.stableHash, kind: target.kind)
        XCTAssertEqual(state, true)
        XCTAssertEqual(recorder.favoriteChanges, 1)
        model.applyFavorite(true, to: target)
        XCTAssertEqual(model.favorites.map(\.stableHash), [target.stableHash])
        XCTAssertTrue(model.entries[2].isFavorite)

        await model.reload()
        XCTAssertEqual(model.favorites.count, 1)

        let off = await vod.toggleFavorite(stableHash: target.stableHash, kind: target.kind)
        XCTAssertEqual(off, false)
        model.applyFavorite(false, to: target)
        XCTAssertTrue(model.favorites.isEmpty)
    }

    func testResumeOfferNeedsMoreThanThirtySecondsAndUnfinished() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        var item = try XCTUnwrap(movies().first)

        item.resumePositionMs = 29_999
        XCTAssertNil(VodContext.resumePosition(for: item))
        item.resumePositionMs = 30_001
        XCTAssertEqual(VodContext.resumePosition(for: item), 30_001)
        item.isCompleted = true
        XCTAssertNil(VodContext.resumePosition(for: item))
    }

    func testPlayResumesOrRestartsAndKeepsHeaders() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        var item = try XCTUnwrap(movies().first)
        item.resumePositionMs = 2_535_000 // 42:15

        vod.play(item, resume: true)
        vod.play(item, resume: false)
        XCTAssertEqual(recorder.played.map(\.startPositionMs), [2_535_000, 0])
        XCTAssertEqual(recorder.played[0].userAgent, TestFixtures.movieUserAgent)
        XCTAssertEqual(recorder.played[0].referer, "http://portal.test/")
        XCTAssertEqual(recorder.played[0].kind, .movie)
        XCTAssertEqual(TimeFormat.hms(milliseconds: 2_535_000), "42:15")
    }

    func testDetailsModelRefreshesResumeFromDatabaseAndExplainsMissingOmdbKey() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 2)
        let stale = try XCTUnwrap(movies().first)
        XCTAssertEqual(stale.resumePositionMs, 0)
        try engine.recordWatchProgress(stableHash: stale.stableHash, positionMs: 900_000, durationMs: 3_600_000)

        let model = MovieDetailsModel(item: stale, vod: vod)
        XCTAssertNil(model.resumePosition, "the stale row shows no resume yet")
        await model.load()
        XCTAssertEqual(model.resumePosition, 900_000)
        XCTAssertNil(model.metadata)
        XCTAssertTrue(model.metadataMessage?.contains("OMDb") == true, "no key in the Keychain: no network call, a hint instead")
        XCTAssertFalse(model.isLoadingMetadata)

        await model.toggleFavorite()
        XCTAssertTrue(model.item.isFavorite)
    }

    func testMovieDownloadGoesToTheEngineWithSafePathAndToast() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        let item = try XCTUnwrap(movies().first)

        let request = DownloadPlanner.movieRequest(item, baseDirectory: downloadBase)
        XCTAssertEqual(request.fileName, "Film 001 (2020).mkv")
        XCTAssertEqual(request.saveDir, downloadBase.path)
        XCTAssertEqual(request.url, item.streamUrl)
        XCTAssertEqual(request.userAgent, TestFixtures.movieUserAgent)
        XCTAssertEqual(request.referer, "http://portal.test/")
        XCTAssertTrue(request.autoStart)

        let queued = await vod.enqueue([request])
        XCTAssertEqual(queued, 1)
        let tasks = try engine.listDownloads(query: nil)
        XCTAssertEqual(tasks.count, 1)
        XCTAssertEqual(tasks.first?.fileName, "Film 001 (2020).mkv")
        XCTAssertTrue(tasks.first?.savePath.hasPrefix(downloadBase.path) == true)

        let toast = try XCTUnwrap(toasts.current)
        XCTAssertFalse(toast.isError)
        XCTAssertEqual(toast.actionTitle, "Voir")
        toast.action?()
        XCTAssertEqual(recorder.downloadsOpened, 1, "the toast links to the Downloads tab")
    }
}

final class SeriesVodTests: VodTestCase {
    func testSeriesCatalogShowsEachSeriesOnceWithCounts() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 3)
        let model = VodCatalogModel<SeriesRecord>(kind: "series", source: .series(engine: engine))
        await model.reload()

        XCTAssertEqual(model.entries.map(\.title).sorted(), ["Breaking Bad", "Dark"])
        let breakingBad = try XCTUnwrap(model.entries.first { $0.title == "Breaking Bad" })
        XCTAssertEqual(breakingBad.seasonCount, 2)
        XCTAssertEqual(breakingBad.episodeCount, 5)
        XCTAssertEqual(SeriesLabels.counts(breakingBad), "2 saisons · 5 épisodes")
        let dark = try XCTUnwrap(model.entries.first { $0.title == "Dark" })
        XCTAssertEqual(SeriesLabels.counts(dark), "1 saison · 2 épisodes")
        XCTAssertFalse(model.groups.isEmpty)
    }

    func testSeriesFavoritesAreUnified() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        let model = VodCatalogModel<SeriesRecord>(kind: "series", source: .series(engine: engine))
        await model.reload()
        let dark = try XCTUnwrap(model.entries.first { $0.title == "Dark" })

        let state = await vod.toggleFavorite(stableHash: dark.stableHash, kind: "series")
        XCTAssertEqual(state, true)
        XCTAssertEqual(try engine.listFavoriteSeries().map(\.title), ["Dark"])
        model.applyFavorite(true, to: dark)
        XCTAssertEqual(model.favorites.map(\.title), ["Dark"])
        XCTAssertEqual(try engine.getStats().totalFavorites, 1)
    }

    private func detailsModel(for title: String) async throws -> SeriesDetailsModel {
        let series = try XCTUnwrap(allSeries().first { $0.title == title })
        let model = SeriesDetailsModel(series: series, vod: vod)
        await model.load()
        return model
    }

    func testDetailsLoadSeasonsAndEpisodes() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        let model = try await detailsModel(for: "Breaking Bad")

        XCTAssertEqual(model.seasons.map(\.seasonNum), [1, 2])
        XCTAssertEqual(model.seasons.map(\.episodeCount), [3, 2])
        XCTAssertEqual(model.selectedSeason, 1)
        XCTAssertEqual(model.episodes.map(\.episodeNum), [1, 2, 3])
        XCTAssertFalse(model.isHydrating, "a non-Xtream series is never hydrated")
        XCTAssertNil(model.hydrationError)
        XCTAssertTrue(model.metadataMessage?.contains("OMDb") == true)

        await model.select(season: 2)
        XCTAssertEqual(model.selectedSeason, 2)
        XCTAssertEqual(model.episodes.map(\.episodeNum), [1, 2])
        XCTAssertTrue(model.episodes.allSatisfy { $0.seasonNum == 2 })
    }

    func testPlayWholeSeasonQueuesEveryEpisodeInOrder() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        let model = try await detailsModel(for: "Breaking Bad")

        model.playSeason()
        let queue = try XCTUnwrap(recorder.queues.last)
        XCTAssertEqual(queue.index, 0)
        XCTAssertEqual(queue.items.count, 3)
        XCTAssertTrue(queue.items.allSatisfy { $0.kind == .episode })
        XCTAssertEqual(queue.items.map(\.url.lastPathComponent), model.episodes.map { URL(string: $0.streamUrl)!.lastPathComponent })
    }

    func testPlayingOneEpisodeKeepsTheRestOfTheSeasonQueuedAndResumes() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        let model = try await detailsModel(for: "Breaking Bad")
        let second = model.episodes[1]
        try engine.recordWatchProgress(stableHash: second.stableHash, positionMs: 300_000, durationMs: 2_700_000)
        await model.reloadEpisodes()

        model.playEpisode(at: 1)
        let queue = try XCTUnwrap(recorder.queues.last)
        XCTAssertEqual(queue.index, 1)
        XCTAssertEqual(queue.items.count, 3, "auto-advance continues with the next episode")
        XCTAssertEqual(queue.items[1].startPositionMs, 300_000)
        XCTAssertEqual(queue.items[0].startPositionMs, 0)
        XCTAssertEqual(queue.items[2].startPositionMs, 0)
    }

    func testPlayWholeSeriesSpansAllSeasons() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        let model = try await detailsModel(for: "Breaking Bad")
        await model.playWholeSeries()
        XCTAssertEqual(recorder.queues.last?.items.count, 5)
    }

    func testSeasonDownloadCreatesOneTaskPerEpisodeInTheSeasonFolder() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        let model = try await detailsModel(for: "Breaking Bad")

        let requests = DownloadPlanner.seasonRequests(seriesTitle: "Breaking Bad", episodes: model.episodes, baseDirectory: downloadBase)
        let expectedDir = downloadBase.appendingPathComponent("Breaking Bad/Season 01").path
        XCTAssertEqual(requests.map(\.saveDir), Array(repeating: expectedDir, count: 3))
        XCTAssertEqual(requests.map(\.fileName), [
            "S01E01 - Pilot.mkv",
            "S01E02 - Cat's in the Bag.mkv",
            "S01E03 - And the Bag's in the River.mkv",
        ])

        model.downloadSeason()
        let queued = await eventually(timeout: 10) { (try? self.engine.listDownloads(query: nil).count) == 3 }
        XCTAssertTrue(queued)
        let paths = Set(try engine.listDownloads(query: nil).map(\.savePath))
        XCTAssertEqual(paths, Set(requests.map { "\(expectedDir)/\($0.fileName!)" }))
        XCTAssertEqual(toasts.current?.text, "3 téléchargements ajoutés")
    }

    func testSingleEpisodeDownloadUsesSeasonFolderAndSanitizedName() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 1)
        let model = try await detailsModel(for: "Breaking Bad")
        await model.select(season: 2)

        model.downloadEpisode(model.episodes[0])
        let queued = await eventually(timeout: 10) { (try? self.engine.listDownloads(query: nil).count) == 1 }
        XCTAssertTrue(queued)
        let task = try XCTUnwrap(engine.listDownloads(query: nil).first)
        XCTAssertEqual(task.fileName, "S02E01 - Seven Thirty-Seven.mkv")
        XCTAssertEqual(task.savePath, downloadBase.appendingPathComponent("Breaking Bad/Season 02/S02E01 - Seven Thirty-Seven.mkv").path)
        XCTAssertEqual(toasts.current?.text, "1 téléchargement ajouté")
    }
}

@MainActor
final class ToastCenterTests: XCTestCase {
    func testToastDismissesItselfAndNewerToastSurvivesOlderTimer() async throws {
        let center = ToastCenter(duration: .milliseconds(120))
        center.show("first")
        try await Task.sleep(for: .milliseconds(70))
        center.show("second")
        try await Task.sleep(for: .milliseconds(80))
        XCTAssertEqual(center.current?.text, "second", "the first toast's timer must not close the second")
        let gone = await eventually { center.current == nil }
        XCTAssertTrue(gone)
    }
}

// MARK: - Search spans every category

final class SearchAcrossCategoriesTests: VodTestCase {
    func testMovieSearchIgnoresTheSelectedCategoryAndItComesBackAfterwards() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 12)
        let model = VodCatalogModel<IptvItemRecord>(kind: "movie", source: .movies(engine: engine))

        // Film 003 is a "Drame"; the selected category is "Action".
        model.filter.group = "Action"
        await model.reload()
        XCTAssertEqual(model.entries.count, 6)
        XCTAssertFalse(model.entries.contains { $0.nameClean == "Film 003" })
        XCTAssertFalse(model.searchIgnoresCategory)

        model.filter.text = "Film 003"
        model.filter.commit()
        await model.reload()
        XCTAssertEqual(model.entries.map(\.nameClean), ["Film 003"], "found although it is not in the selected category")
        XCTAssertEqual(model.entries.first?.groupTitle, "Drame")
        XCTAssertTrue(model.searchIgnoresCategory)

        model.filter.text = ""
        model.filter.commit()
        await model.reload()
        XCTAssertEqual(model.entries.count, 6, "the Action filter applies again once the search is cleared")
        XCTAssertTrue(model.entries.allSatisfy { $0.groupTitle == "Action" })
        XCTAssertEqual(model.filter.group, "Action", "the selection itself was never lost")
        XCTAssertFalse(model.searchIgnoresCategory)
    }

    func testSearchStillHonorsTheServerFilter() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 4)
        let model = VodCatalogModel<IptvItemRecord>(kind: "movie", source: .movies(engine: engine))
        model.filter.group = "Action"
        model.filter.host = "elsewhere.example"
        model.filter.text = "Film"
        model.filter.commit()
        await model.reload()
        XCTAssertTrue(model.entries.isEmpty, "only the category is ignored while searching, not the server filter")
    }

    func testSeriesSearchIgnoresTheSelectedCategoryToo() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 2)
        let model = VodCatalogModel<SeriesRecord>(kind: "series", source: .series(engine: engine))
        await model.reload()
        let group = try XCTUnwrap(model.groups.first?.value)

        model.filter.group = "Une autre catégorie"
        await model.reload()
        XCTAssertTrue(model.entries.isEmpty)

        model.filter.text = "Dark"
        model.filter.commit()
        await model.reload()
        XCTAssertEqual(model.entries.map(\.title), ["Dark"])
        XCTAssertEqual(model.entries.first?.groupTitle, group)
    }
}

@MainActor
final class LiveSearchAcrossCategoriesTests: XCTestCase {
    func testLiveSearchLooksInEveryCategory() async throws {
        let engine = try CndEngine.inMemory()
        let directory = try TestFixtures.makeTempDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        try await TestFixtures.importCatalog(into: engine, directory: directory, count: 30)

        let model = LiveTvModel(engine: engine)
        model.filter.group = "Sport"            // channel 17 is in "Kids"
        await model.reload()
        XCTAssertEqual(model.channels.count, 10)

        model.filter.text = "Chaine 017"
        model.filter.commit()
        await model.reload()
        XCTAssertEqual(model.channels.map(\.name), ["Chaine 017"])
        XCTAssertTrue(model.searchIgnoresCategory)

        model.filter.text = ""
        model.filter.commit()
        await model.reload()
        XCTAssertEqual(model.channels.count, 10)
        XCTAssertFalse(model.searchIgnoresCategory)
    }
}

// MARK: - Genres of films and series (PRD-33 lot 33.5)

final class VodGenreTests: VodTestCase {
    func testMoviesAreBrowsedByGenreOnTheThemeAxis() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 130)
        let model = VodCatalogModel<IptvItemRecord>(kind: "movie", source: .movies(engine: engine))
        model.axis = .theme
        await model.reloadTaxonomyFacets()

        let cinema = try XCTUnwrap(model.columnFacets.first { $0.key == "movies" })
        XCTAssertEqual(cinema.count, 130)
        XCTAssertEqual(Set(cinema.subkeys.map(\.value)), ["action", "drama"], "the provider's Action and Drame become genres")
        XCTAssertEqual(CategoryText.subthemeName("drama"), "Drame")
        XCTAssertFalse(model.crossFacets.isEmpty, "the country chips come with the theme column")

        model.filter.theme = "movies"
        model.filter.subtheme = "action"
        await model.reload()
        XCTAssertEqual(model.entries.count, 60)
        XCTAssertEqual(model.subthemeFacets.map(\.value).sorted(), ["action", "drama"])
        XCTAssertFalse(model.showsSections, "a genre is a filter: no 'resume' shelf")
        XCTAssertTrue(model.entries.allSatisfy { $0.groupTitle == "Action" })
        await model.loadNextPage()
        XCTAssertEqual(model.entries.count, 65)
    }

    func testTheProviderAxisIgnoresTheTaxonomySelection() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 12)
        let model = VodCatalogModel<IptvItemRecord>(kind: "movie", source: .movies(engine: engine))
        model.filter.theme = "documentary"
        await model.reload()
        XCTAssertEqual(model.entries.count, 12, "axis .provider: the selection of another axis is not applied")
        model.axis = .theme
        await model.reload()
        XCTAssertTrue(model.entries.isEmpty)
    }

    func testSeriesCarryTheirGenreToo() async throws {
        try await TestFixtures.importVod(into: engine, directory: directory, movies: 3)
        let model = VodCatalogModel<SeriesRecord>(kind: "series", source: .series(engine: engine))
        model.axis = .theme
        await model.reloadTaxonomyFacets()
        let series = try XCTUnwrap(model.columnFacets.first { $0.key == "series" })
        XCTAssertEqual(series.count, 2)
        model.filter.theme = "series"
        await model.reload()
        XCTAssertEqual(model.entries.count, 2)
    }
}
