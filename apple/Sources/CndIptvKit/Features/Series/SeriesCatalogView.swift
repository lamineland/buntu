import SwiftUI
import CndCoreBridge

enum SeriesLabels {
    static func counts(_ series: SeriesRecord) -> String {
        let seasons = series.seasonCount
        let episodes = series.episodeCount
        guard episodes > 0 else { return "Épisodes à charger" }
        return "\(seasons) saison\(seasons > 1 ? "s" : "") · \(episodes) épisode\(episodes > 1 ? "s" : "")"
    }
}

struct SeriesCatalogView: View {
    @State private var model: VodCatalogModel<SeriesRecord>
    let revision: Int
    let totalSeries: Int
    let engine: CndEngine
    var onCategoriesChanged: () -> Void
    @Environment(VodContext.self) private var vod

    init(engine: CndEngine, revision: Int, totalSeries: Int, onCategoriesChanged: @escaping () -> Void = {}) {
        self.engine = engine
        self.onCategoriesChanged = onCategoriesChanged
        _model = State(initialValue: VodCatalogModel(kind: "series", source: .series(engine: engine)))
        self.revision = revision
        self.totalSeries = totalSeries
    }

    var body: some View {
        VodCatalogView(
            title: "Séries",
            model: model,
            totalCount: totalSeries,
            revision: revision,
            viewModeKey: "series.viewMode",
            titles: VodSectionTitles(resume: "Récemment consultées", resumeIcon: "clock", all: "Toutes les séries"),
            sortOptions: [.name, .rating, .recentlyAdded, .group],
            searchPrompt: "Rechercher une série",
            emptyDescription: "Ajoutez une source dans l'onglet Sources pour importer des séries.",
            engine: engine,
            onCategoriesChanged: onCategoriesChanged
        ) { series, style in
            SeriesCard(
                series: series,
                style: style,
                onToggleFavorite: { toggleFavorite(series) },
                onToggleBroken: { toggleBroken(series) }
            )
        }
    }

    private func toggleFavorite(_ series: SeriesRecord) {
        Task {
            if let value = await vod.toggleFavorite(stableHash: series.stableHash, kind: "series") {
                model.applyFavorite(value, to: series)
            }
        }
    }

    private func toggleBroken(_ series: SeriesRecord) {
        Task {
            if let value = await vod.toggleBroken(stableHash: series.stableHash, kind: "series") {
                model.applyBroken(value, to: series)
            }
        }
    }
}

struct SeriesCard: View {
    @Environment(\.uiMetrics) private var ui
    let series: SeriesRecord
    let style: VodCardStyle
    var onToggleFavorite: () -> Void
    var onToggleBroken: (() -> Void)? = nil

    var body: some View {
        VodCardLink(route: .series(series)) {
            switch style {
            case .poster: poster
            case .row: row
            }
        }
        .contextMenu {
            Button(series.isFavorite ? "Retirer des favoris" : "Ajouter aux favoris",
                   systemImage: series.isFavorite ? "star.slash" : "star", action: onToggleFavorite)
            if let onToggleBroken {
                Button(series.isBroken ? "Marquer comme fonctionnel" : "Signaler comme non fonctionnel",
                       systemImage: series.isBroken ? "checkmark.circle" : "exclamationmark.triangle",
                       action: onToggleBroken)
            }
        }
        .accessibilityLabel(series.title)
    }

    private var poster: some View {
        VStack(alignment: .leading, spacing: 6) {
            PosterArtwork(
                posterUrl: series.posterUrl, rating: series.imdbRating, progress: nil,
                isFavorite: series.isFavorite, placeholder: "play.tv", onToggleFavorite: onToggleFavorite
            )
            .hoverLift()
            Text(series.title).font(.subheadline.weight(.semibold)).lineLimit(2)
            if series.isBroken {
                Label("En panne", systemImage: "exclamationmark.triangle.fill")
                    .font(.caption2.weight(.bold))
                    .padding(.horizontal, 4).padding(.vertical, 1)
                    .background(Color.red.opacity(0.2), in: RoundedRectangle(cornerRadius: 3))
                    .foregroundStyle(Color.red)
            }
            Text(SeriesLabels.counts(series)).font(.caption).foregroundStyle(.secondary).lineLimit(1)
        }
    }

    private var row: some View {
        HStack(spacing: 12) {
            ThumbnailImageView(urlString: series.posterUrl, size: .poster, placeholderSystemImage: "play.tv", fills: true)
                .frame(width: ui.posterThumbWidth, height: ui.posterHeight(forWidth: ui.posterThumbWidth))
                .background(.quaternary)
                .clipShape(RoundedRectangle(cornerRadius: 6))
            VStack(alignment: .leading, spacing: 3) {
                Text(series.title).font(.body.weight(.semibold)).lineLimit(1)
                if series.isBroken {
                    Label("En panne", systemImage: "exclamationmark.triangle.fill")
                        .font(.caption2.weight(.bold))
                        .foregroundStyle(.red)
                }
                Text(SeriesLabels.counts(series)).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            Button(action: onToggleFavorite) {
                Image(systemName: series.isFavorite ? "star.fill" : "star")
                    .foregroundStyle(series.isFavorite ? Color.yellow : Color.secondary)
            }
            .buttonStyle(.plain)
            .accessibilityLabel(series.isFavorite ? "Retirer des favoris" : "Ajouter aux favoris")
        }
        .padding(.vertical, 6)
        .contentShape(Rectangle())
    }
}
