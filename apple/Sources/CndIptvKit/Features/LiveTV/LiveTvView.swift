import SwiftUI
import CndCoreBridge

struct LiveTvView: View {
    @Environment(\.uiMetrics) private var ui
    @State private var model: LiveTvModel
    let revision: Int
    let totalChannels: Int
    let playingURL: String?
    let onPlay: (IptvItemRecord) -> Void
    let onShowGuide: (IptvItemRecord) -> Void
    var zapper: LiveZapper?
    var onOpenAssistant: (() -> Void)?
    var onAddToMultiView: ((IptvItemRecord) -> Void)? = nil
    var onOpenMultiView: (() -> Void)? = nil
    var onStartOver: ((IptvItemRecord, EpgProgramRecord) -> Void)? = nil
    /// A country, a theme or a correction changed what the catalogue shows (PRD-33).
    var onCategoriesChanged: () -> Void = {}
    @Binding var searchRequest: String?

    @AppStorage("live.viewMode") private var viewModeRaw = LiveViewMode.grid.rawValue
    @AppStorage("live.categoryAxis") private var axisRaw = CategoryAxis.theme.storageValue
    @State private var mirrorTarget: IptvItemRecord?
    @State private var mirrorList: [IptvItemRecord] = []
    @State private var correction: TaxonomyCorrectionModel?
    init(
        engine: CndEngine,
        revision: Int,
        totalChannels: Int,
        playingURL: String?,
        onPlay: @escaping (IptvItemRecord) -> Void,
        onShowGuide: @escaping (IptvItemRecord) -> Void,
        onFavoriteChanged: @escaping () -> Void,
        zapper: LiveZapper? = nil,
        onOpenAssistant: (() -> Void)? = nil,
        onAddToMultiView: ((IptvItemRecord) -> Void)? = nil,
        onOpenMultiView: (() -> Void)? = nil,
        onStartOver: ((IptvItemRecord, EpgProgramRecord) -> Void)? = nil,
        onCategoriesChanged: @escaping () -> Void = {},
        searchRequest: Binding<String?> = .constant(nil)
    ) {
        let model = LiveTvModel(engine: engine)
        model.onFavoriteChanged = onFavoriteChanged
        _model = State(initialValue: model)
        self.revision = revision
        self.totalChannels = totalChannels
        self.playingURL = playingURL
        self.onPlay = onPlay
        self.onShowGuide = onShowGuide
        self.zapper = zapper
        self.onOpenAssistant = onOpenAssistant
        self.onAddToMultiView = onAddToMultiView
        self.onOpenMultiView = onOpenMultiView
        self.onStartOver = onStartOver
        self.onCategoriesChanged = onCategoriesChanged
        _searchRequest = searchRequest
    }

    /// A "search" decided by the assistant: shown here, across every category.
    private func applySearchRequest() {
        guard let query = searchRequest else { return }
        searchRequest = nil
        model.filter.group = nil
        model.filter.text = query
        model.filter.commit()
    }

    private var viewMode: LiveViewMode { LiveViewMode(rawValue: viewModeRaw) ?? .grid }

    private var reloadKey: String { "\(model.filter.signature)#\(revision)#\(axisRaw)" }

    // MARK: Categories by country / theme (PRD-33)

    private var axis: CategoryAxis { CategoryAxis(storageValue: axisRaw, fallback: .theme) }

    private var axisBinding: Binding<CategoryAxis> {
        Binding(get: { axis }, set: { axisRaw = $0.storageValue })
    }

    /// Facets are re-read when the axis, the selection or the catalogue changes (not for the search text).
    private var facetsKey: String {
        let f = model.filter
        return "\(axisRaw)|\(f.country ?? "-")|\(f.theme ?? "-")|\(f.subtheme ?? "-")|\(revision)"
    }

    private var axisPicker: AnyView {
        AnyView(
            Picker("Classer par", selection: axisBinding) {
                ForEach(CategoryAxis.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .accessibilityLabel("Classer les catégories par")
        )
    }

    /// Selection of the column, which depends on the axis.
    private func columnSelection(_ filter: SearchFilterState) -> Binding<String?> {
        switch axis {
        case .provider: return Binding(get: { filter.group }, set: { filter.group = $0 })
        case .country: return countryBinding(filter)
        case .theme: return themeBinding(filter)
        }
    }

    private func countryBinding(_ filter: SearchFilterState) -> Binding<String?> {
        Binding(get: { filter.country }, set: { filter.country = $0 })
    }

    /// Changing the theme drops the sub-theme, which belongs to the previous one.
    private func themeBinding(_ filter: SearchFilterState) -> Binding<String?> {
        Binding(get: { filter.theme }, set: { filter.theme = $0; filter.subtheme = nil })
    }

    private func crossSelection(_ filter: SearchFilterState) -> Binding<String?> {
        axis == .country ? themeBinding(filter) : countryBinding(filter)
    }

    /// Context menu of a column row: hide a country or theme, or classify a provider category.
    private func rowMenu(for key: String) -> AnyView {
        switch axis {
        case .country:
            return AnyView(Button("Masquer ce pays", systemImage: "eye.slash") { hide(kind: "country:live", key: key) })
        case .theme:
            return AnyView(Button("Masquer ce thème", systemImage: "eye.slash") { hide(kind: "theme:live", key: key) })
        case .provider:
            return AnyView(Button("Classer cette catégorie…", systemImage: "globe.europe.africa") {
                correction = TaxonomyCorrectionModel(engine: model.engine, target: .providerCategory(key))
            })
        }
    }

    /// Hides a country or a theme for the active profile; the bouquet editor shows it again.
    private func hide(kind: String, key: String) {
        try? model.engine.hideGroup(profileId: model.engine.activeProfileId(), kind: kind, group: key)
        if axis == .country, model.filter.country == key { model.filter.country = nil }
        if axis == .theme, model.filter.theme == key { model.filter.theme = nil; model.filter.subtheme = nil }
        categoriesChanged()
    }

    private func categoriesChanged() {
        onCategoriesChanged()
        Task {
            await model.reload(revision: revision)
            await model.reloadTaxonomyFacets()
        }
    }

    /// Chips of the other axis, the optional sub-theme row and the title of the selection.
    @ViewBuilder
    private func crossFilters(_ filter: SearchFilterState) -> some View {
        if let other = axis.other {
            VStack(alignment: .leading, spacing: 0) {
                FacetChipsRow(
                    allTitle: other == .theme ? "Tous les thèmes" : "Tous les pays",
                    chips: model.crossFacets.map {
                        FacetChip(key: $0.key, title: CategoryText.label($0.key, axis: other), count: Int($0.count))
                    },
                    selection: crossSelection(filter)
                )
                if !model.subthemeFacets.isEmpty {
                    FacetChipsRow(
                        allTitle: "Tous les sous-thèmes",
                        chips: model.subthemeFacets.map {
                            FacetChip(key: $0.value, title: CategoryText.subthemeName($0.value), count: Int($0.count))
                        },
                        selection: Binding(get: { filter.subtheme }, set: { filter.subtheme = $0 })
                    )
                }
                if let title = CategoryText.selectionTitle(country: filter.country, theme: filter.theme, subtheme: filter.subtheme) {
                    Text(title)
                        .font(.headline)
                        .padding(.horizontal, 16)
                        .padding(.bottom, 4)
                        .accessibilityAddTraits(.isHeader)
                }
            }
        }
    }

    /// Hands the browsed channel list to the zapper (next/previous channel from anywhere).
    private func publishZapList() {
        zapper?.update(channels: model.channels) { [model] item in model.program(for: item)?.now?.title }
    }

    private var actions: ChannelActions {
        ChannelActions(
            play: { item in
                model.markWatched(item)
                onPlay(item)
            },
            toggleFavorite: { item in
                Haptics.light()
                Task { await model.toggleFavorite(item) }
            },
            toggleBroken: { item in
                Haptics.light()
                Task { await model.toggleBroken(item) }
            },
            addToMultiView: onAddToMultiView,
            showMirrors: { item in
                Task {
                    let list = (try? model.engine.findMirrors(itemId: item.id)) ?? []
                    mirrorList = list
                    mirrorTarget = item
                }
            },
            startOver: onStartOver,
            showGuide: onShowGuide,
            correctCategory: { item in
                correction = TaxonomyCorrectionModel(engine: model.engine, target: .channel(stableHash: item.stableHash, name: item.name))
            }
        )
    }

    var body: some View {
        @Bindable var filter = model.filter
        let axis = self.axis
        return CategoryNavigation(
            groups: axis == .provider ? model.groups : CategoryText.asFacetCounts(model.columnFacets),
            totalCount: axis == .provider ? totalChannels : (model.allCount ?? totalChannels),
            allTitle: "Toutes les chaînes",
            label: { CategoryText.label($0, axis: axis) },
            icon: { CategoryText.icon($0, axis: axis) },
            axisPicker: axisPicker,
            rowMenu: { key in rowMenu(for: key) },
            selection: columnSelection(model.filter)
        ) {
            VStack(spacing: 0) {
                crossFilters(model.filter)
                channelContent
            }
        }
        .navigationTitle("Live TV")
        .searchFilterToolbar(
            state: model.filter,
            groups: axis == .provider ? model.groups : [],
            hosts: model.hosts,
            sortOptions: [.channelNumber, .name, .group, .host],
            prompt: "Rechercher une chaîne"
        )
        .onAppear { applySearchRequest() }
        .onChange(of: searchRequest) { _, _ in applySearchRequest() }
        .toolbar {
            if let onOpenAssistant {
                ToolbarItem(placement: .automatic) {
                    Button(action: onOpenAssistant) { Label("Assistant", systemImage: "sparkles") }
                        .help("Changer de chaîne en langage naturel (⌘K)")
                }
            }
            if let onOpenMultiView {
                ToolbarItem(placement: .automatic) {
                    Button(action: onOpenMultiView) { Label("Mosaïque", systemImage: "square.grid.2x2") }
                        .help("Ouvrir la mosaïque de flux (Multi-View)")
                }
            }
            ToolbarItem(placement: .automatic) {
                Picker("Affichage", selection: $viewModeRaw) {
                    Label("Grille", systemImage: "square.grid.2x2").tag(LiveViewMode.grid.rawValue)
                    Label("Liste", systemImage: "list.bullet").tag(LiveViewMode.list.rawValue)
                }
                .pickerStyle(.segmented)
            }
        }
        .onChange(of: axisRaw) { _, _ in
            model.filter.group = nil
            model.filter.clearTaxonomy()
        }
        .task(id: reloadKey) {
            model.axis = axis
            await model.reload(revision: revision)
        }
        .task(id: facetsKey) {
            model.axis = axis
            await model.reloadTaxonomyFacets()
        }
        .onChange(of: model.channels.count) { _, _ in publishZapList() }
        .onChange(of: model.nowNext.count) { _, _ in publishZapList() }
        .task {
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(60))
                model.refreshEpg()
            }
        }
        .sheet(item: $correction) { model in
            TaxonomyCorrectionSheet(model: model) { changed in
                correction = nil
                if changed { categoriesChanged() }
            }
        }
        .sheet(item: Binding(
            get: { mirrorTarget.map { IdentifiableLiveItem(item: $0) } },
            set: { mirrorTarget = $0?.item }
        )) { wrapper in
            MirrorSheetView(
                target: wrapper.item,
                mirrors: mirrorList,
                onPlay: { mirror in
                    model.markWatched(mirror)
                    onPlay(mirror)
                }
            )
        }
    }

    // MARK: Channels

    @ViewBuilder
    private var channelContent: some View {
        if model.channels.isEmpty && !model.isLoading && model.recents.isEmpty && model.favorites.isEmpty {
            emptyState
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 18) {
                    if let message = model.errorMessage {
                        Label(message, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
                    }
                    if model.searchIgnoresCategory {
                        Label("Recherche dans toutes les catégories", systemImage: "magnifyingglass")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                    if model.showsSections {
                        horizontalSection("Récemment consultés", icon: "clock", items: model.recents)
                        horizontalSection("Favoris", icon: "star.fill", items: model.favorites)
                        if !model.recents.isEmpty || !model.favorites.isEmpty {
                            Text("Toutes les chaînes").font(.title3.bold())
                        }
                    }
                    switch viewMode {
                    case .grid: grid
                    case .list: list
                    }
                    if model.hasMore {
                        ProgressView()
                            .frame(maxWidth: .infinity)
                            .padding()
                            .onAppear { Task { await model.loadNextPage() } }
                    }
                }
                .padding(16)
            }
            .overlay(alignment: .topTrailing) {
                if model.isLoadingEpg {
                    ProgressView().controlSize(.small).padding(10)
                }
            }
        }
    }

    private var emptyState: some View {
        Group {
            if model.filter.query.isEmpty && !model.filter.hasActiveFilter {
                ContentUnavailableView(
                    "Aucune chaîne",
                    systemImage: "tv.slash",
                    description: Text("Ajoutez une source dans l'onglet Sources pour importer des chaînes.")
                )
            } else {
                ContentUnavailableView.search(text: model.filter.query)
            }
        }
    }

    private func card(_ item: IptvItemRecord, style: ChannelCardView.Style) -> some View {
        ChannelCardView(
            item: item,
            nowNext: model.program(for: item),
            isPlaying: item.streamUrl == playingURL,
            style: style,
            actions: actions
        )
        .onAppear {
            model.cardAppeared(item)
            model.loadMoreIfNeeded(current: item)
        }
    }

    private var grid: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: ui.channelCardMinWidth, maximum: ui.channelCardMaxWidth), spacing: ui.spacingM)], spacing: ui.spacingM) {
            ForEach(model.channels, id: \.id) { card($0, style: .card) }
        }
    }

    private var list: some View {
        LazyVStack(spacing: 0) {
            ForEach(model.channels, id: \.id) { item in
                card(item, style: .row)
                Divider()
            }
        }
    }

    @ViewBuilder
    private func horizontalSection(_ title: String, icon: String, items: [IptvItemRecord]) -> some View {
        if !items.isEmpty {
            VStack(alignment: .leading, spacing: 8) {
                Label(title, systemImage: icon).font(.title3.bold())
                HorizontalShelf(items: items, spacing: 12) { item in
                    card(item, style: .card).frame(width: ui.shelfChannelCardWidth)
                }
            }
        }
    }
}

private struct IdentifiableLiveItem: Identifiable {
    let item: IptvItemRecord
    var id: Int64 { item.id }
}

private struct MirrorSheetView: View {
    let target: IptvItemRecord
    let mirrors: [IptvItemRecord]
    let onPlay: (IptvItemRecord) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                Section {
                    HStack {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(target.name).font(.headline)
                            if let group = target.groupTitle {
                                Text(group).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        Spacer()
                        Text("Actuel")
                            .font(.caption.bold())
                            .padding(.horizontal, 8)
                            .padding(.vertical, 3)
                            .background(Color.secondary.opacity(0.2), in: Capsule())
                    }
                } header: {
                    Text("Chaîne sélectionnée")
                }

                Section {
                    if mirrors.isEmpty {
                        Text("Aucun flux miroir trouvé pour cette chaîne.")
                            .foregroundStyle(.secondary)
                    } else {
                        ForEach(mirrors, id: \.id) { mirror in
                            HStack {
                                VStack(alignment: .leading, spacing: 4) {
                                    HStack(spacing: 6) {
                                        Text(mirror.name).font(.subheadline.bold())
                                        if let q = ChannelQuality.detect(in: mirror.name) {
                                            Text(q.rawValue)
                                                .font(.caption2.bold())
                                                .padding(.horizontal, 4).padding(.vertical, 1)
                                                .background(Color.secondary.opacity(0.2), in: RoundedRectangle(cornerRadius: 3))
                                        }
                                    }
                                    Text(mirror.host).font(.caption).foregroundStyle(.secondary)
                                }
                                Spacer()
                                Button("Lire") {
                                    dismiss()
                                    onPlay(mirror)
                                }
                                .buttonStyle(.borderedProminent)
                                .controlSize(.small)
                            }
                        }
                    }
                } header: {
                    Text("Flux alternatifs (\(mirrors.count))")
                }
            }
            .navigationTitle("Flux Miroirs")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Fermer") { dismiss() }
                }
            }
        }
    }
}
