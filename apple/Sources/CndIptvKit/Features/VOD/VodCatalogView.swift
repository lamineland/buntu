import SwiftUI
import CndCoreBridge

enum VodCardStyle { case poster, row }

enum VodRoute: Hashable {
    case movie(IptvItemRecord)
    case series(SeriesRecord)
}

/// Tapping a card shows its details: in the side panel under the player on macOS, as a pushed page on iOS.
struct VodCardLink<Content: View>: View {
    let route: VodRoute
    @ViewBuilder let content: () -> Content
    @Environment(VodContext.self) private var vod

    var body: some View {
        #if os(macOS)
        content()
            .padding(4)
            .overlay {
                if vod.selection == route {
                    RoundedRectangle(cornerRadius: 12).strokeBorder(Color.accentColor, lineWidth: 3)
                }
            }
            .accessibleTap("Voir la fiche") { vod.selection = route }
            .accessibilityAddTraits(vod.selection == route ? .isSelected : [])
        #else
        NavigationLink(value: route, label: content).buttonStyle(.plain)
        #endif
    }
}

struct VodSectionTitles {
    var resume: String
    var resumeIcon: String
    var favorites = "Favoris"
    var all: String
}

/// Poster grid / list with categories, "resume" and "favorites" sections and infinite scroll,
/// shared by the Movies and Series tabs. The card is supplied by the caller.
struct VodCatalogView<Entry: CatalogEntry, Card: View>: View {
    @Environment(\.uiMetrics) private var ui
    let title: String
    let model: VodCatalogModel<Entry>
    let totalCount: Int
    let revision: Int
    let titles: VodSectionTitles
    let sortOptions: [SortField]
    let searchPrompt: String
    let emptyDescription: String
    /// Lets a provider category be classified by hand (PRD-33 lot 33.5); `nil` hides that menu.
    var engine: CndEngine?
    var onCategoriesChanged: () -> Void
    @ViewBuilder let card: (Entry, VodCardStyle) -> Card

    @AppStorage private var viewModeRaw: String
    @AppStorage private var axisRaw: String
    @State private var correction: TaxonomyCorrectionModel?

    init(
        title: String,
        model: VodCatalogModel<Entry>,
        totalCount: Int,
        revision: Int,
        viewModeKey: String,
        titles: VodSectionTitles,
        sortOptions: [SortField],
        searchPrompt: String,
        emptyDescription: String,
        engine: CndEngine? = nil,
        onCategoriesChanged: @escaping () -> Void = {},
        @ViewBuilder card: @escaping (Entry, VodCardStyle) -> Card
    ) {
        self.title = title
        self.model = model
        self.totalCount = totalCount
        self.revision = revision
        self.titles = titles
        self.sortOptions = sortOptions
        self.searchPrompt = searchPrompt
        self.emptyDescription = emptyDescription
        self.engine = engine
        self.onCategoriesChanged = onCategoriesChanged
        self.card = card
        _viewModeRaw = AppStorage(wrappedValue: LiveViewMode.grid.rawValue, viewModeKey)
        _axisRaw = AppStorage(
            wrappedValue: CategoryAxis.theme.storageValue,
            viewModeKey.replacingOccurrences(of: "viewMode", with: "categoryAxis")
        )
    }

    private var viewMode: LiveViewMode { LiveViewMode(rawValue: viewModeRaw) ?? .grid }
    private var reloadKey: String { "\(model.filter.signature)#\(revision)#\(axisRaw)" }

    // MARK: Categories by country / genre (PRD-33 lot 33.5)

    private var axis: CategoryAxis { CategoryAxis(storageValue: axisRaw, fallback: .theme) }

    private var facetsKey: String {
        let f = model.filter
        return "\(axisRaw)|\(f.country ?? "-")|\(f.theme ?? "-")|\(f.subtheme ?? "-")|\(revision)"
    }

    private var axisPicker: AnyView {
        AnyView(
            Picker("Classer par", selection: Binding(get: { axis }, set: { axisRaw = $0.storageValue })) {
                ForEach(CategoryAxis.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .accessibilityLabel("Classer les catégories par")
        )
    }

    private func countryBinding(_ filter: SearchFilterState) -> Binding<String?> {
        Binding(get: { filter.country }, set: { filter.country = $0 })
    }

    /// Changing the theme drops the genre, which belongs to the previous one.
    private func themeBinding(_ filter: SearchFilterState) -> Binding<String?> {
        Binding(get: { filter.theme }, set: { filter.theme = $0; filter.subtheme = nil })
    }

    private func columnSelection(_ filter: SearchFilterState) -> Binding<String?> {
        switch axis {
        case .provider: return Binding(get: { filter.group }, set: { filter.group = $0 })
        case .country: return countryBinding(filter)
        case .theme: return themeBinding(filter)
        }
    }

    private func rowMenu(for key: String) -> AnyView {
        guard axis == .provider, let engine else { return AnyView(EmptyView()) }
        return AnyView(Button("Classer cette catégorie…", systemImage: "globe.europe.africa") {
            correction = TaxonomyCorrectionModel(engine: engine, target: .providerCategory(key))
        })
    }

    private func categoriesChanged() {
        onCategoriesChanged()
        Task {
            await model.reload(revision: revision)
            await model.reloadTaxonomyFacets()
        }
    }

    @ViewBuilder
    private func crossFilters(_ filter: SearchFilterState) -> some View {
        if let other = axis.other {
            VStack(alignment: .leading, spacing: 0) {
                FacetChipsRow(
                    allTitle: other == .theme ? "Tous les genres" : "Tous les pays",
                    chips: model.crossFacets.map {
                        FacetChip(key: $0.key, title: CategoryText.label($0.key, axis: other), count: Int($0.count))
                    },
                    selection: axis == .country ? themeBinding(filter) : countryBinding(filter)
                )
                if !model.subthemeFacets.isEmpty {
                    FacetChipsRow(
                        allTitle: "Tous les genres",
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

    var body: some View {
        let axis = self.axis
        return CategoryNavigation(
            groups: axis == .provider ? model.groups : CategoryText.asFacetCounts(model.columnFacets),
            totalCount: axis == .provider ? totalCount : (model.allCount ?? totalCount),
            label: { CategoryText.label($0, axis: axis) },
            icon: { CategoryText.icon($0, axis: axis) },
            axisPicker: axisPicker,
            rowMenu: engine == nil ? nil : { key in rowMenu(for: key) },
            selection: columnSelection(model.filter)
        ) {
            VStack(spacing: 0) {
                crossFilters(model.filter)
                content
            }
        }
        .navigationTitle(title)
        .searchFilterToolbar(
            state: model.filter,
            hosts: model.hosts,
            sortOptions: sortOptions,
            prompt: searchPrompt
        )
        .toolbar {
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
        .sheet(item: $correction) { model in
            TaxonomyCorrectionSheet(model: model) { changed in
                correction = nil
                if changed { categoriesChanged() }
            }
        }
    }

    @ViewBuilder
    private var content: some View {
        if model.entries.isEmpty && !model.isLoading && model.resume.isEmpty && model.favorites.isEmpty {
            if model.filter.query.isEmpty && !model.filter.hasActiveFilter {
                ContentUnavailableView("Aucun contenu", systemImage: "film.stack", description: Text(emptyDescription))
            } else {
                ContentUnavailableView.search(text: model.filter.query)
            }
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 20) {
                    if let message = model.errorMessage {
                        Label(message, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
                    }
                    if model.searchIgnoresCategory {
                        Label("Recherche dans toutes les catégories", systemImage: "magnifyingglass")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                    if model.showsSections {
                        shelf(titles.resume, icon: titles.resumeIcon, entries: model.resume)
                        shelf(titles.favorites, icon: "star.fill", entries: model.favorites)
                        if !model.resume.isEmpty || !model.favorites.isEmpty {
                            Text(titles.all).font(.title3.bold())
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
        }
    }

    private func paged(_ entry: Entry, style: VodCardStyle) -> some View {
        card(entry, style).onAppear { model.loadMoreIfNeeded(current: entry) }
    }

    private var grid: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: ui.posterGridMinWidth, maximum: ui.posterGridMaxWidth), spacing: ui.spacingL)], spacing: ui.value(20)) {
            ForEach(model.entries, id: \.entryId) { paged($0, style: .poster) }
        }
    }

    private var list: some View {
        LazyVStack(spacing: 0) {
            ForEach(model.entries, id: \.entryId) { entry in
                paged(entry, style: .row)
                Divider()
            }
        }
    }

    @ViewBuilder
    private func shelf(_ title: String, icon: String, entries: [Entry]) -> some View {
        if !entries.isEmpty {
            VStack(alignment: .leading, spacing: 8) {
                Label(title, systemImage: icon).font(.title3.bold())
                HorizontalShelf(items: entries, spacing: 16, alignment: .top) { entry in
                    card(entry, .poster).frame(width: ui.shelfPosterWidth)
                }
            }
        }
    }
}

/// Shared poster artwork: 2:3 frame, rating badge, resume bar and favorite star.
struct PosterArtwork: View {
    let posterUrl: String?
    let rating: Double?
    let progress: Double?
    let isFavorite: Bool
    var placeholder = "film"
    var onToggleFavorite: () -> Void

    var body: some View {
        // The 2:3 frame comes from a flexible clear base, not from the image: inside a horizontal
        // shelf the parent proposes no height, and an image-driven frame collapsed to nothing.
        Color.clear
            .aspectRatio(2.0 / 3.0, contentMode: .fit)
            .overlay {
                ThumbnailImageView(urlString: posterUrl, size: .poster, placeholderSystemImage: placeholder, fills: true)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(.quaternary)
            }
            .clipShape(RoundedRectangle(cornerRadius: 10))
            .overlay(alignment: .topLeading) {
                if let rating, rating > 0 {
                    Text(String(format: "★ %.1f", rating))
                        .font(.caption2.weight(.bold))
                        .padding(.horizontal, 6).padding(.vertical, 3)
                        .background(.black.opacity(0.7), in: Capsule())
                        .foregroundStyle(.yellow)
                        .padding(6)
                }
            }
            .overlay(alignment: .topTrailing) {
                Button(action: onToggleFavorite) {
                    Image(systemName: isFavorite ? "star.fill" : "star")
                        .foregroundStyle(isFavorite ? Color.yellow : Color.white)
                        .padding(6)
                        .background(.black.opacity(0.45), in: Circle())
                }
                .buttonStyle(.plain)
                .padding(6)
                .accessibilityLabel(isFavorite ? "Retirer des favoris" : "Ajouter aux favoris")
            }
            .overlay(alignment: .bottom) {
                if let progress, progress > 0 {
                    ProgressView(value: min(progress, 1))
                        .progressViewStyle(.linear)
                        .tint(.red)
                        .padding(.horizontal, 6).padding(.bottom, 6)
                }
            }
    }
}

/// Hover highlight (macOS pointer; inert on touch).
struct HoverLift: ViewModifier {
    @State private var hovering = false

    func body(content: Content) -> some View {
        #if os(tvOS)
        content
        #else
        content
            .scaleEffect(hovering ? 1.03 : 1)
            .shadow(color: .black.opacity(hovering ? 0.25 : 0), radius: 8, y: 4)
            .animation(.easeOut(duration: 0.15), value: hovering)
            .onHover { hovering = $0 }
        #endif
    }
}

extension View {
    func hoverLift() -> some View { modifier(HoverLift()) }
}
