import SwiftUI
import Observation
import CndCoreBridge

/// The categories a Kids profile may see (PRD 20, lot 20.3): everything else stays hidden from it.
/// A new Kids profile starts with nothing allowed, so a parent opens this screen to choose.
@Observable
@MainActor
final class KidsAllowListModel {
    enum Kind: String, CaseIterable, Identifiable {
        case live, movie, series
        var id: String { rawValue }
        var title: String {
            switch self {
            case .live: return "Chaînes"
            case .movie: return "Films"
            case .series: return "Séries"
            }
        }
    }

    let engine: CndEngine
    let profile: ProfileRecord
    var kind: Kind = .live { didSet { reload() } }
    private(set) var groups: [FacetCount] = []
    private(set) var allowed: Set<String> = []
    /// Themes of the live catalogue, for "allow a whole theme" (PRD-33 lot 33.4). Adult is never offered.
    private(set) var themes: [CategoryFacet] = []
    private(set) var allowedThemes: Set<String> = []
    private(set) var errorMessage: String?
    nonisolated static let themeKind = "theme:live"

    init(engine: CndEngine, profile: ProfileRecord) {
        self.engine = engine
        self.profile = profile
        reload()
    }

    /// Same rule as the engine: trimmed and lower-cased.
    nonisolated static func key(_ group: String) -> String {
        group.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    }

    /// Adult categories can never be allowed to a Kids profile: they are not offered.
    func reload() {
        do {
            groups = try engine.getGroupsWithHidden(kind: kind.rawValue, sourceId: nil, includeHidden: true)
                .filter { !engine.isAdultCategory(group: $0.value) }
            allowed = Set(try engine.listAllowedGroups(profileId: profile.id, kind: kind.rawValue))
            if kind == .live {
                let all = try engine.getCategoryFacets(
                    kind: "live", axis: .theme, sourceId: nil,
                    filter: TaxonomyFilter(country: nil, theme: nil, subtheme: nil)
                )
                themes = CategoryText.sortedThemes(all.filter { $0.key != "adult" && $0.key != "other" })
                allowedThemes = Set(try engine.listAllowedGroups(profileId: profile.id, kind: Self.themeKind))
            } else {
                themes = []
                allowedThemes = []
            }
            errorMessage = nil
        } catch {
            errorMessage = userMessage(for: error)
        }
    }

    func isAllowed(_ group: FacetCount) -> Bool { allowed.contains(Self.key(group.value)) }

    func setAllowed(_ group: FacetCount, _ on: Bool) {
        do {
            if on {
                try engine.allowGroup(profileId: profile.id, kind: kind.rawValue, group: group.value)
                allowed.insert(Self.key(group.value))
            } else {
                _ = try engine.disallowGroup(profileId: profile.id, kind: kind.rawValue, group: group.value)
                allowed.remove(Self.key(group.value))
            }
        } catch {
            errorMessage = userMessage(for: error)
        }
    }

    func isThemeAllowed(_ theme: CategoryFacet) -> Bool { allowedThemes.contains(Self.key(theme.key)) }

    func setThemeAllowed(_ theme: CategoryFacet, _ on: Bool) {
        do {
            if on {
                try engine.allowGroup(profileId: profile.id, kind: Self.themeKind, group: theme.key)
                allowedThemes.insert(Self.key(theme.key))
            } else {
                _ = try engine.disallowGroup(profileId: profile.id, kind: Self.themeKind, group: theme.key)
                allowedThemes.remove(Self.key(theme.key))
            }
        } catch {
            errorMessage = userMessage(for: error)
        }
    }

    func allowAll() { groups.forEach { if !isAllowed($0) { setAllowed($0, true) } } }
    func allowNone() { groups.forEach { if isAllowed($0) { setAllowed($0, false) } } }
}

struct KidsAllowListView: View {
    @State var model: KidsAllowListModel
    var onClose: () -> Void

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Picker("Type", selection: $model.kind) {
                        ForEach(KidsAllowListModel.Kind.allCases) { Text($0.title).tag($0) }
                    }
                    .pickerStyle(.segmented)
                } footer: {
                    Text("« \(model.profile.name) » ne voit que les catégories cochées. Les catégories adultes ne sont jamais proposées.")
                }
                if let message = model.errorMessage {
                    Section { Label(message, systemImage: "xmark.octagon.fill").foregroundStyle(.red) }
                }
                if !model.themes.isEmpty {
                    Section {
                        ForEach(model.themes, id: \.key) { theme in
                            Toggle(isOn: Binding(get: { model.isThemeAllowed(theme) }, set: { model.setThemeAllowed(theme, $0) })) {
                                HStack {
                                    Label(CategoryText.themeName(theme.key), systemImage: CategoryText.themeIcon(theme.key))
                                    Spacer()
                                    Text("\(theme.count)").foregroundStyle(.secondary).font(.caption)
                                }
                            }
                        }
                    } header: {
                        Text("Thèmes")
                    } footer: {
                        Text("Autoriser un thème autorise toutes ses chaînes, sauf celles des catégories adultes. Le thème Adultes n'est jamais proposé.")
                    }
                }
                Section {
                    HStack {
                        Button("Tout autoriser") { model.allowAll() }
                        Spacer()
                        Button("Rien") { model.allowNone() }
                    }
                    .buttonStyle(.borderless)
                    ForEach(model.groups, id: \.value) { group in
                        Toggle(isOn: Binding(get: { model.isAllowed(group) }, set: { model.setAllowed(group, $0) })) {
                            HStack {
                                Text(group.value).lineLimit(1)
                                Spacer()
                                Text("\(group.count)").foregroundStyle(.secondary).font(.caption)
                            }
                        }
                    }
                }
            }
            .formStyle(.grouped)
            .navigationTitle("Catégories autorisées")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Terminé", action: onClose) } }
        }
        #if os(macOS)
        .frame(minWidth: 440, minHeight: 520)
        #endif
    }
}
