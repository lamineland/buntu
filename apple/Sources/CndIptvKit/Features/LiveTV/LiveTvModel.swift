import Foundation
import Observation
import CndCoreBridge

enum LiveViewMode: String, CaseIterable, Identifiable {
    case grid
    case list

    var id: String { rawValue }
}

/// Data behind the Live TV screen: paged catalog, Recents / Favorites sections and the
/// "now playing" EPG of the channels currently on screen.
///
/// Every engine call runs off the main actor; a generation counter drops results that belong to a
/// previous query so fast typing or scrolling can never show stale rows.
@Observable
@MainActor
final class LiveTvModel {
    nonisolated static let pageSize = 60
    nonisolated static let epgBatchSize = 60
    nonisolated static let recentsLimit: UInt32 = 20
    nonisolated static let epgRefreshCap = 240

    let engine: CndEngine
    let filter: SearchFilterState
    @ObservationIgnored var onFavoriteChanged: () -> Void = {}

    private(set) var groups: [FacetCount] = []
    /// Axis of the category column (PRD-33). `.provider` is the provider's own list (`groups`).
    var axis: CategoryAxis = .provider
    /// Entries of the column for the country / theme axes, and of the chips for the other axis.
    private(set) var columnFacets: [CategoryFacet] = []
    private(set) var crossFacets: [CategoryFacet] = []
    /// "Toutes" counter of the column: what the current selection of the *other* axis leaves.
    private(set) var allCount: Int?
    private(set) var hosts: [FacetCount] = []
    private(set) var recents: [IptvItemRecord] = []
    private(set) var favorites: [IptvItemRecord] = []
    private(set) var channels: [IptvItemRecord] = []
    private(set) var hasMore = false
    private(set) var isLoading = false
    private(set) var nowNext: [String: ChannelNowNext] = [:]
    private(set) var isLoadingEpg = false
    private(set) var errorMessage: String?

    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var loadedFacetsRevision: Int?
    @ObservationIgnored private var isLoadingMore = false
    @ObservationIgnored private var requestedEpg = Set<String>()
    @ObservationIgnored private var requestedOrder: [String] = []
    @ObservationIgnored private var pendingEpg: [String] = []
    @ObservationIgnored private var epgFlushTask: Task<Void, Never>?

    init(engine: CndEngine, filter: SearchFilterState? = nil) {
        self.engine = engine
        self.filter = filter ?? SearchFilterState(sort: .channelNumber)
    }

    /// Recents and Favorites are only shown when nothing narrows the catalog.
    var showsSections: Bool {
        filter.query.isEmpty && filter.group == nil && !filter.hasTaxonomySelection && filter.host == nil && !filter.showBroken
    }

    /// A search looks in every category; the selected one comes back when the search is cleared.
    var searchIgnoresCategory: Bool {
        !filter.query.isEmpty && (filter.group != nil || filter.hasTaxonomySelection)
    }

    /// Country / theme restriction of the catalogue query (none while searching, like the provider group).
    var taxonomyFilter: TaxonomyFilter {
        guard filter.query.isEmpty, axis != .provider else { return TaxonomyFilter(country: nil, theme: nil, subtheme: nil) }
        return TaxonomyFilter(country: filter.country, theme: filter.theme, subtheme: filter.theme == nil ? nil : filter.subtheme)
    }

    private func makeFilter(offset: Int) -> CatalogFilter {
        CatalogFilter(
            kind: "live",
            sourceId: nil,
            query: filter.query,
            groupTitle: filter.query.isEmpty && axis == .provider ? filter.group : nil,
            host: filter.host,
            favoritesOnly: false,
            includeBroken: filter.showBroken,
            includeHidden: false,
            sortBy: filter.sort,
            sortAsc: filter.ascending,
            limit: UInt32(Self.pageSize),
            offset: UInt32(offset)
        )
    }

    private struct Snapshot: Sendable {
        var page: [IptvItemRecord]
        var groups: [FacetCount]?
        var hosts: [FacetCount]?
        var recents: [IptvItemRecord]
        var favorites: [IptvItemRecord]
    }

    // MARK: Loading

    /// Runs the first query. `revision` identifies the catalog snapshot: group and host facets are
    /// only re-read when it changed, not on every keystroke.
    func reload(revision: Int = 0) async {
        generation += 1
        let gen = generation
        channels = []
        hasMore = false
        isLoadingMore = false
        errorMessage = nil
        requestedEpg = []
        requestedOrder = []
        pendingEpg = []
        epgFlushTask?.cancel()
        epgFlushTask = nil
        isLoadingEpg = false
        isLoading = true

        let engine = self.engine
        let first = makeFilter(offset: 0)
        let taxonomy = taxonomyFilter
        let wantsSections = showsSections
        let wantsFacets = loadedFacetsRevision != revision
        do {
            let snapshot = try await Task.detached(priority: .userInitiated) { () -> Snapshot in
                Snapshot(
                    page: try engine.queryCatalogByTaxonomy(filter: first, taxonomy: taxonomy),
                    groups: wantsFacets ? try engine.getGroups(kind: "live", sourceId: nil) : nil,
                    hosts: wantsFacets ? try engine.getHosts(kind: "live", sourceId: nil) : nil,
                    recents: wantsSections ? try engine.listRecentItems(kind: "live", limit: Self.recentsLimit) : [],
                    favorites: wantsSections ? try engine.listFavoriteItems(kind: "live") : []
                )
            }.value
            guard gen == generation else { return }
            channels = snapshot.page
            hasMore = snapshot.page.count >= Self.pageSize
            recents = snapshot.recents
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

    // MARK: Country / theme facets (PRD-33)

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
        let engine = self.engine
        let axis = self.axis
        let selection = TaxonomyFilter(country: filter.country, theme: filter.theme, subtheme: filter.subtheme)
        let wantsAllCountryTotal = axis == .theme && filter.country != nil
        do {
            let snapshot = try await Task.detached(priority: .userInitiated) { () -> FacetSnapshot in
                let column = try engine.getCategoryFacets(kind: "live", axis: axis, sourceId: nil, filter: selection)
                let cross = try engine.getCategoryFacets(kind: "live", axis: other, sourceId: nil, filter: selection)
                var all: Int?
                if axis == .country {
                    all = column.reduce(0) { $0 + Int($1.count) }
                } else if wantsAllCountryTotal {
                    // The theme column counts what the chosen country leaves, whatever theme is picked.
                    let countries = try engine.getCategoryFacets(
                        kind: "live", axis: .country, sourceId: nil,
                        filter: TaxonomyFilter(country: nil, theme: nil, subtheme: nil)
                    )
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

    /// Sub-themes of the selected theme (football, comedy…), for the second chips row.
    var subthemeFacets: [FacetCount] {
        guard axis == .theme || filter.theme != nil, let theme = filter.theme else { return [] }
        let source = axis == .theme ? columnFacets : crossFacets
        return source.first { $0.key == theme }?.subkeys ?? []
    }

    func loadNextPage() async {
        guard hasMore, !isLoading, !isLoadingMore else { return }
        isLoadingMore = true
        let gen = generation
        let engine = self.engine
        let next = makeFilter(offset: channels.count)
        let taxonomy = taxonomyFilter
        do {
            let page = try await Task.detached(priority: .userInitiated) {
                try engine.queryCatalogByTaxonomy(filter: next, taxonomy: taxonomy)
            }.value
            guard gen == generation else { return }
            channels.append(contentsOf: page)
            hasMore = page.count >= Self.pageSize
        } catch {
            guard gen == generation else { return }
            errorMessage = userMessage(for: error)
        }
        isLoadingMore = false
    }

    /// Infinite scroll: asks for the next page when one of the last rows becomes visible.
    func loadMoreIfNeeded(current item: IptvItemRecord) {
        guard hasMore, let index = channels.lastIndex(where: { $0.id == item.id }),
              index >= channels.count - 10 else { return }
        Task { await loadNextPage() }
    }

    // MARK: EPG now/next (batched)

    func cardAppeared(_ item: IptvItemRecord) {
        guard let id = item.tvgId?.trimmingCharacters(in: .whitespaces), !id.isEmpty,
              requestedEpg.insert(id).inserted else { return }
        requestedOrder.append(id)
        pendingEpg.append(id)
        scheduleEpgFlush(after: .milliseconds(40))
    }

    /// Re-queries the most recently shown channels so programme titles and gauges stay current.
    func refreshEpg() {
        pendingEpg = Array(requestedOrder.suffix(Self.epgRefreshCap))
        scheduleEpgFlush(after: .zero)
    }

    private func scheduleEpgFlush(after delay: Duration) {
        guard epgFlushTask == nil else { return }
        isLoadingEpg = true
        epgFlushTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled, let self else { return }
            await self.flushEpg()
        }
    }

    private func flushEpg() async {
        epgFlushTask = nil
        guard !pendingEpg.isEmpty else {
            isLoadingEpg = false
            return
        }
        let ids = Array(pendingEpg.prefix(Self.epgBatchSize))
        pendingEpg.removeFirst(ids.count)
        let gen = generation
        let engine = self.engine
        let results = try? await Task.detached(priority: .userInitiated) { try engine.getEpgNowNext(tvgIds: ids) }.value
        guard gen == generation else { return }
        for entry in results ?? [] { nowNext[entry.channelTvgId] = entry }
        if pendingEpg.isEmpty {
            isLoadingEpg = false
        } else {
            scheduleEpgFlush(after: .zero)
        }
    }

    func program(for item: IptvItemRecord) -> ChannelNowNext? {
        item.tvgId.flatMap { nowNext[$0] }
    }

    // MARK: Actions

    func toggleFavorite(_ item: IptvItemRecord) async {
        let engine = self.engine
        do {
            let isFavorite = try await Task.detached(priority: .userInitiated) {
                try engine.toggleFavorite(stableHash: item.stableHash, itemKind: item.kind)
            }.value
            apply(isFavorite: isFavorite, to: item)
            onFavoriteChanged()
        } catch {
            errorMessage = userMessage(for: error)
        }
    }

    func toggleBroken(_ item: IptvItemRecord) async {
        let engine = self.engine
        do {
            let isBroken = try await Task.detached(priority: .userInitiated) {
                try engine.toggleBroken(stableHash: item.stableHash, itemKind: item.kind)
            }.value
            apply(isBroken: isBroken, to: item)
        } catch {
            errorMessage = userMessage(for: error)
        }
    }

    private func apply(isBroken: Bool, to item: IptvItemRecord) {
        if !filter.showBroken && isBroken {
            channels.removeAll { $0.stableHash == item.stableHash }
            recents.removeAll { $0.stableHash == item.stableHash }
            favorites.removeAll { $0.stableHash == item.stableHash }
        } else {
            for index in channels.indices where channels[index].stableHash == item.stableHash {
                channels[index].isBroken = isBroken
            }
            for index in recents.indices where recents[index].stableHash == item.stableHash {
                recents[index].isBroken = isBroken
            }
            for index in favorites.indices where favorites[index].stableHash == item.stableHash {
                favorites[index].isBroken = isBroken
            }
        }
    }

    private func apply(isFavorite: Bool, to item: IptvItemRecord) {
        for index in channels.indices where channels[index].stableHash == item.stableHash {
            channels[index].isFavorite = isFavorite
        }
        for index in recents.indices where recents[index].stableHash == item.stableHash {
            recents[index].isFavorite = isFavorite
        }
        favorites.removeAll { $0.stableHash == item.stableHash }
        if isFavorite {
            var added = item
            added.isFavorite = true
            favorites.insert(added, at: 0)
        }
    }

    /// Records the channel in the history (a live stream has no position) and moves it to the front
    /// of the Recents section without re-querying.
    func markWatched(_ item: IptvItemRecord) {
        let engine = self.engine
        Task.detached(priority: .utility) {
            try? engine.recordWatchProgress(stableHash: item.stableHash, positionMs: 0, durationMs: 0)
        }
        recents.removeAll { $0.stableHash == item.stableHash }
        recents.insert(item, at: 0)
        if recents.count > Int(Self.recentsLimit) { recents.removeLast(recents.count - Int(Self.recentsLimit)) }
    }
}
