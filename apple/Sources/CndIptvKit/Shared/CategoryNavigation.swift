import SwiftUI
import CndCoreBridge

/// Adaptive category picker around a screen's content: a permanent column with counters from
/// 900 pt of width (iPad landscape; 520 pt on macOS), horizontal chips plus a searchable sheet below.
struct CategoryNavigation<Content: View>: View {
    let groups: [FacetCount]
    let totalCount: Int
    var allTitle = "Toutes les catégories"
    /// How a key is written in the column (PRD-33: `FR` -> "🇫🇷 France"). Provider labels stay as they are.
    var label: (String) -> String = { $0 }
    /// SF Symbol of a key, when the column shows one (themes).
    var icon: (String) -> String? = { _ in nil }
    /// Pays · Thèmes · Fournisseur switch, shown above the column, the chips and in the sheet.
    var axisPicker: AnyView? = nil
    /// Context menu of a row (hide a country or theme, classify a provider category…).
    var rowMenu: ((String) -> AnyView)? = nil
    @Binding var selection: String?
    @ViewBuilder var content: () -> Content

    @Environment(\.uiMetrics) private var ui
    @State private var isWide = false
    @AppStorage("categories.collapsed") private var collapsed = false
    @State private var showingSheet = false
    @State private var query = ""
    @State private var chipIndex = 0

    /// On a Mac the vertical list stays up in small windows (a mouse wheel cannot scroll the chips
    /// sideways); the column is narrower there.
    /// Default (scale 1) threshold; the view uses `ui.breakpoints.categoryColumn`.
    static var wideLayoutThreshold: CGFloat {
        #if os(macOS)
        520
        #else
        900
        #endif
    }
    private static var chipLimit: Int { 40 }

    var body: some View {
        GeometryReader { geo in
            let wide = geo.size.width >= ui.breakpoints.categoryColumn
            HStack(spacing: 0) {
                if wide && !collapsed {
                    VStack(spacing: 0) {
                        if let axisPicker { axisPicker.padding(.horizontal, 8).padding(.top, 8) }
                        HStack(spacing: 0) {
                            homeButton
                            filterField
                        }
                        list
                    }
                    .frame(width: geo.size.width >= ui.breakpoints.categoryColumnWide ? ui.categoryColumnWide : ui.categoryColumnNarrow)
                    #if os(tvOS)
                    .focusSection()
                    #endif
                    Divider()
                }
                VStack(spacing: 0) {
                    if !wide {
                        if let axisPicker { axisPicker.padding(.horizontal, 12).padding(.top, 8) }
                        chips
                    }
                    content()
                }
            }
            .onChange(of: wide, initial: true) { _, newValue in isWide = newValue }
        }
        .toolbar {
            if !isWide {
                ToolbarItem(placement: .automatic) {
                    Button { showingSheet = true } label: { Label("Catégories", systemImage: "folder") }
                }
            } else {
                ToolbarItem(placement: .automatic) {
                    Button { withAnimation { collapsed.toggle() } } label: {
                        Label(collapsed ? "Afficher les catégories" : "Masquer les catégories",
                              systemImage: "sidebar.left")
                    }
                    .help(collapsed ? "Afficher les catégories" : "Masquer les catégories")
                }
            }
        }
        .sheet(isPresented: $showingSheet) { sheet }
    }

    private var filteredGroups: [FacetCount] {
        let needle = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !needle.isEmpty else { return groups }
        return groups.filter {
            $0.value.localizedCaseInsensitiveContains(needle) || label($0.value).localizedCaseInsensitiveContains(needle)
        }
    }

    /// Back to the start: every category, no category filter text. Also what "all" does.
    private func goHome() {
        selection = nil
        query = ""
    }

    /// The home button.
    private var homeButton: some View {
        Button(action: goHome) {
            Image(systemName: "house.fill")
                .font(ui.font(.body))
                .frame(minWidth: ui.minHitTarget - 8, minHeight: ui.minHitTarget - 8)
                .contentShape(Rectangle())
        }
        .padding(.leading, 8)
        .accessibilityLabel("Accueil : toutes les catégories")
        #if !os(tvOS)
        .buttonStyle(.borderless)
        .help("Accueil : toutes les catégories")
        #endif
    }

    private var filterField: some View {
        #if os(tvOS)
        TextField("Filtrer les catégories", text: $query)
            .padding(8)
        #else
        TextField("Filtrer les catégories", text: $query)
            .textFieldStyle(.roundedBorder)
            .padding(8)
        #endif
    }

    private var list: some View {
        List(selection: $selection) {
            #if os(tvOS)
            Label(allTitle, systemImage: "square.grid.2x2")
                .tag(String?.none)
                .onChange(of: selection) { _, newValue in if newValue == nil { query = "" } }
            ForEach(filteredGroups, id: \.value) { group in
                row(group)
                    .tag(String?.some(group.value))
            }
            #else
            Label(allTitle, systemImage: "square.grid.2x2")
                .badge(totalCount)
                .tag(String?.none)
                .simultaneousGesture(TapGesture().onEnded { goHome() })
            ForEach(filteredGroups, id: \.value) { group in
                row(group)
                    .badge(Int(group.count))
                    .tag(String?.some(group.value))
            }
            #endif
        }
        #if os(tvOS)
        .listStyle(.plain)
        #else
        .listStyle(.sidebar)
        #endif
    }

    /// One category row: a symbol when the axis has one, the label otherwise.
    @ViewBuilder
    private func row(_ group: FacetCount) -> some View {
        if let symbol = icon(group.value) {
            Label(label(group.value), systemImage: symbol).lineLimit(1).rowMenu(rowMenu, key: group.value)
        } else {
            Text(label(group.value)).lineLimit(1).rowMenu(rowMenu, key: group.value)
        }
    }

    private var sheet: some View {
        NavigationStack {
            VStack(spacing: 0) {
                if let axisPicker { axisPicker.padding(.horizontal, 12).padding(.top, 8) }
                filterField
                list.onChange(of: selection) { _, _ in showingSheet = false }
            }
            .navigationTitle("Catégories")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("OK") { showingSheet = false } }
            }
        }
        #if os(macOS)
        .frame(minWidth: 320, minHeight: 420)
        #endif
        .presentationDetents([.medium, .large])
    }

    private var chips: some View {
        let shown = Array(groups.prefix(Self.chipLimit))
        return ScrollViewReader { proxy in
            HStack(spacing: 0) {
                #if os(macOS)
                // A mouse wheel does not scroll sideways: explicit arrows step through the chips.
                chipArrow("chevron.left", "Catégories précédentes") {
                    chipIndex = max(0, chipIndex - 4)
                    withAnimation { proxy.scrollTo(chipIndex, anchor: .leading) }
                }
                #endif
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        chip("Toutes", selected: selection == nil) { selection = nil }.id(0)
                        ForEach(Array(shown.enumerated()), id: \.element.value) { index, group in
                            chip("\(label(group.value)) \(group.count)", selected: selection == group.value) {
                                selection = group.value
                            }
                            .id(index + 1)
                        }
                        if groups.count > Self.chipLimit {
                            chip("Plus…", selected: false) { showingSheet = true }.id(shown.count + 1)
                        }
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                }
                #if os(macOS)
                chipArrow("chevron.right", "Catégories suivantes") {
                    chipIndex = min(shown.count + 1, chipIndex + 4)
                    withAnimation { proxy.scrollTo(chipIndex, anchor: .leading) }
                }
                #endif
            }
        }
    }

    #if os(macOS)
    private func chipArrow(_ symbol: String, _ label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(ui.font(.body, weight: .semibold))
                .frame(width: ui.value(22), height: ui.minHitTarget)
        }
            .buttonStyle(.plain)
            .accessibilityLabel(label)
            .help(label)
    }
    #endif

    private func chip(_ title: String, selected: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(title)
                .font(ui.font(.subheadline))
                .lineLimit(1)
                .padding(.horizontal, ui.spacingM)
                .padding(.vertical, ui.value(6))
                .background(selected ? Color.accentColor : Color.secondary.opacity(0.18), in: Capsule())
                .foregroundStyle(selected ? Color.white : Color.primary)
        }
        .buttonStyle(.plain)
    }
}

private extension View {
    /// Adds the row's context menu when the screen provides one.
    @ViewBuilder
    func rowMenu(_ menu: ((String) -> AnyView)?, key: String) -> some View {
        if let menu { contextMenu { menu(key) } } else { self }
    }
}
