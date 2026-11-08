import SwiftUI
import CndCoreBridge

/// Quality badge guessed from the channel name (`4K`, `UHD`, `FHD`, `HD`).
enum ChannelQuality: String {
    case uhd = "4K"
    case fullHD = "FHD"
    case hd = "HD"

    private static func matches(_ pattern: String, in name: String) -> Bool {
        name.range(of: pattern, options: [.regularExpression, .caseInsensitive]) != nil
    }

    static func detect(in name: String) -> ChannelQuality? {
        if matches(#"(^|[^A-Za-z0-9])(4K|UHD|2160P?)($|[^A-Za-z0-9])"#, in: name) { return .uhd }
        if matches(#"(^|[^A-Za-z0-9])(FHD|1080[PI]?)($|[^A-Za-z0-9])"#, in: name) { return .fullHD }
        if matches(#"(^|[^A-Za-z0-9])(HD|720P?)($|[^A-Za-z0-9])"#, in: name) { return .hd }
        return nil
    }
}

enum EpgProgress {
    /// Elapsed fraction of a programme at `now`, clamped to 0...1.
    static func ratio(start: Int64, end: Int64, now: Date = Date()) -> Double {
        guard end > start else { return 0 }
        let elapsed = now.timeIntervalSince1970 - Double(start)
        return min(max(elapsed / Double(end - start), 0), 1)
    }

    static func timeRange(_ program: EpgProgramRecord) -> String {
        let start = Date(timeIntervalSince1970: TimeInterval(program.startTs))
        let end = Date(timeIntervalSince1970: TimeInterval(program.endTs))
        return "\(start.formatted(date: .omitted, time: .shortened)) – \(end.formatted(date: .omitted, time: .shortened))"
    }
}

struct ChannelActions {
    var play: (IptvItemRecord) -> Void
    var toggleFavorite: (IptvItemRecord) -> Void
    var toggleBroken: ((IptvItemRecord) -> Void)? = nil
    var addToMultiView: ((IptvItemRecord) -> Void)? = nil
    var showMirrors: ((IptvItemRecord) -> Void)? = nil
    /// Plays the programme on air from its beginning (provider catch-up); shown only when the channel has an archive.
    var startOver: ((IptvItemRecord, EpgProgramRecord) -> Void)? = nil
    var showGuide: (IptvItemRecord) -> Void
    /// "Changer le pays / le thème…" (PRD-33 § 7).
    var correctCategory: ((IptvItemRecord) -> Void)? = nil
}

/// One channel, as a grid card or a list row. Fixed height so lazy grids never reflow.
struct ChannelCardView: View {
    enum Style { case card, row }

    let item: IptvItemRecord
    let nowNext: ChannelNowNext?
    var health: StreamHealthRecord? = nil
    var isPlaying = false
    var style: Style = .card
    let actions: ChannelActions
    @Environment(\.uiMetrics) private var ui

    var body: some View {
        content
        .contextMenu { menu }
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel(accessibilityText)
        .accessibilityHint("Lance la chaîne")
        .channelAccessibilityActions(item: item, nowNext: nowNext, actions: actions)
    }

    @ViewBuilder
    private var content: some View {
        #if os(tvOS)
        // A tap gesture is not focusable with the Siri Remote: the whole card is the button
        // (select plays, long press opens the menu, which holds the favorite toggle).
        Button { actions.play(item) } label: { layout }
            .buttonStyle(.card)
        #else
        layout
            .accessibleTap { actions.play(item) }
        #endif
    }

    private var layout: some View {
        Group {
            switch style {
            case .card: card
            case .row: row
            }
        }
    }

    // MARK: Layouts

    private var card: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                logo
                titleBlock
                Spacer(minLength: 0)
                favoriteButton
            }
            programBlock
        }
        .padding(12)
        .frame(maxWidth: .infinity, minHeight: ui.channelCardMinHeight, alignment: .topLeading)
        .background(.quaternary.opacity(isPlaying ? 0.9 : 0.4), in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            if isPlaying {
                RoundedRectangle(cornerRadius: 12).strokeBorder(Color.accentColor, lineWidth: 2)
            }
        }
    }

    private var row: some View {
        HStack(spacing: 12) {
            logo
            VStack(alignment: .leading, spacing: 4) {
                titleBlock
                programBlock
            }
            favoriteButton
        }
        .padding(.vertical, 6)
        .padding(.horizontal, 8)
        .background(isPlaying ? Color.accentColor.opacity(0.15) : .clear)
    }

    // MARK: Pieces

    private var logo: some View {
        ThumbnailImageView(urlString: item.logoUrl, size: .tvLogo)
            .frame(width: ui.channelLogo, height: ui.channelLogo)
            .padding(2)
            .background(.background.opacity(0.6), in: RoundedRectangle(cornerRadius: 8))
    }

    private var titleBlock: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                healthIndicator
                if isPlaying { Image(systemName: "speaker.wave.2.fill").font(.caption).foregroundStyle(.tint) }
                if let number = item.chno { Text("\(number)").font(ui.font(.caption).monospacedDigit()).foregroundStyle(.secondary) }
                Text(item.name).font(ui.font(.subheadline, weight: .semibold)).lineLimit(1)
                if let quality = ChannelQuality.detect(in: item.name) {
                    Text(quality.rawValue)
                        .font(.caption2.weight(.bold))
                        .padding(.horizontal, 4).padding(.vertical, 1)
                        .background(quality == .uhd ? Color.orange : Color.secondary.opacity(0.25), in: RoundedRectangle(cornerRadius: 3))
                        .foregroundStyle(quality == .uhd ? Color.black : Color.primary)
                }
                if item.isBroken {
                    Label("En panne", systemImage: "exclamationmark.triangle.fill")
                        .font(.caption2.weight(.bold))
                        .padding(.horizontal, 4).padding(.vertical, 1)
                        .background(Color.red.opacity(0.2), in: RoundedRectangle(cornerRadius: 3))
                        .foregroundStyle(Color.red)
                }
            }
            if let group = item.groupTitle, !group.isEmpty {
                Text(group).font(ui.font(.caption)).foregroundStyle(.secondary).lineLimit(1)
            }
        }
    }

    @ViewBuilder
    private var programBlock: some View {
        if let now = nowNext?.now {
            VStack(alignment: .leading, spacing: 3) {
                Text(now.title).font(ui.font(.caption)).lineLimit(1)
                TimelineView(.periodic(from: .now, by: 30)) { context in
                    ProgressView(value: EpgProgress.ratio(start: now.startTs, end: now.endTs, now: context.date))
                        .progressViewStyle(.linear)
                        .controlSize(.mini)
                }
            }
        } else {
            Text(item.tvgId?.isEmpty == false ? "Guide indisponible" : "Pas de guide")
                .font(.caption)
                .foregroundStyle(.tertiary)
                .lineLimit(1)
        }
    }

    @ViewBuilder
    private var favoriteButton: some View {
        #if os(tvOS)
        if item.isFavorite { Image(systemName: "star.fill").foregroundStyle(Color.yellow) }
        #else
        Button { actions.toggleFavorite(item) } label: {
            Image(systemName: item.isFavorite ? "star.fill" : "star")
                .font(ui.font(.body))
                .foregroundStyle(item.isFavorite ? Color.yellow : Color.secondary)
                .frame(minWidth: ui.minHitTarget - 8, minHeight: ui.minHitTarget - 8)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(item.isFavorite ? "Retirer des favoris" : "Ajouter aux favoris")
        #endif
    }

    @ViewBuilder
    private var healthIndicator: some View {
        if item.isBroken {
            StatusDot(kind: .broken, description: "Signalée en panne")
        } else if let health {
            if health.status == "ok" {
                let isRecent = Date().timeIntervalSince1970 - Double(health.checkedAt) < 3600
                if isRecent && (health.latencyMs ?? 0) < 500 {
                    StatusDot(kind: .ok, description: "Opérationnelle (\(health.latencyMs ?? 0) ms)")
                } else {
                    StatusDot(kind: .slow, description: "Flux lent (\(health.latencyMs ?? 0) ms)")
                }
            } else if health.consecutiveFailures > 0 {
                StatusDot(kind: .failing, description: "\(health.consecutiveFailures) échec(s)")
            }
        }
    }

    @ViewBuilder
    private var menu: some View {
        Button("Lire", systemImage: "play.fill") { actions.play(item) }
        Button(item.isFavorite ? "Retirer des favoris" : "Ajouter aux favoris",
               systemImage: item.isFavorite ? "star.slash" : "star") { actions.toggleFavorite(item) }
        if let toggleBroken = actions.toggleBroken {
            Button(item.isBroken ? "Marquer comme fonctionnel" : "Signaler comme non fonctionnel",
                   systemImage: item.isBroken ? "checkmark.circle" : "exclamationmark.triangle") {
                toggleBroken(item)
            }
        }
        if let startOver = actions.startOver, let now = nowNext?.now, Self.hasArchive(item) {
            Button("Revoir depuis le début", systemImage: "backward.end.fill") { startOver(item, now) }
        }
        if let addToMultiView = actions.addToMultiView {
            Button("Ajouter à la mosaïque Multi-View", systemImage: "square.grid.2x2") {
                addToMultiView(item)
            }
        }
        if let showMirrors = actions.showMirrors {
            Button("Alternatives (Miroirs)", systemImage: "arrow.triangle.swap") {
                showMirrors(item)
            }
        }
        if let correctCategory = actions.correctCategory {
            Button("Changer le pays ou le thème…", systemImage: "globe.europe.africa") { correctCategory(item) }
        }
        if let media = PlayableMedia(item: item) {
            Menu("Ouvrir dans…", systemImage: "arrow.up.forward.app") {
                ForEach(ExternalPlayer.availableOnThisPlatform) { player in
                    Button(player.displayName) { Task { _ = await ExternalPlayerLauncher.launch(player, media: media) } }
                }
            }
        }
        Button("Voir le programme TV complet", systemImage: "calendar") { actions.showGuide(item) }
            .disabled(item.tvgId?.isEmpty != false)
        Divider()
        if let media = PlayableMedia(item: item) {
            Button("Copier l'URL du flux", systemImage: "link") { ExternalPlayerLauncher.copyStreamUrl(media) }
        }
    }

    /// The provider keeps an archive for this channel (days of catch-up, or a catch-up address template).
    static func hasArchive(_ item: IptvItemRecord) -> Bool {
        (item.catchupDays ?? 0) > 0 || (item.catchupSource?.isEmpty == false)
    }

    private var accessibilityText: String {
        A11yText.channel(
            name: item.name, number: item.chno, isPlaying: isPlaying, isFavorite: item.isFavorite, isBroken: item.isBroken,
            nowTitle: nowNext?.now?.title, timeRange: nowNext?.now.map(EpgProgress.timeRange)
        )
    }
}

private extension View {
    /// The context menu's entries as VoiceOver actions (swipe up or down): a long press is hard to do blind.
    func channelAccessibilityActions(item: IptvItemRecord, nowNext: ChannelNowNext?, actions: ChannelActions) -> some View {
        self
            .accessibilityAction(named: item.isFavorite ? "Retirer des favoris" : "Ajouter aux favoris") { actions.toggleFavorite(item) }
            .accessibilityAction(named: "Voir le programme TV complet") { actions.showGuide(item) }
            .modifier(OptionalChannelActions(item: item, nowNext: nowNext, actions: actions))
    }
}

private struct OptionalChannelActions: ViewModifier {
    let item: IptvItemRecord
    let nowNext: ChannelNowNext?
    let actions: ChannelActions

    func body(content: Content) -> some View {
        content
            .accessibilityActions {
                if let toggleBroken = actions.toggleBroken {
                    Button(item.isBroken ? "Marquer comme fonctionnel" : "Signaler comme non fonctionnel") { toggleBroken(item) }
                }
                if let startOver = actions.startOver, let now = nowNext?.now, ChannelCardView.hasArchive(item) {
                    Button("Revoir depuis le début") { startOver(item, now) }
                }
                if let addToMultiView = actions.addToMultiView {
                    Button("Ajouter à la mosaïque") { addToMultiView(item) }
                }
                if let showMirrors = actions.showMirrors {
                    Button("Alternatives, miroirs") { showMirrors(item) }
                }
            }
    }
}
