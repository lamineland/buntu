import Foundation
import Observation
import SwiftUI
import CndCoreBridge

public struct BouquetCategoryItem: Identifiable, Equatable {
    public var id: String { name }
    public let name: String
    public let count: UInt32
    public var isHidden: Bool
    public var isAdult: Bool
    public var position: Int32
}

@Observable
@MainActor
public final class BouquetEditorModel {
    private let engine: CndEngine
    public let kind: String
    public private(set) var categories: [BouquetCategoryItem] = []
    public private(set) var favoriteFolders: [FavoriteFolderRecord] = []
    /// Countries and themes hidden for the active profile (ISO codes in upper case, theme ids), PRD-33.
    public private(set) var hiddenCountries: [String] = []
    public private(set) var hiddenThemes: [String] = []
    public var errorMessage: String?
    public var isLoading = false

    public init(engine: CndEngine, kind: String = "live") {
        self.engine = engine
        self.kind = kind
        reload()
    }

    public func reload() {
        isLoading = true
        let profileId = engine.activeProfileId()

        do {
            let allGroups = try engine.getGroupsWithHidden(kind: kind, sourceId: nil, includeHidden: true)
            let hiddenList = Set(try engine.listHiddenGroups(profileId: profileId, kind: kind))

            var items: [BouquetCategoryItem] = []
            for (index, group) in allGroups.enumerated() {
                let norm = group.value.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
                let isHidden = hiddenList.contains(norm)
                let isAdult = engine.isAdultCategory(group: group.value)
                items.append(BouquetCategoryItem(
                    name: group.value,
                    count: group.count,
                    isHidden: isHidden,
                    isAdult: isAdult,
                    position: Int32(index)
                ))
            }
            self.categories = items
            hiddenCountries = try engine.listHiddenGroups(profileId: profileId, kind: "country:\(kind)").map { $0.uppercased() }
            hiddenThemes = try engine.listHiddenGroups(profileId: profileId, kind: "theme:\(kind)")
            self.favoriteFolders = try engine.listFavoriteFolders(profileId: profileId)
        } catch {
            errorMessage = "Erreur lors du chargement des catégories : \(error.localizedDescription)"
        }
        isLoading = false
    }

    public func toggleGroupVisibility(_ item: BouquetCategoryItem) {
        let profileId = engine.activeProfileId()
        do {
            if item.isHidden {
                _ = try engine.unhideGroup(profileId: profileId, kind: kind, group: item.name)
            } else {
                try engine.hideGroup(profileId: profileId, kind: kind, group: item.name)
            }
            reload()
        } catch {
            errorMessage = "Impossible de modifier la visibilité : \(error.localizedDescription)"
        }
    }

    /// Shows a country hidden from the category column again.
    public func unhideCountry(_ code: String) {
        unhide(kind: "country:\(kind)", key: code)
    }

    public func unhideTheme(_ id: String) {
        unhide(kind: "theme:\(kind)", key: id)
    }

    private func unhide(kind: String, key: String) {
        do {
            _ = try engine.unhideGroup(profileId: engine.activeProfileId(), kind: kind, group: key)
            reload()
        } catch {
            errorMessage = "Impossible d'afficher de nouveau : \(error.localizedDescription)"
        }
    }

    public func moveCategory(fromOffsets: IndexSet, toOffset: Int) {
        var updated = categories
        updated.move(fromOffsets: fromOffsets, toOffset: toOffset)
        categories = updated

        let profileId = engine.activeProfileId()
        for (idx, cat) in categories.enumerated() {
            let key = cat.name.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
            _ = try? engine.setCustomOrder(profileId: profileId, scope: "group", key: key, position: Int32(idx))
        }
    }

    public func createFavoriteFolder(name: String) -> Int64? {
        let profileId = engine.activeProfileId()
        let pos = Int32(favoriteFolders.count)
        do {
            let id = try engine.createFavoriteFolder(profileId: profileId, name: name, position: pos)
            reload()
            return id
        } catch {
            errorMessage = "Impossible de créer le dossier : \(error.localizedDescription)"
            return nil
        }
    }

    public func deleteFavoriteFolder(id: Int64) {
        do {
            _ = try engine.deleteFavoriteFolder(id: id)
            reload()
        } catch {
            errorMessage = "Impossible de supprimer le dossier : \(error.localizedDescription)"
        }
    }
}

