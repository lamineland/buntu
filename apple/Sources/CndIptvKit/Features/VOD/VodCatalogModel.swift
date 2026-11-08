import Foundation
import Observation
import CndCoreBridge

/// A catalog row the generic VOD list can page through (a movie or a normalised series).
protocol CatalogEntry: Sendable, Hashable {
    var entryId: Int64 { get }
    var entryStableHash: String { get }
    var entryIsFavorite: Bool { get set }
    var entryIsBroken: Bool { get set }
    /// `itemKind` passed to `toggleFavorite`.
    var favoriteKind: String { get }
}

extension IptvItemRecord: CatalogEntry {
    var entryId: Int64 { id }
    var entryStableHash: String { stableHash }
    var entryIsFavorite: Bool {
        get { isFavorite }
        set { isFavorite = newValue }
    }
    var entryIsBroken: Bool {
        get { isBroken }
        set { isBroken = newValue }
    }
    var favoriteKind: String { kind }
}

extension SeriesRecord: CatalogEntry {
    var entryId: Int64 { id }
    var entryStableHash: String { stableHash }
    var entryIsFavorite: Bool {
        get { isFavorite }
        set { isFavorite = newValue }
    }
    var entryIsBroken: Bool {
        get { isBroken }
        set { isBroken = newValue }
    }
    var favoriteKind: String { "series" }
}

/// How a model reaches the engine for one kind of entry; closures keep the model generic and let
/// tests plug in anything.
struct VodSource<Entry: CatalogEntry>: Sendable {
    var query: @Sendable (CatalogFilter, TaxonomyFilter) throws -> [Entry]
    /// Entries of the country / theme axes (PRD-33 lot 33.5), with the other axis' selection applied.
    var facets: @Sendable (CategoryAxis, TaxonomyFilter) throws -> [CategoryFacet]
    var resume: @Sendable () throws -> [Entry]
    var favorites: @Sendable () throws -> [Entry]
    var groups: @Sendable () throws -> [FacetCount]
    var hosts: @Sendable () throws -> [FacetCount]
}

extension VodSource where Entry == IptvItemRecord {
    static func movies(engine: CndEngine) -> VodSource {
        VodSource(
            query: { try engine.queryCatalogByTaxonomy(filter: $0, taxonomy: $1) },
            facets: { try engine.getCategoryFacets(kind: "movie", axis: $0, sourceId: nil, filter: $1) },
            // "Continue watching": started and not finished.
            resume: {
                try engine.listRecentItems(kind: "movie", limit: 40)
                    .filter { $0.resumePositionMs > 0 && !$0.isCompleted }
                    .prefix(20).map { $0 }
            },
            favorites: { try engine.listFavoriteItems(kind: "movie") },
            groups: { try engine.getGroups(kind: "movie", sourceId: nil) },
            hosts: { try engine.getHosts(kind: "movie", sourceId: nil) }
        )
    }
}

extension VodSource where Entry == SeriesRecord {
    static func series(engine: CndEngine) -> VodSource {
        VodSource(
            query: { try engine.querySeriesByTaxonomy(filter: $0, taxonomy: $1) },
            facets: { try engine.getCategoryFacets(kind: "series", axis: $0, sourceId: nil, filter: $1) },
            resume: { try engine.listRecentSeries(limit: 20) },
            favorites: { try engine.listFavoriteSeries() },
            groups: { try engine.getGroups(kind: "series", sourceId: nil) },
            hosts: { [] }
        )
    }
}

/// Paged, filterable catalog shared by Movies and Series. Same guarantees as `LiveTvModel`: engine
/// calls run off the main actor and a generation counter drops stale results.
@Observable
@MainActor
final class VodCatalogModel<Entry: CatalogEntry> {
    nonisolated static var pageSize: Int { 60 }

    let kind: String
    let source: VodSource<Entry>
    let filter: SearchFilterState
    /// Axis of the category column (PRD-33). `.provider` is the provider's own list (`groups`).
    var axis: CategoryAxis = .provider

    private(set) var columnFacets: [CategoryFacet] = []
    private(set) var crossFacets: [CategoryFacet] = []
    /// "Tous" counter of the column when a selection of the other axis narrows it.
    private(set) var allCount: Int?
    private(set) var entries: [Entry] = []
    private(set) var resume: [Entry] = []
    private(set) var favorites: [Entry] = []
    private(set) var groups: [FacetCount] = []
    private(set) var hosts: [FacetCount] = []
    private(set) var hasMore = false
    private(set) var isLoading = false
    private(set) var errorMessage: String?

    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var loadedFacetsRevision: Int?
    @ObservationIgnored private var isLoadingMore = false

    init(kind: String, source: VodSource<Entry>, filter: SearchFilterState? = nil) {
        self.kind = kind
        self.source = source
        self.filter = filter ?? SearchFilterState(sort: .name)
    }

    var showsSections: Bool {
        filter.query.isEmpty && filter.group == nil && !filter.hasTaxonomySelection && filter.host == nil && !filter.showBroken
    }

    /// A search looks in every category: the selected one is only applied while the search is empty,
    /// and comes back as soon as it is cleared.
    var searchIgnoresCategory: Bool {
        !filter.query.isEmpty && (filter.group != nil || filter.hasTaxonomySelection)
    }

    var taxonomyFilter: TaxonomyFilter {
        guard filter.query.isEmpty, axis != .provider else { return TaxonomyFilter(country: nil, theme: nil, subtheme: nil) }
        return TaxonomyFilter(country: filter.country, theme: filter.theme, subtheme: filter.theme == nil ? nil : filter.subtheme)
    }

    private func makeFilter(offset: Int) -> CatalogFilter {
        CatalogFilter(
            kind: kind, sourceId: nil, query: filter.query,
            groupTitle: filter.query.isEmpty && axis == .provider ? filter.group : nil, host: filter.host,
            favoritesOnly: false, includeBroken: filter.showBroken, includeHidden: false, sortBy: filter.sort, sortAsc: filter.ascending,
            limit: UInt32(Self.pageSize), offset: UInt32(offset)
        )
    }

    private struct Snapshot: Sendable {
        var page: [Entry]
        var resume: [Entry]
        var favorites: [Entry]
        var groups: [FacetCount]?
        var hosts: [FacetCount]?
    }

    func reload(revision: Int = 0) async {
        generation += 1
        let gen = generation
        entries = []
        hasMore = false
        isLoadingMore = false
        errorMessage = nil
        isLoading = true

        let source = self.source
        let first = makeFilter(offset: 0)
        let taxonomy = taxonomyFilter
        let sections = showsSections
        let facets = loadedFacetsRevision != revision
        do {
            let snapshot = try await Task.detached(priority: .userInitiated) { () -> Snapshot in
                Snapshot(
                    page: try source.query(first, taxonomy),
                    resume: sections ? try source.resume() : [],
                    favorites: sections ? try source.favorites() : [],
                    groups: facets ? try source.groups() : nil,
                    hosts: facets ? try source.hosts() : nil
                )
            }.value
            guard gen == generation else { return }
            entries = snapshot.page
            hasMore = snapshot.page.count >= Self.pageSize
            resume = snapshot.resume
            favorites = snapshot.favorites
            if let groups = snapshot.groups, let hosts = snapshot.hosts {
                self.groups = groups
                self.hosts = hosts
                loadedFacetsRevision = revision
            }
            isLoading = false
        } catch {
            guard gen == generation else { return }
            errorMessage = userMessage(for: error)
            isLoading = false
        }
    }

    // MARK: Country / theme facets (PRD-33 lot 33.5)

    private struct FacetSnapshot: Sendable {
        var column: [CategoryFacet]
        var cross: [CategoryFacet]
        var allCount: Int?
    }

    /// Reads the column and the chips of the current axis. Called when the axis, the selection or the
    /// catalogue changes, not on every keystroke of the search. Does nothing on the provider axis.
    func reloadTaxonomyFacets() async {
        guard axis != .provider, let other = axis.other else {
            columnFacets = []
            crossFacets = []
            allCount = nil
            return
        }
        let source = self.source
        let axis = self.axis
        let selection = TaxonomyFilter(country: filter.country, theme: filter.theme, subtheme: filter.subtheme)
        let wantsAllCountryTotal = axis == .theme && filter.country != nil
        do {
            let snapshot = try await Task.detached(priority: .userInitiated) { () -> FacetSnapshot in
                let column = try source.facets(axis, selection)
                let cross = try source.facets(other, selection)
                var all: Int?
                if axis == .country {
                    all = column.reduce(0) { $0 + Int($1.count) }
                } else if wantsAllCountryTotal {
                    let countries = try source.facets(.country, TaxonomyFilter(country: nil, theme: nil, subtheme: nil))
                    all = countries.first { $0.key == selection.country }.map { Int($0.count) } ?? 0
                }
                return FacetSnapshot(column: column, cross: cross, allCount: all)
            }.value
            guard axis == self.axis else { return }
            columnFacets = CategoryText.sorted(snapshot.column, axis: axis)
            crossFacets = CategoryText.sorted(snapshot.cross, axis: other)
            allCount = snapshot.allCount
        } catch {
            errorMessage = userMessage(for: error)
        }
    }

    /// Genres of the selected theme (action, comedy…), for the second chips row.
    var subthemeFacets: [FacetCount] {
        guard axis == .theme || filter.theme != nil, let theme = filter.theme else { return [] }
        let source = axis == .theme ? columnFacets : crossFacets
        return source.first { $0.key == theme }?.subkeys ?? []
    }

    func loadNextPage() async {
        guard hasMore, !isLoading, !isLoadingMore else { return }
        isLoadingMore = true
        let gen = generation
        let source = self.source
        let next = makeFilter(offset: entries.count)
        let taxonomy = taxonomyFilter
        do {
            let page = try await Task.detached(priority: .userInitiated) { try source.query(next, taxonomy) }.value
            guard gen == generation else { return }
            entries.append(contentsOf: page)
            hasMore = page.count >= Self.pageSize
        } catch {
            guard gen == generation else { return }
            errorMessage = userMessage(for: error)
        }
        isLoadingMore = false
    }

    func loadMoreIfNeeded(current entry: Entry) {
        guard hasMore, let index = entries.lastIndex(where: { $0.entryId == entry.entryId }),
              index >= entries.count - 12 else { return }
        Task { await loadNextPage() }
    }

    /// Mirrors a favorite change made anywhere (card, details) into the loaded lists.
    func applyFavorite(_ isFavorite: Bool, to entry: Entry) {
        func update(_ list: inout [Entry]) {
            for index in list.indices where list[index].entryStableHash == entry.entryStableHash {
                list[index].entryIsFavorite = isFavorite
            }
        }
        update(&entries)
        update(&resume)
        favorites.removeAll { $0.entryStableHash == entry.entryStableHash }
        if isFavorite {
            var added = entry
            added.entryIsFavorite = true
            favorites.insert(added, at: 0)
        }
    }

    /// Toggles the broken flag on an entry.
    func toggleBroken(_ entry: Entry, engine: CndEngine) async {
        do {
            let isBroken = try await Task.detached(priority: .userInitiated) {
                try engine.toggleBroken(stableHash: entry.entryStableHash, itemKind: entry.favoriteKind)
            }.value
            applyBroken(isBroken, to: entry)
        } catch {
            errorMessage = userMessage(for: error)
        }
    }

    /// Mirrors a broken flag change made anywhere into the loaded lists.
    func applyBroken(_ isBroken: Bool, to entry: Entry) {
        if !filter.showBroken && isBroken {
            entries.removeAll { $0.entryStableHash == entry.entryStableHash }
            resume.removeAll { $0.entryStableHash == entry.entryStableHash }
            favorites.removeAll { $0.entryStableHash == entry.entryStableHash }
        } else {
            func update(_ list: inout [Entry]) {
                for index in list.indices where list[index].entryStableHash == entry.entryStableHash {
                    list[index].entryIsBroken = isBroken
                }
            }
            update(&entries)
            update(&resume)
            update(&favorites)
        }
    }
}
