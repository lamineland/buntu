import Foundation
import Observation
import CndCoreBridge

/// Edits `RuntimeSettings` and the OMDb key. Changes are saved after a short pause (so typing a
/// User-Agent does not rebuild the HTTP pool on every keystroke); `flush()` saves immediately.
@Observable
@MainActor
final class SettingsModel {
    nonisolated static var segmentRange: ClosedRange<Int> { 1...16 }
    nonisolated static var partSizeRangeMB: ClosedRange<Int> { 1...256 }
    nonisolated static var activeFilesRange: ClosedRange<Int> { 1...8 }

    let engine: CndEngine
    let keychain: KeychainStore
    let directory: DownloadDirectoryManager
    @ObservationIgnored var onConsoleVisibilityChanged: (Bool) -> Void = { _ in }
    /// Corrections of countries and themes were reset (PRD-33): screens must reload.
    @ObservationIgnored var onCategoriesChanged: () -> Void = {}
    /// How many corrections of country / theme the user made, for a channel or a provider category.
    private(set) var categoryCorrections = TaxonomyOverrideSummary(items: 0, groups: 0)

    /// Simultaneous connections of the user's subscription (default 1). Downloads stay under it.
    var connectionLimit: Int {
        didSet {
            guard connectionLimit != oldValue else { return }
            defaults.set(connectionLimit, forKey: ConnectionBudget.defaultsKey)
            applyConnectionBudget()
        }
    }
    @ObservationIgnored private let defaults: UserDefaults

    var budget: ConnectionBudget { ConnectionBudget(limit: connectionLimit) }

    /// Segments and parallel files are brought under what the subscription allows.
    private func applyConnectionBudget() {
        let clamped = budget.clamp(segments: segmentsPerFile, files: maxActiveFiles)
        if segmentsPerFile != clamped.segments { segmentsPerFile = clamped.segments }
        if maxActiveFiles != clamped.files { maxActiveFiles = clamped.files }
    }

    var segmentsPerFile: Int { didSet { scheduleSave() } }
    var partSizeMB: Int { didSet { scheduleSave() } }
    var maxActiveFiles: Int { didSet { scheduleSave() } }
    var userAgent: String { didSet { scheduleSave() } }
    var referer: String { didSet { scheduleSave() } }
    var showNetworkConsole: Bool { didSet { scheduleSave(after: .zero) } }
    var omdbKey: String { didSet { scheduleSave() } }
    /// OpenSubtitles API key; kept in the Keychain like the OMDb one.
    var openSubtitlesKey: String { didSet { scheduleSave() } }
    /// Optional OpenSubtitles account (lifts the daily quota); the password lives in the Keychain too.
    var openSubtitlesUser: String { didSet { scheduleSave() } }
    var openSubtitlesPassword: String { didSet { scheduleSave() } }

    /// Whether Xtream passwords live in iCloud Keychain (opt-in, see `setPasswordSync`).
    private(set) var syncsPasswords: Bool
    private(set) var passwordSyncError: String?

    private(set) var errorMessage: String?
    private(set) var saveCount = 0
    private(set) var cacheClearedAt: Date?

    @ObservationIgnored private let saveDelay: Duration
    @ObservationIgnored private var saveTask: Task<Void, Never>?
    @ObservationIgnored private var base: RuntimeSettings

    init(
        engine: CndEngine,
        keychain: KeychainStore = .shared,
        directory: DownloadDirectoryManager,
        saveDelay: Duration = .milliseconds(400),
        defaults: UserDefaults = .standard
    ) {
        self.defaults = defaults
        connectionLimit = ConnectionBudget.current(defaults: defaults).limit
        self.engine = engine
        self.keychain = keychain
        self.directory = directory
        self.saveDelay = saveDelay
        let settings = engine.getSettings()
        base = settings
        segmentsPerFile = Int(settings.segmentsPerFile)
        partSizeMB = Int(settings.partSizeBytes / (1024 * 1024))
        maxActiveFiles = Int(settings.maxActiveFiles)
        userAgent = settings.defaultUserAgent
        referer = settings.defaultReferer ?? ""
        showNetworkConsole = settings.showNetworkConsole
        omdbKey = (try? keychain.readSecret(forKey: KeychainStore.omdbApiKeyRef)) ?? ""
        syncsPasswords = keychain.passwordScope == .iCloud
        OpenSubtitlesCredentials.migrateLegacyKey(keychain: keychain)
        openSubtitlesKey = (try? keychain.readSecret(forKey: OpenSubtitlesCredentials.keychainKey)) ?? ""
        openSubtitlesUser = (try? keychain.readSecret(forKey: OpenSubtitlesCredentials.userKey)) ?? ""
        openSubtitlesPassword = (try? keychain.readSecret(forKey: OpenSubtitlesCredentials.passwordKey)) ?? ""
        applyConnectionBudget()
    }

    /// Turns the iCloud Keychain option on or off and moves the existing source passwords with it.
    /// The choice is only recorded once every password has moved; on failure nothing changes.
    func setPasswordSync(_ enabled: Bool) {
        passwordSyncError = nil
        let sources = (try? engine.listSources()) ?? []
        let keys = sources.filter { $0.sourceType == "xtream" }
            .map { $0.passwordRef ?? KeychainStore.xtreamKey(forSourceUuid: $0.uuid) }
        do {
            try keychain.move(keys: keys, to: enabled ? .iCloud : .local)
            keychain.passwordScope = enabled ? .iCloud : .local
            syncsPasswords = enabled
        } catch {
            passwordSyncError = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
            syncsPasswords = keychain.passwordScope == .iCloud
        }
    }

    private func scheduleSave(after delay: Duration? = nil) {
        saveTask?.cancel()
        let wait = delay ?? saveDelay
        saveTask = Task { [weak self] in
            try? await Task.sleep(for: wait)
            guard !Task.isCancelled else { return }
            self?.save()
        }
    }

    /// Saves now and cancels any pending delayed save.
    func flush() {
        saveTask?.cancel()
        saveTask = nil
        save()
    }

    /// Called after the download folder changed (it is part of the persisted settings).
    func downloadDirectoryChanged() { scheduleSave(after: .zero) }

    func save() {
        let trimmedAgent = userAgent.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedReferer = referer.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedKey = omdbKey.trimmingCharacters(in: .whitespacesAndNewlines)

        var settings = base
        settings.segmentsPerFile = UInt32(segmentsPerFile.clamped(to: Self.segmentRange))
        settings.partSizeBytes = UInt64(partSizeMB.clamped(to: Self.partSizeRangeMB)) * 1024 * 1024
        settings.maxActiveFiles = UInt32(maxActiveFiles.clamped(to: Self.activeFilesRange))
        // Never more connections than the subscription allows, whatever the steppers say.
        let allowed = budget.clamp(segments: Int(settings.segmentsPerFile), files: Int(settings.maxActiveFiles))
        settings.segmentsPerFile = UInt32(allowed.segments)
        settings.maxActiveFiles = UInt32(allowed.files)
        settings.defaultUserAgent = trimmedAgent.isEmpty ? base.defaultUserAgent : trimmedAgent
        settings.defaultReferer = trimmedReferer.isEmpty ? nil : trimmedReferer
        settings.downloadDir = directory.isCustom ? directory.directory.path : nil
        settings.showNetworkConsole = showNetworkConsole
        settings.omdbApiKeyRef = trimmedKey.isEmpty ? nil : KeychainStore.omdbApiKeyRef

        let trimmedSubtitleKey = openSubtitlesKey.trimmingCharacters(in: .whitespacesAndNewlines)

        do {
            if trimmedKey.isEmpty {
                try keychain.deleteSecret(forKey: KeychainStore.omdbApiKeyRef)
            } else {
                try keychain.saveSecret(trimmedKey, forKey: KeychainStore.omdbApiKeyRef)
            }
            if trimmedSubtitleKey.isEmpty {
                try keychain.deleteSecret(forKey: OpenSubtitlesCredentials.keychainKey)
            } else {
                try keychain.saveSecret(trimmedSubtitleKey, forKey: OpenSubtitlesCredentials.keychainKey)
            }
            let user = openSubtitlesUser.trimmingCharacters(in: .whitespacesAndNewlines)
            if user.isEmpty || openSubtitlesPassword.isEmpty {
                // Both parts or nothing: a half-filled account is not kept.
                try keychain.deleteSecret(forKey: OpenSubtitlesCredentials.userKey)
                try keychain.deleteSecret(forKey: OpenSubtitlesCredentials.passwordKey)
            } else {
                try keychain.saveSecret(user, forKey: OpenSubtitlesCredentials.userKey)
                try keychain.saveSecret(openSubtitlesPassword, forKey: OpenSubtitlesCredentials.passwordKey)
            }
            try engine.updateSettings(settings: settings)
            let consoleChanged = base.showNetworkConsole != settings.showNetworkConsole
            base = settings
            errorMessage = nil
            saveCount += 1
            if consoleChanged { onConsoleVisibilityChanged(settings.showNetworkConsole) }
        } catch {
            errorMessage = userMessage(for: error)
        }
    }

    // MARK: Diagnostics

    func clearImageCache() async {
        await ImageThumbnailCache.shared.clearAll()
        cacheClearedAt = Date()
    }

    var engineVersion: String { engine.version() }

    nonisolated static var appVersion: String {
        let info = Bundle.main.infoDictionary
        let version = info?["CFBundleShortVersionString"] as? String ?? "dev"
        let build = info?["CFBundleVersion"] as? String
        return build.map { "\(version) (\($0))" } ?? version
    }

    func refreshCategoryCorrections() {
        if let summary = try? engine.taxonomyOverrideSummary() { categoryCorrections = summary }
    }

    /// "Réinitialiser mes corrections de catégories": the classifier's own country and themes come back.
    @discardableResult
    func resetCategoryCorrections() -> UInt32 {
        let removed = (try? engine.resetTaxonomyOverrides()) ?? 0
        refreshCategoryCorrections()
        if removed > 0 { onCategoriesChanged() }
        return removed
    }
}

extension Int {
    func clamped(to range: ClosedRange<Int>) -> Int { Swift.min(Swift.max(self, range.lowerBound), range.upperBound) }
}
