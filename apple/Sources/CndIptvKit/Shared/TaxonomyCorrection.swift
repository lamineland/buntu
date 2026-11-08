import SwiftUI
import Observation
import CndCoreBridge

/// "Changer le pays / le thème…" (PRD-33 § 7): the user corrects what the classifier decided, for one
/// channel or for a whole provider category. A correction is kept across refreshes.
@Observable
@MainActor
final class TaxonomyCorrectionModel: Identifiable {
    enum Target: Equatable {
        case channel(stableHash: String, name: String)
        case providerCategory(String)

        var title: String {
            switch self {
            case .channel(_, let name): return "Classer « \(name) »"
            case .providerCategory(let title): return "Classer la catégorie « \(title) »"
            }
        }

        var footer: String {
            switch self {
            case .channel:
                return "Sert à cette chaîne seulement, y compris après une actualisation de la source."
            case .providerCategory:
                return "S'applique à toutes les chaînes de cette catégorie du fournisseur. Une correction faite sur une chaîne l'emporte."
            }
        }
    }

    let id = UUID()
    let engine: CndEngine
    let target: Target
    /// `nil` keeps the computed country; otherwise an ISO code or `INT`.
    var country: String?
    /// Empty keeps the computed themes.
    var themes: Set<String> = []
    private(set) var hasCorrection = false
    /// What the classifier decided for a channel, when no correction hides it.
    private(set) var currentDescription: String?
    private(set) var errorMessage: String?

    init(engine: CndEngine, target: Target) {
        self.engine = engine
        self.target = target
        load()
    }

    private func load() {
        do {
            switch target {
            case .channel(let hash, _):
                if let correction = try engine.getTaxonomyOverride(stableHash: hash) { apply(correction) }
                if let stored = try engine.getItemTaxonomy(stableHash: hash) {
                    let country = stored.country.map { CategoryText.countryLabel($0) } ?? "Pays inconnu"
                    let themes = stored.themes.map(CategoryText.themeName).joined(separator: ", ")
                    currentDescription = "\(country) · \(themes.isEmpty ? "Non classé" : themes)"
                }
            case .providerCategory(let title):
                if let rule = try engine.getTaxonomyGroupOverride(groupTitle: title) { apply(rule) }
            }
        } catch {
            errorMessage = userMessage(for: error)
        }
    }

    private func apply(_ correction: TaxonomyCorrection) {
        country = correction.country
        themes = Set(correction.themes)
        hasCorrection = true
    }

    /// Saving with nothing chosen removes the correction, like "Rétablir".
    var canSave: Bool { country != nil || !themes.isEmpty || hasCorrection }

    func toggle(theme: String) {
        if themes.contains(theme) { themes.remove(theme) } else { themes.insert(theme) }
    }

    /// The chosen themes in the closed list's order, so the stored list never depends on set order.
    private var orderedThemes: [String]? {
        let ordered = CategoryText.assignableThemes.filter(themes.contains)
        return ordered.isEmpty ? nil : ordered
    }

    @discardableResult
    func save() -> Bool {
        do {
            switch target {
            case .channel(let hash, _):
                try engine.setTaxonomyOverride(stableHash: hash, country: country, themes: orderedThemes)
            case .providerCategory(let title):
                try engine.setTaxonomyGroupOverride(groupTitle: title, country: country, themes: orderedThemes)
            }
            errorMessage = nil
            return true
        } catch {
            errorMessage = userMessage(for: error)
            return false
        }
    }

    @discardableResult
    func restoreAutomatic() -> Bool {
        do {
            switch target {
            case .channel(let hash, _): try engine.clearTaxonomyOverride(stableHash: hash)
            case .providerCategory(let title): try engine.clearTaxonomyGroupOverride(groupTitle: title)
            }
            country = nil
            themes = []
            hasCorrection = false
            errorMessage = nil
            return true
        } catch {
            errorMessage = userMessage(for: error)
            return false
        }
    }
}

struct TaxonomyCorrectionSheet: View {
    @State var model: TaxonomyCorrectionModel
    var onDone: (_ changed: Bool) -> Void

    var body: some View {
        NavigationStack {
            Form {
                if let current = model.currentDescription {
                    Section("Actuellement") { Text(current) }
                }
                Section {
                    Picker("Pays", selection: $model.country) {
                        Text("Automatique").tag(String?.none)
                        ForEach(CategoryText.allCountryCodes(), id: \.self) { code in
                            Text(CategoryText.countryLabel(code)).tag(String?.some(code))
                        }
                    }
                } footer: {
                    Text(model.target.footer)
                }
                Section {
                    ForEach(CategoryText.assignableThemes, id: \.self) { theme in
                        Toggle(isOn: Binding(get: { model.themes.contains(theme) }, set: { _ in model.toggle(theme: theme) })) {
                            Label(CategoryText.themeName(theme), systemImage: CategoryText.themeIcon(theme))
                        }
                    }
                } header: {
                    Text("Thèmes")
                } footer: {
                    Text("Aucun thème coché : le classement automatique est gardé.")
                }
                if let message = model.errorMessage {
                    Section { Label(message, systemImage: "xmark.octagon.fill").foregroundStyle(.red) }
                }
                if model.hasCorrection {
                    Section {
                        Button("Rétablir le classement automatique", role: .destructive) {
                            if model.restoreAutomatic() { onDone(true) }
                        }
                    }
                }
            }
            .formStyle(.grouped)
            .navigationTitle(model.target.title)
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Annuler") { onDone(false) } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Enregistrer") { if model.save() { onDone(true) } }
                        .disabled(!model.canSave)
                }
            }
        }
        #if os(macOS)
        .frame(minWidth: 460, minHeight: 560)
        #endif
    }
}
