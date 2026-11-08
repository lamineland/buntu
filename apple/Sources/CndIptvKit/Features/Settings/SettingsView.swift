import SwiftUI
import CndCoreBridge

struct SettingsView: View {
    @Bindable var model: SettingsModel
    @Bindable var directory: DownloadDirectoryManager
    let stats: DbStats?
    var sync: CloudKitSyncManager?
    var ai: AiProviderSettings?
    var assistant: AssistantModel?
    var upNext: UpNextModel?
    var library: LibraryModel?

    @State private var showsOmdbKey = false
    @State private var showsAiKey = false
    @State private var aiStatus: AiProviderStatus?
    @State private var isTestingAi = false
    @AppStorage("autoFailoverEnabled") private var autoFailoverEnabled = true
    @AppStorage("healthChecksEnabled") private var healthChecksEnabled = true
    @AppStorage("timeshiftDuration") private var timeshiftDurationRaw: String = TimeshiftDuration.oneHour.rawValue
    @AppStorage("openSubtitlesLanguages") private var openSubtitlesLanguages: String = "fr,en"
    @AppStorage("openSubtitlesAutoSearch") private var openSubtitlesAutoSearch = false
    @State private var showingProfilesManagement = false
    @State private var confirmingCategoryReset = false
    @State private var showingAdultUnlock = false
    @State private var parental = ParentalControlManager.shared

    var body: some View {
        Form {
            playbackAndHealthSection
            if let upNext { recommendationsSection(upNext) }
            profilesAndBouquetsSection
            categoriesSection
            parentalSection
            appearanceSection
            #if os(macOS)
            windowSection
            #endif
            #if os(iOS)
            liveActivitiesSection
            #endif
            #if os(tvOS)
            displaySection
            #endif
            subtitlesSection
            downloadsSection
            #if !os(tvOS)
            if let library { librarySection(library) }
            #endif
            networkSection
            omdbSection
            if let ai, let assistant { assistantSection(ai, assistant) }
            if let sync { iCloudSection(sync) }
            diagnosticSection
            aboutSection
        }
        .formStyle(.grouped)
        .navigationTitle("Réglages")
        .onDisappear { model.flush() }
    }

    // MARK: Sections

    private var playbackAndHealthSection: some View {
        Section("Lecture & Santé des Flux") {
            Toggle("Bascule automatique vers un miroir", isOn: $autoFailoverEnabled)
            Toggle("Vérification périodique de santé", isOn: $healthChecksEnabled)
            Picker("Tampon du direct (Timeshift)", selection: $timeshiftDurationRaw) {
                ForEach(TimeshiftDuration.allCases) { item in
                    Text(item.title).tag(item.rawValue)
                }
            }
        }
    }

    @ViewBuilder
    private func recommendationsSection(_ upNext: UpNextModel) -> some View {
        Section {
            Toggle("Afficher « À suivre » sous le lecteur", isOn: Binding(
                get: { upNext.isUpNextShelfEnabled },
                set: { upNext.isUpNextShelfEnabled = $0 }
            ))
            Toggle("Carte « Film suivant » en fin de lecture", isOn: Binding(
                get: { upNext.isNextUpCardEnabled },
                set: { upNext.isNextUpCardEnabled = $0 }
            ))
            if upNext.isNextUpCardEnabled {
                Toggle("Lecture automatique", isOn: Binding(
                    get: { upNext.isAutoPlayNextEnabled },
                    set: { upNext.isAutoPlayNextEnabled = $0 }
                ))
                if upNext.isAutoPlayNextEnabled {
                    Picker("Compte à rebours", selection: Binding(
                        get: { upNext.countdownDuration },
                        set: { upNext.countdownDuration = $0 }
                    )) {
                        Text("5 secondes").tag(5)
                        Text("10 secondes").tag(10)
                        Text("15 secondes").tag(15)
                        Text("30 secondes").tag(30)
                    }
                }
            }
            Picker("Ordre des sagas", selection: Binding(
                get: { upNext.sagaOrderPreference },
                set: { upNext.sagaOrderPreference = $0 }
            )) {
                Text("Chronologie de l'histoire").tag(SagaOrderPreference.storyChronology)
                Text("Ordre de sortie").tag(SagaOrderPreference.releaseOrder)
            }
            Toggle("Utiliser l'assistant IA pour reconnaître les sagas", isOn: Binding(
                get: { upNext.useAiForSagas },
                set: { upNext.useAiForSagas = $0 }
            ))
            .disabled(ai?.isAssistantEnabled != true)

            Button("Réinitialiser les contenus « Pas intéressé »") {
                upNext.resetDismissed()
            }
        } header: {
            Text("Recommandations & Sagas")
        } footer: {
            Text("L'assistant IA facultatif permet d'ordonner les sagas selon la chronologie de l'histoire en analysant uniquement les titres et les années. Aucune donnée personnelle ni URL de flux n'est transmise.")
        }
    }

    private var parentalSection: some View {
        Section {
            LabeledContent("Catégories adultes") {
                if let until = parental.adultUnlockedUntil, parental.isAdultUnlocked {
                    Text("Visibles jusqu'à \(until.formatted(date: .omitted, time: .shortened))")
                } else {
                    Text("Masquées")
                }
            }
            if parental.isAdultUnlocked {
                Button("Reverrouiller maintenant") { parental.lockAdultSession() }
            } else {
                Button("Déverrouiller 15 minutes…") { showingAdultUnlock = true }
            }
        } header: {
            Text("Contrôle parental")
        } footer: {
            Text("Les catégories dont le nom évoque un contenu adulte (adulte, xxx, +18…) sont masquées partout, recherche comprise, et ne sont jamais montrées au profil Enfants. Le code PIN se règle dans Profils.")
        }
        .sheet(isPresented: $showingAdultUnlock) {
            AdultUnlockSheet(parental: parental) { showingAdultUnlock = false }
        }
    }

    #if os(tvOS)
    private var displaySection: some View {
        @Bindable var matcher = DisplayRateMatcher.shared
        return Section {
            Toggle("Adapter la fréquence de l'écran au contenu", isOn: $matcher.isEnabled)
        } footer: {
            Text("Demande à l'Apple TV la fréquence (24, 25, 50 i/s…) et la plage dynamique (HDR10, HLG) de la vidéo. Réglage système requis : Vidéo et audio > Faire correspondre le contenu.")
        }
    }
    #endif

    #if os(iOS)
    private var liveActivitiesSection: some View {
        @Bindable var activities = LiveActivityManager.shared
        return Section {
            Toggle("Enregistrement en cours", isOn: $activities.isRecordingActivityEnabled)
            Toggle("Programme en cours de lecture", isOn: $activities.isPlaybackActivityEnabled)
        } header: {
            Text("Écran verrouillé & Dynamic Island")
        } footer: {
            Text("Affiche le temps restant d'un enregistrement et le programme lu. Mis à jour au plus une fois par minute, sans adresse de flux ni identifiants.")
        }
    }
    #endif

    private var appearanceSection: some View {
        @Bindable var scale = UIScaleStore.shared
        return Section {
            Picker("Taille de l'interface", selection: $scale.setting) {
                ForEach(UIScaleSetting.allCases) { Text($0.title).tag($0) }
            }
        } header: {
            Text("Apparence")
        } footer: {
            #if os(macOS)
            Text("Agrandit ou réduit les icônes, les cartes, les affiches et le texte de l'app. ⌘+ et ⌘- changent la taille, ⌘0 revient à « Standard ».")
            #else
            Text("S'ajoute à la taille de texte du système : l'interface ne devient jamais plus petite que ce que vous avez choisi dans Réglages > Accessibilité.")
            #endif
        }
    }

    #if os(macOS)
    private var windowSection: some View {
        @Bindable var level = WindowLevelController.shared
        return Section {
            Picker("Rester au premier plan", selection: $level.mode) {
                ForEach(AlwaysOnTopMode.allCases) { Text($0.title).tag($0) }
            }
            Toggle("Afficher sur tous les bureaux", isOn: $level.showsOnAllSpaces)
        } header: {
            Text("Fenêtre")
        } footer: {
            Text("Garde la fenêtre au-dessus des autres applications pour regarder en travaillant. « Pendant la lecture » ne la garde devant que tant qu'une lecture est ouverte. Sans effet en plein écran. ⌥⌘T bascule l'option.")
        }
    }
    #endif

    private var profilesAndBouquetsSection: some View {
        Section("Profils & Bouquets") {
            Button {
                showingProfilesManagement = true
            } label: {
                Label("Gérer les profils, bouquets et contrôle parental", systemImage: "person.2.badge.gearshape")
            }
        }
        .sheet(isPresented: $showingProfilesManagement) {
            NavigationStack {
                ProfilesManagementView(engine: model.engine)
                    .toolbar {
                        ToolbarItem(placement: .cancellationAction) {
                            Button("Fermer") { showingProfilesManagement = false }
                        }
                    }
            }
        }
    }

    private var categoriesSection: some View {
        let count = Int(model.categoryCorrections.items + model.categoryCorrections.groups)
        return Section {
            LabeledContent("Corrections faites") {
                Text(count == 0 ? "Aucune" : "\(model.categoryCorrections.items) chaîne(s), \(model.categoryCorrections.groups) catégorie(s)")
            }
            Button("Réinitialiser mes corrections de catégories…", role: .destructive) { confirmingCategoryReset = true }
                .disabled(count == 0)
        } header: {
            Text("Catégories")
        } footer: {
            Text("Le pays et le thème des chaînes sont devinés à partir de la playlist. « Changer le pays ou le thème » (clic droit sur une chaîne) les corrige ; ceci les efface toutes.")
        }
        .onAppear { model.refreshCategoryCorrections() }
        .confirmationDialog("Réinitialiser les corrections de catégories ?", isPresented: $confirmingCategoryReset) {
            Button("Réinitialiser", role: .destructive) { model.resetCategoryCorrections() }
            Button("Annuler", role: .cancel) {}
        } message: {
            Text("Les \(count) correction(s) seront effacées et le classement automatique reviendra.")
        }
    }

    private var subtitlesSection: some View {
        Section("Sous-titres en ligne (OpenSubtitles)") {
            SecureField("Clé API OpenSubtitles", text: $model.openSubtitlesKey)
            TextField("Langues préférées", text: $openSubtitlesLanguages)
            TextField("Compte OpenSubtitles (facultatif)", text: $model.openSubtitlesUser)
                .autocorrectionDisabled()
            SecureField("Mot de passe du compte", text: $model.openSubtitlesPassword)
            Toggle("Rechercher automatiquement quand il n'y a pas de sous-titres dans ma langue", isOn: $openSubtitlesAutoSearch)
            Text("Utilisé pour rechercher et télécharger les sous-titres manquants via api.opensubtitles.com. Un compte lève la limite de téléchargements par jour. La recherche automatique ne concerne ni le direct ni les flux sans clé API.")
                .font(.caption)
                .foregroundStyle(.secondary)
            bundledKeyNotice(.openSubtitles, userKey: model.openSubtitlesKey)
        }
    }

    private var downloadsSection: some View {
        Section("Téléchargements") {
            #if os(tvOS)
            LabeledContent("Connexions de mon abonnement", value: "\(model.connectionLimit)")
            LabeledContent("Segments par fichier", value: "\(model.segmentsPerFile)")
            LabeledContent("Taille d'une partie", value: "\(model.partSizeMB) Mo")
            LabeledContent("Fichiers simultanés", value: "\(model.maxActiveFiles)")
            #else
            Stepper(value: $model.connectionLimit, in: ConnectionBudget.range) {
                LabeledContent("Connexions de mon abonnement", value: "\(model.connectionLimit)")
            }
            Stepper(value: $model.segmentsPerFile, in: 1...model.budget.downloadConnections) {
                LabeledContent("Segments par fichier", value: "\(model.segmentsPerFile)")
            }
            Stepper(value: $model.partSizeMB, in: SettingsModel.partSizeRangeMB) {
                LabeledContent("Taille d'une partie", value: "\(model.partSizeMB) Mo")
            }
            Stepper(value: $model.maxActiveFiles, in: 1...model.budget.downloadConnections) {
                LabeledContent("Fichiers simultanés", value: "\(model.maxActiveFiles)")
            }
            #endif
            Text(model.budget.pausesDuringPlayback
                 ? "Avec une seule connexion, un téléchargement ne peut pas tourner pendant une lecture : il est mis en pause puis repris à la fin. Si votre abonnement en autorise plusieurs, indiquez-le ci-dessus."
                 : "Une connexion reste libre pour regarder : les téléchargements en utilisent au plus \(model.budget.downloadConnections) ensemble.")
                .font(.caption)
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 6) {
                LabeledContent("Dossier") {
                    #if os(tvOS)
                    Text(directory.directory.path)
                        .font(.callout).lineLimit(2).truncationMode(.middle)
                        .multilineTextAlignment(.trailing)
                    #else
                    Text(directory.directory.path)
                        .font(.callout).lineLimit(2).truncationMode(.middle).textSelection(.enabled)
                        .multilineTextAlignment(.trailing)
                    #endif
                }
                #if os(macOS)
                HStack {
                    Button("Choisir…") {
                        directory.chooseDirectory()
                        model.downloadDirectoryChanged()
                    }
                    Button("Par défaut") {
                        directory.resetToDefault()
                        model.downloadDirectoryChanged()
                    }
                    .disabled(!directory.isCustom)
                }
                #else
                Text("Visible dans l'app Fichiers, dans « Sur mon iPhone ».")
                    .font(.caption).foregroundStyle(.secondary)
                #endif
                if let error = directory.lastError {
                    Label(error, systemImage: "exclamationmark.triangle.fill").font(.caption).foregroundStyle(.red)
                }
            }
        }
    }

    #if !os(tvOS)
    private func librarySection(_ lib: LibraryModel) -> some View {
        Section("Mes fichiers") {
            NavigationLink {
                LibrarySettingsView(model: lib)
            } label: {
                Label("Gestion des dossiers locaux", systemImage: "folder.badge.gear")
            }
        }
    }
    #endif

    private var networkSection: some View {
        Section("Réseau et en-têtes HTTP") {
            TextField("User-Agent", text: $model.userAgent)
                .autocorrectionDisabled()
            TextField("Referer (optionnel)", text: $model.referer)
                .autocorrectionDisabled()
                #if os(iOS)
                .textInputAutocapitalization(.never)
                #endif
        }
    }

    private var omdbSection: some View {
        Section {
            HStack {
                Group {
                    if showsOmdbKey {
                        TextField("Clé API OMDb", text: $model.omdbKey)
                    } else {
                        SecureField("Clé API OMDb", text: $model.omdbKey)
                    }
                }
                .autocorrectionDisabled()
                #if os(iOS)
                .textInputAutocapitalization(.never)
                #endif
                Button { showsOmdbKey.toggle() } label: {
                    Image(systemName: showsOmdbKey ? "eye.slash" : "eye")
                }
                .buttonStyle(.borderless)
                .accessibilityLabel(showsOmdbKey ? "Masquer la clé" : "Afficher la clé")
            }
        } header: {
            Text("Métadonnées OMDb")
        } footer: {
            VStack(alignment: .leading, spacing: 4) {
                Text("Stockée dans le Trousseau, jamais dans la base de données. Utilisée pour enrichir les fiches films et séries.")
                bundledKeyNotice(.omdb, userKey: model.omdbKey)
            }
        }
    }

    // MARK: Assistant IA

    @ViewBuilder
    private func assistantSection(_ ai: AiProviderSettings, _ assistant: AssistantModel) -> some View {
        @Bindable var ai = ai
        Section {
            Picker("Fournisseur", selection: $ai.choice) {
                ForEach(AiChoice.allCases) { Text($0.title).tag($0) }
            }
            if ai.choice.isRemote {
                TextField("Modèle", text: $ai.model, prompt: Text("Modèle par défaut du fournisseur"))
                    .autocorrectionDisabled()
                    #if os(iOS)
                    .textInputAutocapitalization(.never)
                    #endif
                TextField("Adresse du serveur", text: $ai.baseURL, prompt: Text(ai.choice.baseURLPlaceholder))
                    .autocorrectionDisabled()
                    #if os(iOS)
                    .textInputAutocapitalization(.never)
                    .keyboardType(.URL)
                    #endif
                HStack {
                    Group {
                        if showsAiKey {
                            TextField(ai.choice.requiresKey ? "Clé API" : "Clé API (facultative)", text: $ai.apiKey)
                        } else {
                            SecureField(ai.choice.requiresKey ? "Clé API" : "Clé API (facultative)", text: $ai.apiKey)
                        }
                    }
                    .autocorrectionDisabled()
                    #if os(iOS)
                    .textInputAutocapitalization(.never)
                    #endif
                    Button { showsAiKey.toggle() } label: { Image(systemName: showsAiKey ? "eye.slash" : "eye") }
                        .buttonStyle(.borderless)
                        .accessibilityLabel(showsAiKey ? "Masquer la clé" : "Afficher la clé")
                }
                if ai.endpointIsLocal {
                    Label("Serveur sur cet appareil : rien ne le quitte.", systemImage: "lock.shield")
                        .font(.callout).foregroundStyle(.green)
                } else {
                    Toggle("J'accepte d'envoyer mes commandes à ce fournisseur", isOn: $ai.consent)
                }
                HStack {
                    Button("Tester") { Task { await runAiTest(assistant) } }
                        .disabled(isTestingAi || (ai.choice.requiresKey && !ai.hasKey))
                    if isTestingAi { ProgressView().controlSize(.small) }
                    if let status = aiStatus {
                        Label(
                            status.ok ? "\(status.message) (\(status.latencyMs) ms, \(status.model))" : status.message,
                            systemImage: status.ok ? "checkmark.circle.fill" : "xmark.octagon.fill"
                        )
                        .font(.callout)
                        .foregroundStyle(status.ok ? .green : .red)
                    }
                }
                if ai.usesBundledKey {
                    Label("Clé intégrée à l'application utilisée (\(BundledAiQuota.dailyLimit) requêtes par jour). Saisissez la vôtre pour ne plus partager ce quota.", systemImage: "key.fill")
                        .font(.callout).foregroundStyle(.secondary)
                }
                if let blocker = ai.blocker {
                    Label("\(blocker) En attendant, la résolution reste locale.", systemImage: "info.circle")
                        .font(.callout).foregroundStyle(.secondary)
                }
                if let keychainError = ai.keychainError {
                    Label(keychainError, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red).font(.callout)
                }
                #if !os(tvOS)
                DisclosureGroup("Ce qui est envoyé") { sentTable }
                    .font(.callout)
                #endif
            }
        } header: {
            Text("Assistant IA")
        } footer: {
            Text(assistantFooter(ai))
        }
        .onChange(of: ai.choice) { _, _ in aiStatus = nil }
    }

    private func assistantFooter(_ ai: AiProviderSettings) -> String {
        switch ai.choice {
        case .disabled: return "L'assistant (⌘K, bouton ✨) est masqué."
        case .localOnly: return "Les commandes explicites (« France 2 », « chaîne 6 », « suivante ») sont résolues sur l'appareil, sans aucun envoi. Choisissez un fournisseur pour les demandes plus libres."
        default: return "La clé est conservée dans le Trousseau. Sans clé, sans consentement ou en cas d'erreur, l'assistant retombe sur la résolution locale."
        }
    }

    private var sentTable: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Envoyé").font(.caption.weight(.semibold))
            Text("• la phrase que vous tapez\n• pour 40 chaînes au plus : nom, catégorie, numéro, programme en cours et suivant\n• la langue de l'interface")
                .font(.caption)
            Text("Jamais envoyé").font(.caption.weight(.semibold)).padding(.top, 4)
            Text("• les adresses des flux et le serveur IPTV\n• identifiants, mots de passe, en-têtes\n• l'historique, les favoris hors propositions, les téléchargements")
                .font(.caption)
        }
        .foregroundStyle(.secondary)
        .padding(.top, 4)
    }

    private func runAiTest(_ assistant: AssistantModel) async {
        isTestingAi = true
        aiStatus = await assistant.testProvider()
        isTestingAi = false
    }

    private func iCloudSection(_ sync: CloudKitSyncManager) -> some View {
        Section {
            LabeledContent("État") {
                HStack(spacing: 6) {
                    if sync.status == .syncing { ProgressView().controlSize(.small) }
                    Text(sync.status.title)
                }
            }
            if let detail = sync.status.detail {
                Label(detail, systemImage: sync.isAvailable ? "exclamationmark.triangle.fill" : "icloud.slash")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Button("Synchroniser maintenant") { Task { await sync.syncNow() } }
                .disabled(!sync.isAvailable || sync.status == .syncing)
            Toggle("Synchroniser aussi les mots de passe via le Trousseau iCloud", isOn: Binding(
                get: { model.syncsPasswords },
                set: { model.setPasswordSync($0) }
            ))
            if let error = model.passwordSyncError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.red)
            }
        } header: {
            Text("Synchronisation iCloud")
        } footer: {
            Text("Seuls les sources, les favoris et la progression de lecture sont synchronisés. Le catalogue et le guide sont reconstruits sur chaque appareil. Les mots de passe restent sur cet appareil, sauf si vous activez l'option : ils passent alors par le Trousseau iCloud de votre compte Apple (chiffré de bout en bout), à condition qu'il soit activé sur vos appareils.")
        }
    }

    private var diagnosticSection: some View {
        Section("Diagnostic") {
            Toggle("Afficher la console réseau", isOn: $model.showNetworkConsole)
            HStack {
                Button("Vider le cache des images") { Task { await model.clearImageCache() } }
                if let date = model.cacheClearedAt {
                    Text("Vidé à \(date.formatted(date: .omitted, time: .standard))").font(.caption).foregroundStyle(.secondary)
                }
            }
            if let error = model.errorMessage {
                Label(error, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
            }
        }
    }

    /// "Clé intégrée utilisée" under a key field that is empty while the build carries a default key.
    @ViewBuilder
    private func bundledKeyNotice(_ service: ApiService, userKey: String) -> some View {
        let resolver = ApiKeyResolver.shared
        if resolver.hasBundledKey(service),
           userKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            Label("Clé intégrée à l'application utilisée. Saisissez la vôtre pour ne plus partager le quota.", systemImage: "key.fill")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private var aboutSection: some View {
        Section("À propos") {
            HStack(spacing: 12) {
                AppLogoView(size: 44)
                VStack(alignment: .leading, spacing: 2) {
                    Text(AppBrand.name).font(.headline)
                    Text(AppBrand.descriptor).font(.caption).foregroundStyle(.secondary)
                }
            }
            LabeledContent("Application", value: SettingsModel.appVersion)
            LabeledContent("Moteur Rust", value: model.engineVersion)
            LabeledContent("Clés intégrées à ce build") {
                let services = ApiService.allCases.filter { ApiKeyResolver.shared.hasBundledKey($0) }
                Text(services.isEmpty ? "aucune" : services.map(\.displayName).joined(separator: ", "))
                    .multilineTextAlignment(.trailing)
            }
            if let stats {
                LabeledContent("Base de données", value: ByteCountFormatter.string(fromByteCount: Int64(stats.dbSizeBytes), countStyle: .file))
                LabeledContent("Chaînes / Films / Séries", value: "\(stats.totalLiveChannels) / \(stats.totalMovies) / \(stats.totalSeries)")
                LabeledContent("Favoris / En panne", value: "\(stats.totalFavorites) / \(stats.totalBroken)")
            }
        }
    }
}
