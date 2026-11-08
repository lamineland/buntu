import SwiftUI
import CndCoreBridge

enum AppTab: String, CaseIterable, Identifiable {
    case live
    case guide
    case movies
    case series
    case downloads
    case recordings
    #if !os(tvOS)
    case libraryVideo
    case libraryAudio
    case libraryImages
    case libraryFolders
    #endif
    case sources
    case settings

    var id: String { rawValue }

    var title: String {
        switch self {
        case .live: return "Live TV"
        case .guide: return "Guide TV"
        case .movies: return "Films"
        case .series: return "Séries"
        case .downloads: return "Téléchargements"
        case .recordings: return "Enregistrements"
        #if !os(tvOS)
        case .libraryVideo: return "Vidéos"
        case .libraryAudio: return "Audio"
        case .libraryImages: return "Photos"
        case .libraryFolders: return "Dossiers"
        #endif
        case .sources: return "Sources"
        case .settings: return "Réglages"
        }
    }

    var systemImage: String {
        switch self {
        case .live: return "tv"
        case .guide: return "calendar"
        case .movies: return "film"
        case .series: return "play.tv"
        case .downloads: return "arrow.down.circle"
        case .recordings: return "recordingtape"
        #if !os(tvOS)
        case .libraryVideo: return "film.stack"
        case .libraryAudio: return "music.note.list"
        case .libraryImages: return "photo.on.rectangle.angled"
        case .libraryFolders: return "folder"
        #endif
        case .sources: return "antenna.radiowaves.left.and.right"
        case .settings: return "gearshape"
        }
    }
}

/// Root view: waits for the engine, then shows the adaptive navigation.
public struct AppShellView: View {
    @Bindable var state: AppState
    var commands: AppCommandCenter

    public init(state: AppState, commands: AppCommandCenter) {
        self.state = state
        self.commands = commands
    }

    public var body: some View {
        Group {
            if let engine = state.engine {
                LoadedShellView(state: state, engine: engine, commands: commands)
            } else if let error = state.errorMessage {
                ContentUnavailableView("Démarrage impossible", systemImage: "exclamationmark.triangle", description: Text(error))
            } else {
                ProgressView("Initialisation du moteur…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .uiScaleRoot()
        .systemAccessibilityPreferences()
    }
}

/// macOS: `NavigationSplitView` (sidebar, then the screen with its own category column and the
/// docked player). iOS: `TabView` with a persistent mini-player above the tab bar. One navigation
/// container per platform, so there is a single title bar and a single toolbar.
private struct LoadedShellView: View {
    @Environment(\.uiMetrics) private var ui
    @Bindable var state: AppState
    let engine: CndEngine
    let commands: AppCommandCenter

    @AppStorage("shell.selectedTab") private var selectedTabRaw = AppTab.live.rawValue
    @State private var sourcesModel: SourcesModel
    @State private var toasts: ToastCenter
    @State private var vodContext: VodContext
    @State private var downloadsModel: DownloadsModel
    @State private var recordingsModel: RecordingsModel
    @State private var settingsModel: SettingsModel
    @State private var consoleModel: ConsoleModel
    @State private var pip = PiPManager()
    @State private var nowPlaying: NowPlayingCoordinator
    @State private var zapper = LiveZapper()
    @State private var assistant: AssistantModel
    @State private var showingAssistant = false
    @State private var showingShortcuts = false
    @State private var showingKidsExit = false
    @State private var autoSubtitleTask: Task<Void, Never>?
    @State private var showingProfilePicker = false
    @State private var offeredProfilePicker = false
    @State private var liveSearchRequest: String?
    @State private var epgFocus: IptvItemRecord?
    @State private var isPlayerExpanded = false
    @State private var columnVisibility: NavigationSplitViewVisibility = .automatic
    /// True while the window is full screen because the player asked for it (not because the user chose it).
    @State private var fullscreenIsOurs = false
    #if os(macOS)
    /// Whether the player was already large before the compact mode, to restore the layout.
    @State private var wasExpandedBeforeCompact = false
    #endif
    #if os(macOS)
    @AppStorage("sidePanel.width") private var sidePanelWidthStored: Double = 0
    @State private var sidePanelDragStart: Double?
    #endif
    @State private var windowIsFullscreen = false
    @State private var isDropTargeted = false
    @State private var multiViewController = MultiViewController()
    @State private var currentlyPlayingItem: IptvItemRecord?
    #if !os(tvOS)
    @State private var libraryModel: LibraryModel
    #endif

    init(state: AppState, engine: CndEngine, commands: AppCommandCenter) {
        self.state = state
        self.engine = engine
        self.commands = commands
        _nowPlaying = State(initialValue: NowPlayingCoordinator(controller: state.player))
        _assistant = State(initialValue: AssistantModel(backend: engine, settings: state.aiSettings))
        _sourcesModel = State(initialValue: SourcesModel(engine: engine, onChange: { [weak state] in
            state?.reloadSnapshot()
        }))
        let toasts = ToastCenter()
        _toasts = State(initialValue: toasts)
        _vodContext = State(initialValue: VodContext(
            engine: engine,
            toasts: toasts,
            downloadBase: { [weak state] in state?.downloadDirectory.directory ?? DownloadLocation.baseDirectory },
            playMedia: { [weak state] media in state?.player.open(media) },
            playQueue: { [weak state] medias, index in state?.player.openQueue(medias, startIndex: index) },
            openDownloads: { UserDefaults.standard.set(AppTab.downloads.rawValue, forKey: "shell.selectedTab") },
            onFavoriteChanged: { [weak state] in state?.reloadSnapshot(notifyScreens: false) },
            aiConfigProvider: { [weak state] in state?.aiSettings.providerConfig() ?? AiProviderConfig(kind: .localOnly, model: "", baseUrl: nil, apiKey: nil, timeoutMs: 8_000) },
            connections: state.downloadCoordinator
        ))
        _downloadsModel = State(initialValue: DownloadsModel(backend: engine))
        _recordingsModel = State(initialValue: RecordingsModel(backend: engine))
        let settings = SettingsModel(engine: engine, directory: state.downloadDirectory)
        settings.onConsoleVisibilityChanged = { [weak state] visible in state?.showNetworkConsole = visible }
        settings.onCategoriesChanged = { [weak state] in state?.reloadSnapshot() }
        _settingsModel = State(initialValue: settings)
        _consoleModel = State(initialValue: ConsoleModel(engine: engine))
        #if !os(tvOS)
        let library = LibraryModel(engine: engine)
        library.autoRegisterDownloadsFolder(url: state.downloadDirectory.directory)
        _libraryModel = State(initialValue: library)
        #endif
    }

    private var isKidsMode: Bool { state.profiles?.isKidsModeActive ?? false }
    private var visibleTabs: [AppTab] { KidsModePolicy.allowedTabs(isKids: isKidsMode) }
    /// A tab the Kids profile may not open (a menu command, a link) falls back to Live TV.
    private var selectedTab: AppTab {
        let tab = AppTab(rawValue: selectedTabRaw) ?? .live
        return visibleTabs.contains(tab) ? tab : .live
    }
    private var player: CndPlayerController { state.player }
    private var hasMedia: Bool { player.media != nil }

    var body: some View {
        VStack(spacing: 0) {
            mainShell
            if state.showNetworkConsole {
                NetworkConsoleDrawer(model: consoleModel)
            }
        }
    }

    private var mainShell: some View {
        attachSheets(platformShell)
            .environment(vodContext)
            .environment(state.profiles)
            .overlay(alignment: .bottom) {
                if let toast = toasts.current {
                    ToastView(toast: toast) { toasts.dismiss() }
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                }
            }
            .animation(.easeOut(duration: 0.2), value: toasts.current?.id)
            #if !os(tvOS)
            .dropDestination(for: URL.self) { urls, _ in handleDrop(urls) } isTargeted: { isDropTargeted = $0 }
            #endif
            .onOpenURL { url in _ = openShare(url) }
            .overlay {
                if isDropTargeted {
                    RoundedRectangle(cornerRadius: 12)
                        .strokeBorder(Color.accentColor, style: StrokeStyle(lineWidth: 3, dash: [10]))
                        .background(Color.accentColor.opacity(0.08))
                        .padding(8)
                        .allowsHitTesting(false)
                }
            }
            .alert(
                "Erreur",
                isPresented: Binding(get: { state.errorMessage != nil }, set: { if !$0 { state.errorMessage = nil } })
            ) {
                Button("OK") { state.errorMessage = nil }
            } message: {
                Text(state.errorMessage ?? "")
            }
            .onChange(of: selectedTabRaw) { _, _ in vodContext.selection = nil }
            #if !os(tvOS)
            .onChange(of: libraryModel.isAudioPlaying) { _, playing in
                if playing && player.state == .playing {
                    player.pause()
                }
            }
            #endif
            #if os(macOS)
            .onChange(of: WindowLevelController.shared.isCompact) { _, compact in
                if compact {
                    wasExpandedBeforeCompact = isPlayerExpanded
                    setExpanded(true, windowFullscreen: false)
                } else if !wasExpandedBeforeCompact {
                    setExpanded(false)
                }
            }
            #endif
            .onChange(of: hasMedia) { _, present in
                if !present { setExpanded(false) }
                commands.hasMedia = present
                #if os(macOS)
                WindowLevelController.shared.hasMedia = present
                #endif
                nowPlaying.refresh()
            }
            .onChange(of: player.state) { old, new in
                nowPlaying.refresh()
                pip.syncState(from: player)
                announcePlayback(from: old, to: new)
            }
            .onChange(of: player.reconnectBanner) { _, banner in
                if let banner { AccessibilityAnnouncer.shared.announce(banner) }
            }
            .onChange(of: player.failoverBanner) { _, banner in
                if let banner { AccessibilityAnnouncer.shared.announce(banner) }
            }
            .onChange(of: player.media) { _, _ in
                nowPlaying.refresh()
                commands.isFavorite = player.isFavorite
                scheduleAutoSubtitles()
                #if os(iOS)
                updatePlaybackActivity()
                #endif
            }
            .onChange(of: player.isFavorite) { _, isFav in
                commands.isFavorite = isFav
            }
            .onChange(of: player.surfaceLayer.map(ObjectIdentifier.init)) { _, _ in
                pip.attach(layer: player.surfaceLayer, controller: player)
            }
            .onAppear { connectPlayerIntegrations() }
            .task { checkInitialProfilePicker() }
            #if os(macOS)
            // The expanded player stays up when the window leaves full screen; "Réduire" docks it.
            .onReceive(NotificationCenter.default.publisher(for: NSWindow.didEnterFullScreenNotification)) { _ in
                windowIsFullscreen = true
            }
            .onReceive(NotificationCenter.default.publisher(for: NSWindow.didExitFullScreenNotification)) { _ in
                windowIsFullscreen = false
                fullscreenIsOurs = false
            }
            #endif
    }

    private func checkInitialProfilePicker() {
        guard !offeredProfilePicker else { return }
        let profileCount = state.profiles?.profiles.count ?? 0
        if profileCount > 1 {
            offeredProfilePicker = true
            showingProfilePicker = true
        }
    }

    @ViewBuilder
    private func attachSheets<V: View>(_ content: V) -> some View {
        content
            .sheet(isPresented: $showingProfilePicker) {
                if let profiles = state.profiles { ProfilePickerSheet(profiles: profiles) { showingProfilePicker = false } }
            }
            .sheet(isPresented: $showingKidsExit) {
                if let profiles = state.profiles { KidsExitSheet(profiles: profiles) { showingKidsExit = false } }
            }
            .sheet(isPresented: $showingShortcuts) {
                KeyboardShortcutsSheet { showingShortcuts = false }
            }
            .sheet(isPresented: $showingAssistant) {
                CommandPaletteView(model: assistant)
                    #if os(iOS)
                    .presentationDetents([.medium, .large])
                    #endif
            }
    }

    #if os(iOS)
    /// Lock screen / Dynamic Island "now playing": started with the media, ended when it is closed.
    private func updatePlaybackActivity() {
        guard let media = player.media else {
            LiveActivityManager.shared.endPlaybackActivity()
            return
        }
        LiveActivityManager.shared.startPlaybackActivity(
            channelName: media.title,
            programTitle: media.kind == .live ? "Direct" : "Lecture",
            durationSeconds: Int(player.durationMs / 1000),
            progress: { [weak player] in
                guard let player else { return nil }
                return (Int(player.positionMs / 1000), Int(player.durationMs / 1000))
            }
        )
    }
    #endif

    /// Shows the player large. `windowFullscreen` additionally puts the macOS window in full screen
    /// (the player's own button); starting a movie only fills the window, which is gentler.
    /// On iOS the player is presented full screen either way.
    private func setExpanded(_ expanded: Bool, windowFullscreen: Bool = true) {
        isPlayerExpanded = expanded
        #if os(macOS)
        columnVisibility = expanded ? .detailOnly : .automatic
        guard let window = NSApp.keyWindow ?? NSApp.mainWindow else { return }
        let isFull = window.styleMask.contains(.fullScreen)
        if expanded, windowFullscreen, !isFull {
            fullscreenIsOurs = true
            window.toggleFullScreen(nil)
        } else if !expanded, fullscreenIsOurs {
            fullscreenIsOurs = false
            if isFull { window.toggleFullScreen(nil) }
        }
        #endif
    }

    /// macOS shows a separate "Réduire" next to the full-screen button; on iOS the single button collapses.
    private var collapseAction: (() -> Void)? {
        #if os(macOS)
        { setExpanded(false) }
        #else
        nil
        #endif
    }

    /// macOS Mode Cinéma: the large player in full screen on black; the same button leaves it.
    private var cinemaAction: (() -> Void)? {
        #if os(macOS)
        { toggleWindowFullscreen() }
        #else
        nil
        #endif
    }

    private var expandedShowsFullscreen: Bool {
        #if os(macOS)
        windowIsFullscreen
        #else
        true
        #endif
    }

    /// Tells a screen-reader user what the player is doing: playback started, failed (PRD 31 § 3.4).
    private func announcePlayback(from old: PlaybackState, to new: PlaybackState) {
        guard let media = player.media else { return }
        switch (old, new) {
        case (.loading, .playing), (.idle, .playing):
            AccessibilityAnnouncer.shared.announce(media.kind == .live ? "\(media.title), en direct" : "\(media.title), lecture")
        case (_, .failed(let message)):
            AccessibilityAnnouncer.shared.announce("Erreur de lecture. \(message)", priority: .high)
        case (.playing, .buffering):
            AccessibilityAnnouncer.shared.announce("Chargement…", priority: .low)
        default:
            break
        }
    }

    /// Opens the command palette, or says how to enable it.
    private func openAssistant() {
        guard KidsModePolicy.canUseAssistant(isKids: isKidsMode) else {
            toasts.show("L'assistant n'est pas disponible avec le profil Enfants.")
            return
        }
        guard state.aiSettings.isAssistantEnabled else {
            toasts.show("L'assistant est désactivé : choisissez un fournisseur dans Réglages > Assistant IA.")
            return
        }
        assistant.input = ""
        showingAssistant = true
    }

    /// The player's full-screen button when it is already large: toggles the window's full screen.
    private func toggleWindowFullscreen() {
        #if os(macOS)
        guard let window = NSApp.keyWindow ?? NSApp.mainWindow else { return }
        fullscreenIsOurs = !window.styleMask.contains(.fullScreen)
        window.toggleFullScreen(nil)
        #endif
    }

    /// Hooks the system integrations (Now Playing, media keys, menu bar) to this shell.
    private func connectPlayerIntegrations() {
        #if os(macOS)
        // Covers runs that are not a .app bundle (swift run, Xcode scheme): they would show a generic icon.
        if Bundle.main.bundlePath.hasSuffix(".app") == false, let icon = AppLogoRenderer.nsImage(pixels: 512) {
            NSApplication.shared.applicationIconImage = icon
        }
        #endif
        nowPlaying.onZap = { zap($0) }
        nowPlaying.subtitleProvider = { media in
            media.kind == .live
                ? (zapper.nowTitle(forStreamUrl: media.url.absoluteString) ?? NowPlayingInfoBuilder.defaultSubtitle(for: media))
                : NowPlayingInfoBuilder.defaultSubtitle(for: media)
        }
        nowPlaying.activate()

        assistant.playingStableHash = { player.media?.stableHash }
        assistant.onDismiss = {
            showingAssistant = false
            assistant.input = ""
        }
        assistant.actions = AssistantActions(
            play: { item in
                Task.detached { try? engine.recordWatchProgress(stableHash: item.stableHash, positionMs: 0, durationMs: 0) }
                play(item)
                Haptics.light()
            },
            showGuide: { showGuide($0) },
            next: { zap(1) },
            previous: { zap(-1) },
            search: { query in
                selectedTabRaw = AppTab.live.rawValue
                liveSearchRequest = query
            },
            multiView: { items in
                multiViewController.closeAll()
                for item in items.prefix(multiViewController.maxAllowedSlots) {
                    multiViewController.addChannel(item, sources: state.sources, makeActive: false)
                }
                if multiViewController.count > 0 {
                    multiViewController.setActiveIndex(0)
                    player.pause()
                    multiViewController.isPresented = true
                }
                Haptics.light()
            }
        )
        commands.openAssistant = { openAssistant() }
        commands.showKeyboardShortcuts = { showingShortcuts = true }

        commands.addSource = { selectedTabRaw = AppTab.sources.rawValue; sourcesModel.request(.addSource) }
        commands.importPlaylist = { selectedTabRaw = AppTab.sources.rawValue; sourcesModel.request(.importPlaylist) }
        commands.importShare = { selectedTabRaw = AppTab.sources.rawValue; sourcesModel.request(.importShare) }
        commands.refreshAllSources = { Task { await sourcesModel.syncAll(state.sources) } }
        commands.toggleNetworkConsole = { toggleLogs() }
        commands.zap = { zap($0) }
        commands.togglePlayPause = { player.togglePlayPause() }
        commands.toggleMute = { player.isMuted.toggle() }
        commands.togglePictureInPicture = { pip.toggle() }
        commands.toggleMultiView = { toggleMultiView() }
        commands.toggleFavorite = { player.toggleFavorite() }
        commands.hasMedia = hasMedia
        commands.isFavorite = player.isFavorite

        state.cloudSync?.onRemoteChange = { ids in
            state.reloadSnapshot()
            refreshImportedSources(ids)
        }
    }

    /// Sources that arrived from iCloud have no catalog yet: fetch M3U ones now; Xtream ones need
    /// their password, which lives in the Keychain of the device where it was typed.
    private func refreshImportedSources(_ ids: [Int64]) {
        guard !ids.isEmpty else { return }
        Task {
            let sources = (try? engine.listSources()) ?? []
            for source in sources where ids.contains(source.id) {
                if source.sourceType == "xtream" {
                    toasts.show("« \(source.name) » vient d'iCloud : saisissez son mot de passe pour la charger.")
                } else {
                    await sourcesModel.sync(source)
                }
            }
        }
    }

    /// Next / previous episode when a queue is playing, otherwise next / previous channel.
    /// The player's download button: queues the film or episode on screen (same files and folders as the
    /// download buttons of its sheet) and confirms with the usual toast linking to the Downloads tab.
    private func downloadCurrentMedia() {
        guard let media = player.media, media.isDownloadable, let itemId = media.itemId,
              let item = try? engine.getItemById(itemId: itemId) else {
            toasts.show("Ce contenu ne peut pas être téléchargé.", isError: true)
            return
        }
        if media.itemKind == "episode" {
            vodContext.downloadEpisodes([item], seriesTitle: media.seriesTitle ?? item.name)
        } else {
            vodContext.downloadMovie(item)
        }
    }

    /// From a replay back to the channel's live stream.
    private func returnToLive() {
        guard let itemId = player.media?.itemId, let channel = try? engine.getItemById(itemId: itemId) else { return }
        play(channel)
    }

    /// Optional (Settings): a few seconds after a film or episode starts, when it has no subtitle track in a
    /// preferred language, search online and load the best match.
    private func scheduleAutoSubtitles() {
        autoSubtitleTask?.cancel()
        guard let media = player.media, media.kind != .live else { return }
        autoSubtitleTask = Task { @MainActor in
            try? await Task.sleep(nanoseconds: 6_000_000_000) // the engine needs a moment to list embedded tracks
            guard !Task.isCancelled, player.media == media else { return }
            let search = makeSubtitleSearch()
            guard AutoSubtitlePolicy.shouldSearch(
                media: media, tracks: player.subtitleTracks, overlayLoaded: player.subtitleOverlay.isLoaded,
                languages: search.languages,
                enabled: UserDefaults.standard.bool(forKey: OpenSubtitlesCredentials.autoSearchDefaultsKey),
                hasKey: OpenSubtitlesCredentials.apiKey() != nil
            ) else { return }
            await search.search()
            guard case .results(let rows, _) = search.state,
                  let best = AutoSubtitlePolicy.best(of: rows, languages: search.languages),
                  !Task.isCancelled, player.media == media else { return }
            await search.download(best)
            AppLog.shared.info("SUBTITLES", "Sous-titres ajoutés automatiquement (\(best.language.uppercased()))")
        }
    }

    /// Online subtitles for what is playing; the downloaded file is added to the current player.
    private func makeSubtitleSearch() -> SubtitleSearchModel {
        let media = player.media
        // An episode is matched through its series' IMDb id (known once OMDb enriched the series).
        let seriesImdb = media?.seriesId.flatMap { (try? engine.getSeriesById(seriesId: $0)) ?? nil }?.imdbId
        let model = SubtitleSearchModel(
            backend: engine, mediaTitle: media?.seriesTitle ?? media?.title ?? "", itemId: media?.itemId,
            imdbId: media?.imdbId, seriesImdbId: seriesImdb,
            seasonNumber: media?.seasonNumber, episodeNumber: media?.episodeNumber
        )
        model.onDownloaded = { [weak player, engine] url in player?.loadExternalSubtitle(fileUrl: url, engine: engine) }
        return model
    }

    private func zap(_ offset: Int) {
        if player.queue != nil {
            offset > 0 ? player.next() : player.previous()
            Haptics.light()
            return
        }
        guard player.media == nil || player.media?.kind == .live,
              let item = zapper.neighbor(of: player.media?.url.absoluteString, offset: offset) else { return }
        play(item)
        Haptics.light()
    }

    // MARK: Logs

    /// Shows or hides the log console (the engine's HTTP, M3U, Xtream, EPG, download and database
    /// events). Opening it also expands it if it was folded.
    private func toggleLogs() {
        let opening = !settingsModel.showNetworkConsole
        if opening { UserDefaults.standard.set(false, forKey: "console.collapsed") }
        settingsModel.showNetworkConsole.toggle()
    }

    private var sidebarBrand: some View {
        HStack(spacing: 10) {
            AppLogoView(size: ui.value(30))
            Text(AppBrand.name).font(ui.font(.headline))
            Spacer(minLength: 0)
            if isKidsMode { kidsExitButton }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private var logsButton: some View {
        Button { toggleLogs() } label: {
            Label("Journaux", systemImage: "terminal")
                .font(ui.font(.body))
                .frame(maxWidth: .infinity, minHeight: ui.minHitTarget - 12, alignment: .leading)
        }
        .buttonStyle(.plain)
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .foregroundStyle(state.showNetworkConsole ? Color.accentColor : Color.secondary)
        #if os(tvOS)
        .background(.thinMaterial)
        #else
        .background(.bar)
        #endif
        .overlay(alignment: .top) { Divider() }
        .help("Afficher ou masquer les journaux de l'application (⇧⌘C)")
        .accessibilityLabel(state.showNetworkConsole ? "Masquer les journaux" : "Afficher les journaux")
    }

    /// Only shown while the Kids profile is active: asks for the PIN, then returns to a regular profile.
    private var kidsExitButton: some View {
        Button { showingKidsExit = true } label: {
            Label("Quitter le profil Enfants", systemImage: "lock.fill")
                .labelStyle(.iconOnly)
        }
        .buttonStyle(.plain)
        .foregroundStyle(.orange)
        .help("Quitter le profil Enfants")
        .accessibilityLabel("Quitter le profil Enfants")
    }

    private var logsToolbarButton: some View {
        Button { toggleLogs() } label: {
            Label("Journaux", systemImage: state.showNetworkConsole ? "terminal.fill" : "terminal")
        }
        .accessibilityLabel(state.showNetworkConsole ? "Masquer les journaux" : "Afficher les journaux")
    }

    // MARK: Platform shells

    @ViewBuilder
    private var platformShell: some View {
        #if os(macOS)
        NavigationSplitView(columnVisibility: $columnVisibility) {
            List(selection: Binding<AppTab?>(
                get: { selectedTab },
                set: { if let tab = $0 { selectedTabRaw = tab.rawValue } }
            )) {
                Section("IPTV") {
                    ForEach([AppTab.live, .guide, .movies, .series, .downloads, .recordings].filter { visibleTabs.contains($0) }) { tab in
                        Label(tab.title, systemImage: tab.systemImage)
                            .font(ui.font(.body))
                            .badge(badge(for: tab))
                            .tag(tab)
                    }
                }
                #if !os(tvOS)
                Section {
                    ForEach([AppTab.libraryVideo, .libraryAudio, .libraryImages, .libraryFolders].filter { visibleTabs.contains($0) }) { tab in
                        Label(tab.title, systemImage: tab.systemImage)
                            .font(ui.font(.body))
                            .badge(badge(for: tab))
                            .tag(tab)
                    }
                } header: {
                    HStack {
                        Text("Mes fichiers")
                        Spacer()
                        if libraryModel.isScanning {
                            ProgressView()
                                .controlSize(.small)
                        }
                    }
                }
                #endif
                Section {
                    ForEach([AppTab.sources, .settings].filter { visibleTabs.contains($0) }) { tab in
                        Label(tab.title, systemImage: tab.systemImage)
                            .font(ui.font(.body))
                            .badge(badge(for: tab))
                            .tag(tab)
                    }
                }
            }
            .navigationTitle(AppBrand.name)
            .navigationSplitViewColumnWidth(min: ui.sidebarMinWidth, ideal: ui.sidebarIdealWidth, max: ui.sidebarMaxWidth)
            .safeAreaInset(edge: .top, spacing: 0) { sidebarBrand }
            .safeAreaInset(edge: .bottom, spacing: 0) { logsButton }
        } detail: {
            HStack(spacing: 0) {
                content(for: selectedTab)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                if showsSidePanel {
                    sidePanelResizeHandle
                    sidePanel.frame(width: sidePanelWidth)
                }
            }
            .safeAreaInset(edge: .bottom, spacing: 0) {
                #if !os(tvOS)
                AudioPlayerBar(model: libraryModel)
                #endif
            }
            .overlay {
                if isPlayerExpanded { expandedPlayer }
                if multiViewController.isPresented {
                    MultiViewScreen(
                        controller: multiViewController,
                        engine: engine,
                        sources: state.sources,
                        onExitToSinglePlayer: { channel in
                            multiViewController.isPresented = false
                            play(channel)
                        },
                        onClose: {
                            multiViewController.isPresented = false
                        }
                    )
                    .transition(.opacity)
                }
            }
        }
        #else
        TabView(selection: Binding(get: { selectedTab }, set: { selectedTabRaw = $0.rawValue })) {
            ForEach(visibleTabs) { tab in
                NavigationStack {
                    content(for: tab)
                        .safeAreaInset(edge: .bottom, spacing: 0) {
                            VStack(spacing: 0) {
                                #if !os(tvOS)
                                AudioPlayerBar(model: libraryModel)
                                #endif
                                miniPlayerBar
                            }
                        }
                        .toolbar {
                            ToolbarItem(placement: .topBarLeading) { logsToolbarButton }
                            if isKidsMode {
                                ToolbarItem(placement: .topBarTrailing) { kidsExitButton }
                            }
                        }
                }
                .tabItem { Label(tab.title, systemImage: tab.systemImage) }
                #if !os(tvOS)
                .badge(badge(for: tab))
                #endif
                .tag(tab)
            }
        }
        .fullScreenCover(isPresented: $isPlayerExpanded) {
            // The video fills the screen; the controls stay inside the safe area (status bar, Dynamic Island).
            expandedPlayer.background(Color.black.ignoresSafeArea())
        }
        .fullScreenCover(isPresented: $multiViewController.isPresented) {
            MultiViewScreen(
                controller: multiViewController,
                engine: engine,
                sources: state.sources,
                onExitToSinglePlayer: { channel in
                    multiViewController.isPresented = false
                    play(channel)
                },
                onClose: {
                    multiViewController.isPresented = false
                }
            )
        }
        #endif
    }

    private func badge(for tab: AppTab) -> Int {
        switch tab {
        case .live: return Int(state.stats?.totalLiveChannels ?? 0)
        case .movies: return Int(state.stats?.totalMovies ?? 0)
        case .series: return Int(state.stats?.totalSeries ?? 0)
        case .downloads: return Int(state.stats?.totalDownloads ?? 0)
        case .sources: return Int(state.stats?.totalSources ?? 0)
        #if !os(tvOS)
        case .libraryVideo: return libraryModel.videoItems.count
        case .libraryAudio: return libraryModel.audioItems.count
        case .libraryImages: return libraryModel.imageItems.count
        case .libraryFolders: return libraryModel.folders.count
        #endif
        case .guide, .settings, .recordings: return 0
        }
    }

    // MARK: Screens

    @ViewBuilder
    private func content(for tab: AppTab) -> some View {
        Group {
            switch tab {
            case .live:
                LiveTvView(
                    engine: engine,
                    revision: state.catalogRevision,
                    totalChannels: Int(state.stats?.totalLiveChannels ?? 0),
                    playingURL: player.media?.url.absoluteString,
                    onPlay: play,
                    onShowGuide: showGuide,
                    onFavoriteChanged: { state.reloadSnapshot(notifyScreens: false) },
                    zapper: zapper,
                    onOpenAssistant: state.aiSettings.isAssistantEnabled ? { openAssistant() } : nil,
                    onAddToMultiView: addToMultiView,
                    onOpenMultiView: openMultiView,
                    onStartOver: { channel, program in startOver(channel: channel, program: program) },
                    onCategoriesChanged: { state.reloadSnapshot() },
                    searchRequest: $liveSearchRequest
                )
            case .guide:
                EpgGuideView(
                    engine: engine,
                    focus: $epgFocus,
                    revision: state.catalogRevision,
                    playingURL: player.media?.url.absoluteString,
                    onPlay: play,
                    onPlayCatchup: playCatchup,
                    onScheduleRecording: scheduleRecording,
                    onAddSeriesRule: addSeriesRule
                )
            case .sources:
                SourcesListView(engine: engine, sources: state.sources, model: sourcesModel)
            case .movies:
                vodScreen(MoviesCatalogView(
                    engine: engine, revision: state.catalogRevision, totalMovies: Int(state.stats?.totalMovies ?? 0),
                    onCategoriesChanged: { state.reloadSnapshot() }
                ))
            case .series:
                vodScreen(SeriesCatalogView(
                    engine: engine, revision: state.catalogRevision, totalSeries: Int(state.stats?.totalSeries ?? 0),
                    onCategoriesChanged: { state.reloadSnapshot() }
                ))
            case .downloads:
                DownloadsDashboardView(model: downloadsModel, directory: state.downloadDirectory, onPlay: playDownload)
            case .recordings:
                RecordingsView(model: recordingsModel, onPlay: playRecording)
            #if !os(tvOS)
            case .libraryVideo:
                LibraryVideoView(model: libraryModel, onPlay: playLibraryVideo)
            case .libraryAudio:
                LibraryAudioView(model: libraryModel)
            case .libraryImages:
                LibraryImagesView(model: libraryModel)
            case .libraryFolders:
                LibraryFoldersView(model: libraryModel, onPlayVideo: playLibraryVideo)
            #endif
            case .settings:
                #if !os(tvOS)
                SettingsView(
                    model: settingsModel, directory: state.downloadDirectory, stats: state.stats, sync: state.cloudSync,
                    ai: state.aiSettings, assistant: assistant, upNext: state.upNext,
                    library: libraryModel
                )
                #else
                SettingsView(
                    model: settingsModel, directory: state.downloadDirectory, stats: state.stats, sync: state.cloudSync,
                    ai: state.aiSettings, assistant: assistant, upNext: state.upNext
                )
                #endif
            }
        }
        .safeAreaInset(edge: .top, spacing: 0) {
            if let banner = sourcesModel.banner {
                SourcesBannerView(banner: banner) { sourcesModel.dismissBanner() }
            }
        }
    }

    /// iOS pushes the details of a movie or series; on macOS they show in the side panel.
    @ViewBuilder
    private func vodScreen<Screen: View>(_ screen: Screen) -> some View {
        #if os(macOS)
        screen // details open in the side panel, not as a page
        #else
        screen.navigationDestination(for: VodRoute.self) { vodDestination($0) }
        #endif
    }

    @ViewBuilder
    private func vodDestination(_ route: VodRoute) -> some View {
        switch route {
        case .movie(let item): MovieDetailsView(item: item, vod: vodContext)
        case .series(let series): SeriesDetailsView(series: series, vod: vodContext)
        }
    }

    // MARK: Player

    #if !os(tvOS)
    private func playLibraryVideo(_ item: LibraryItemRecord) {
        libraryModel.stopAudio()
        let resumePos: Int64
        if let progress = libraryModel.getProgress(fingerprint: item.fingerprint), !progress.completed {
            resumePos = progress.positionMs
        } else {
            resumePos = 0
        }
        let media = PlayableMedia(libraryItem: item, startPositionMs: resumePos)
        player.open(media)

        let companionSubs = libraryModel.companionSubtitleURLs(for: item)
        for subUrl in companionSubs {
            player.loadExternalSubtitle(fileUrl: subUrl, engine: engine)
        }

        #if os(macOS)
        setExpanded(true, windowFullscreen: false)
        #else
        setExpanded(true)
        #endif
    }
    #endif

    private func play(_ item: IptvItemRecord) {
        #if !os(tvOS)
        libraryModel.stopAudio()
        #endif
        currentlyPlayingItem = item
        guard let media = PlayableMedia(item: item) else {
            state.errorMessage = "L'URL du flux de « \(item.name) » est invalide."
            return
        }
        player.open(media)
    }

    /// "Revoir depuis le début": the programme on air, played from its start through the provider's archive.
    private func startOver(channel: IptvItemRecord, program: EpgProgramRecord) {
        guard let url = (try? engine.buildCatchupUrl(itemId: channel.id, startTs: program.startTs, endTs: program.endTs)) ?? nil else {
            toasts.show("Le replay n'est pas disponible pour ce programme.")
            return
        }
        playCatchup(channel: channel, program: program, replayUrlString: url)
    }

    private func playCatchup(channel: IptvItemRecord, program: EpgProgramRecord, replayUrlString: String) {
        #if !os(tvOS)
        libraryModel.stopAudio()
        #endif
        currentlyPlayingItem = channel
        guard let url = URL(string: replayUrlString),
              let media = PlayableMedia(
                item: channel,
                overrideUrl: url,
                customTitle: "\(channel.name) — \(program.title) [Replay]",
                kind: .replay
              ) else {
            state.errorMessage = "L'URL de replay pour « \(program.title) » est invalide."
            return
        }
        player.open(media)
    }

    // MARK: - Multi-View

    private func openMultiView() {
        if multiViewController.slots.isEmpty, let current = currentlyPlayingItem {
            multiViewController.addChannel(current, sources: state.sources, makeActive: true)
            player.pause()
        }
        multiViewController.isPresented = true
    }

    private func toggleMultiView() {
        if multiViewController.isPresented {
            multiViewController.isPresented = false
        } else {
            openMultiView()
        }
    }

    private func addToMultiView(_ channel: IptvItemRecord) {
        if multiViewController.slots.isEmpty, let current = currentlyPlayingItem {
            multiViewController.addChannel(current, sources: state.sources, makeActive: false)
        }
        let added = multiViewController.addChannel(channel, sources: state.sources, makeActive: true)
        if added {
            player.pause()
            multiViewController.isPresented = true
        } else if let err = multiViewController.errorMessage {
            toasts.show(err, isError: true)
        }
    }

    /// Opens a finished download straight from disk in the built-in player.
    private func playDownload(_ task: DownloadTaskRecord) {
        #if !os(tvOS)
        libraryModel.stopAudio()
        #endif
        guard FileManager.default.fileExists(atPath: task.savePath) else {
            toasts.show("Le fichier « \(task.fileName) » est introuvable sur le disque.", isError: true)
            return
        }
        player.open(PlayableMedia(title: task.fileName, url: URL(fileURLWithPath: task.savePath), kind: .movie))
    }

    /// Opens a completed or partial recording from disk.
    private func playRecording(_ recording: RecordingRecord) {
        #if !os(tvOS)
        libraryModel.stopAudio()
        #endif
        guard let path = recording.filePath, FileManager.default.fileExists(atPath: path) else {
            toasts.show("Le fichier de l'enregistrement est introuvable sur le disque.", isError: true)
            return
        }
        player.open(PlayableMedia(
            title: "\(recording.channelName) — \(recording.title)",
            url: URL(fileURLWithPath: path),
            kind: .recording,
            stableHash: "recording_\(recording.id)"
        ))
    }

    private func scheduleRecording(channel: IptvItemRecord, program: EpgProgramRecord) {
        let req = NewRecordingRequest(
            itemId: channel.id,
            channelTvgId: channel.tvgId,
            channelName: channel.name,
            title: program.title,
            subtitle: program.subtitle,
            streamUrl: channel.streamUrl,
            startTs: program.startTs,
            endTs: program.endTs,
            prePaddingS: 120,
            postPaddingS: 300,
            ruleId: nil
        )
        do {
            _ = try recordingsModel.schedule(req: req)
            toasts.show("Enregistrement programmé : \(program.title)")
        } catch {
            toasts.show("Erreur de programmation : \(error.localizedDescription)", isError: true)
        }
    }

    private func addSeriesRule(channel: IptvItemRecord, program: EpgProgramRecord) {
        do {
            _ = try recordingsModel.addSeriesRule(channelTvgId: channel.tvgId, title: program.title)
            toasts.show("Series-Link activé : \(program.title)")
        } catch {
            toasts.show("Erreur Series-Link : \(error.localizedDescription)", isError: true)
        }
    }

    private func showGuide(_ item: IptvItemRecord) {
        epgFocus = item
        selectedTabRaw = AppTab.guide.rawValue
    }

    private func closePlayer() {
        currentlyPlayingItem = nil
        player.close()
        state.cloudSync?.scheduleSync()
        setExpanded(false)
    }

    #if os(macOS)
    /// The movie or series whose details are showing (Movies and Series tabs only).
    private var detailSelection: VodRoute? {
        selectedTab == .movies || selectedTab == .series ? vodContext.selection : nil
    }

    #if os(macOS)
    /// Drag the divider to enlarge the player column (double-click restores the default width).
    private var sidePanelResizeHandle: some View {
        Divider()
            .padding(.horizontal, 3)
            .contentShape(Rectangle())
            .onHover { inside in
                if inside { NSCursor.resizeLeftRight.push() } else { NSCursor.pop() }
            }
            .gesture(
                DragGesture(minimumDistance: 1, coordinateSpace: .global)
                    .onChanged { value in
                        if sidePanelDragStart == nil { sidePanelDragStart = sidePanelWidth }
                        let proposed = (sidePanelDragStart ?? sidePanelWidth) - value.translation.width
                        sidePanelWidthStored = min(max(proposed, Self.sidePanelMinWidth), Self.sidePanelMaxWidth)
                    }
                    .onEnded { _ in sidePanelDragStart = nil }
            )
            .onTapGesture(count: 2) { sidePanelWidthStored = 0 } // lint:ui-ignore decorative resize handle, the width has a Settings-free default
            .help("Glisser pour agrandir le lecteur, double-clic pour rétablir")
    }

    private static var sidePanelMinWidth: CGFloat { 320 }
    private static var sidePanelMaxWidth: CGFloat { 1100 }

    private var sidePanelWidth: CGFloat {
        sidePanelWidthStored > 0
            ? sidePanelWidthStored
            : (detailSelection == nil ? 360 : 400)
    }
    #endif

    private var showsSidePanel: Bool {
        (hasMedia && !isPlayerExpanded) || detailSelection != nil
    }

    /// Right-hand column: the player on top while something plays, the details of the selected
    /// movie or series right below it; browsing on the left stays possible throughout.
    private var sidePanel: some View {
        VStack(spacing: 0) {
            if hasMedia && !isPlayerExpanded { dockedVideo }
            if let route = detailSelection {
                if hasMedia && !isPlayerExpanded { Divider() }
                detailsPanel(route)
            } else if hasMedia {
                dockedInfo
                // No recommendations while a live channel plays (a live stream has no "next").
                if !player.isLive && state.upNext.isUpNextShelfEnabled && !state.upNext.recommendations.isEmpty {
                    Divider()
                    ScrollView {
                        UpNextShelf(
                            model: state.upNext,
                            orientation: .vertical,
                            onSelect: { [weak state] playable in
                                state?.player.open(playable)
                            }
                        )
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                    }
                } else {
                    Spacer(minLength: 0)
                }
            }
        }
        .background(.background)
    }

    @ViewBuilder
    private func detailsPanel(_ route: VodRoute) -> some View {
        switch route {
        case .movie(let item):
            MovieDetailsView(item: item, vod: vodContext, compact: true, onClose: { vodContext.selection = nil })
                .id(route)
        case .series(let series):
            SeriesDetailsView(series: series, vod: vodContext, compact: true, onClose: { vodContext.selection = nil })
                .id(route)
        }
    }

    private var dockedVideo: some View {
        CndVideoPlayerView(
            controller: player,
            upNext: state.upNext,
            onToggleFullscreen: { setExpanded(true) },
            onClose: closePlayer,
            closesOnDisappear: false,
            pip: pip,
            subtitleSearch: { makeSubtitleSearch() },
            onReturnToLive: { returnToLive() },
            onDownload: { downloadCurrentMedia() },
            onToggleCinema: { setExpanded(true) }
        )
        .aspectRatio(16 / 9, contentMode: .fit)
    }

    private var dockedInfo: some View {
        HStack(alignment: .top) {
            VStack(alignment: .leading, spacing: 6) {
                Text(player.media?.title ?? "").font(.headline).lineLimit(2)
                Text(player.isLive ? "Direct" : "Lecture").font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if player.media != nil {
                Button {
                    player.toggleFavorite()
                } label: {
                    Image(systemName: player.isFavorite ? "star.fill" : "star")
                        .font(.title3)
                        .foregroundStyle(player.isFavorite ? Color.yellow : Color.secondary)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(player.isFavorite ? "Retirer des favoris" : "Ajouter aux favoris")
                .help(player.isFavorite ? "Retirer des favoris" : "Ajouter aux favoris")
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding()
    }
    #endif

    private var expandedPlayer: some View {
        CndVideoPlayerView(
            controller: player,
            upNext: state.upNext,
            onToggleFullscreen: {
                #if os(macOS)
                toggleWindowFullscreen()
                #else
                setExpanded(false)
                #endif
            },
            onClose: closePlayer,
            onCollapse: collapseAction,
            closesOnDisappear: false,
            pip: pip,
            isFullscreen: expandedShowsFullscreen,
            onZap: { zap($0) },
            subtitleSearch: { makeSubtitleSearch() },
            onReturnToLive: { returnToLive() },
            onDownload: { downloadCurrentMedia() },
            onToggleCinema: cinemaAction,
            isCinema: expandedShowsFullscreen
        )
        .background(Color.black)
    }

    /// Persistent mini-player (iOS): tap to expand, controls stay one thumb away.
    @ViewBuilder
    private var miniPlayerBar: some View {
        if hasMedia {
            HStack(spacing: 12) {
                PlayerSurface(layer: isPlayerExpanded ? nil : player.surfaceLayer)
                    .frame(width: 112, height: 63)
                    .background(Color.black)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
                VStack(alignment: .leading, spacing: 2) {
                    Text(player.media?.title ?? "").font(.subheadline.weight(.semibold)).lineLimit(1)
                    Text(player.reconnectBanner ?? (player.isLive ? "Direct" : "Lecture"))
                        .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 0)
                Button { player.togglePlayPause() } label: {
                    Image(systemName: player.state == .playing || player.state == .buffering ? "pause.fill" : "play.fill")
                }
                .accessibilityLabel("Lecture / Pause")
                if state.aiSettings.isAssistantEnabled {
                    Button { openAssistant() } label: { Image(systemName: "sparkles") }
                        .accessibilityLabel("Assistant de chaînes")
                }
                Button(action: closePlayer) { Image(systemName: "xmark") }
                    .accessibilityLabel("Fermer le lecteur")
            }
            .buttonStyle(.plain)
            .font(.title3)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(.thinMaterial)
            .accessibleTap("Agrandir le lecteur") { setExpanded(true) }
        }
    }

    // MARK: Drag & drop

    /// A share arrives as a link or a `.cndshare` file: show the import sheet on the Sources tab.
    @discardableResult
    private func openShare(_ url: URL) -> Bool {
        guard let incoming = IncomingShare.from(url: url) else { return false }
        selectedTabRaw = AppTab.sources.rawValue
        sourcesModel.incomingShare = incoming
        return true
    }

    private func handleDrop(_ urls: [URL]) -> Bool {
        if let share = urls.first(where: { $0.pathExtension.lowercased() == ShareEnvelope.fileExtension }) {
            return openShare(share)
        }
        let playlists = urls.filter(SourcesModel.isM3uFile)
        if !playlists.isEmpty {
            Task { for url in playlists { await sourcesModel.importM3u(from: url) } }
            return true
        }
        #if !os(tvOS)
        let directoryUrls = urls.filter { url in
            var isDir: ObjCBool = false
            return FileManager.default.fileExists(atPath: url.path, isDirectory: &isDir) && isDir.boolValue
        }
        if let firstDir = directoryUrls.first {
            Task {
                if let folder = await libraryModel.addFolder(url: firstDir) {
                    toasts.show("Dossier « \(folder.displayName) » ajouté à la bibliothèque.")
                }
            }
            return true
        }
        #endif
        return false
    }
}
