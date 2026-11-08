import SwiftUI
import CndCoreBridge

public struct BouquetEditorView: View {
    @Environment(\.dismiss) private var dismiss
    let engine: CndEngine
    @State private var selectedKind = "live"
    @State private var newFolderName = ""
    @State private var showingCreateFolderSheet = false

    public init(engine: CndEngine) {
        self.engine = engine
    }

    public var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                Picker("Section", selection: $selectedKind) {
                    Text("Chaînes Direct").tag("live")
                    Text("Films VOD").tag("movie")
                    Text("Séries").tag("series")
                }
                .pickerStyle(.segmented)
                .padding()

                BouquetKindListView(engine: engine, kind: selectedKind)
            }
            .navigationTitle("Éditeur de bouquets")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Fermer") {
                        dismiss()
                    }
                }
            }
        }
        .frame(minWidth: 500, minHeight: 450)
    }
}

private struct BouquetKindListView: View {
    let engine: CndEngine
    let kind: String
    @State private var model: BouquetEditorModel?
    @State private var newFolderName = ""
    @State private var showNewFolderAlert = false

    var body: some View {
        Group {
            if let model {
                List {
                    Section {
                        ForEach(model.categories) { cat in
                            HStack {
                                Button {
                                    model.toggleGroupVisibility(cat)
                                } label: {
                                    Image(systemName: cat.isHidden ? "eye.slash" : "eye")
                                        .foregroundColor(cat.isHidden ? .secondary : .accentColor)
                                }
                                .buttonStyle(.plain)
                                .accessibilityLabel(cat.isHidden ? "Afficher la catégorie \(cat.name)" : "Masquer la catégorie \(cat.name)")

                                Text(cat.name)
                                    .strikethrough(cat.isHidden)
                                    .foregroundColor(cat.isHidden ? .secondary : .primary)

                                if cat.isAdult {
                                    Text("+18")
                                        .font(.caption2.bold())
                                        .padding(.horizontal, 4)
                                        .padding(.vertical, 1)
                                        .background(Color.red.opacity(0.8))
                                        .foregroundColor(.white)
                                        .cornerRadius(4)
                                }

                                Spacer()

                                Text("\(cat.count)")
                                    .font(.caption)
                                    .foregroundColor(.secondary)
                            }
                        }
                        .onMove { indices, newOffset in
                            model.moveCategory(fromOffsets: indices, toOffset: newOffset)
                        }
                    } header: {
                        HStack {
                            Text("Catégories (\(model.categories.count))")
                            Spacer()
                            Text("Glisser pour réordonner")
                                .font(.caption2)
                                .foregroundColor(.secondary)
                        }
                    }

                    if !model.hiddenCountries.isEmpty || !model.hiddenThemes.isEmpty {
                        Section {
                            ForEach(model.hiddenCountries, id: \.self) { code in
                                HStack {
                                    Text(CategoryText.countryLabel(code))
                                    Spacer()
                                    Button("Afficher") { model.unhideCountry(code) }
                                        .buttonStyle(.borderless)
                                        .accessibilityLabel("Afficher de nouveau \(CategoryText.countryName(code))")
                                }
                            }
                            ForEach(model.hiddenThemes, id: \.self) { theme in
                                HStack {
                                    Label(CategoryText.themeName(theme), systemImage: CategoryText.themeIcon(theme))
                                    Spacer()
                                    Button("Afficher") { model.unhideTheme(theme) }
                                        .buttonStyle(.borderless)
                                        .accessibilityLabel("Afficher de nouveau \(CategoryText.themeName(theme))")
                                }
                            }
                        } header: {
                            Text("Pays et thèmes masqués")
                        } footer: {
                            Text("Masqués depuis la colonne des catégories (clic droit sur un pays ou un thème).")
                        }
                    }

                    Section {
                        ForEach(model.favoriteFolders, id: \.id) { folder in
                            HStack {
                                Image(systemName: "folder.fill")
                                    .foregroundColor(.orange)
                                Text(folder.name)
                                Spacer()
                                Text("\(folder.itemCount) élément(s)")
                                    .font(.caption)
                                    .foregroundColor(.secondary)
                                Button(role: .destructive) {
                                    model.deleteFavoriteFolder(id: folder.id)
                                } label: {
                                    Image(systemName: "trash")
                                        .foregroundColor(.red)
                                }
                                .buttonStyle(.plain)
                                .accessibilityLabel("Supprimer le dossier \(folder.name)")
                            }
                        }

                        Button {
                            showNewFolderAlert = true
                        } label: {
                            Label("Nouveau dossier de favoris", systemImage: "folder.badge.plus")
                        }
                    } header: {
                        Text("Dossiers de favoris")
                    }
                }
                .alert("Créer un dossier de favoris", isPresented: $showNewFolderAlert) {
                    TextField("Nom du dossier (ex. Sport, Cinéma)", text: $newFolderName)
                    Button("Annuler", role: .cancel) { newFolderName = "" }
                    Button("Créer") {
                        if !newFolderName.trimmingCharacters(in: .whitespaces).isEmpty {
                            _ = model.createFavoriteFolder(name: newFolderName)
                            newFolderName = ""
                        }
                    }
                }
            } else {
                ProgressView()
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .task(id: kind) {
            model = BouquetEditorModel(engine: engine, kind: kind)
        }
    }
}

