import SwiftUI

/// One entry of a `FacetChipsRow`.
struct FacetChip: Identifiable, Equatable {
    var id: String { key }
    let key: String
    let title: String
    let count: Int
}

/// A horizontal row of selectable chips with counters: the second axis of the category column
/// (PRD-33 § 8.2). "All" clears the selection; chips with no result are not shown by the engine.
struct FacetChipsRow: View {
    @Environment(\.uiMetrics) private var ui
    let allTitle: String
    let chips: [FacetChip]
    @Binding var selection: String?

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                chip(allTitle, selected: selection == nil) { selection = nil }
                ForEach(chips) { entry in
                    chip("\(entry.title) \(entry.count)", selected: selection == entry.key) { selection = entry.key }
                        .accessibilityLabel("\(entry.title), \(entry.count)")
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
        .accessibilityElement(children: .contain)
    }

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
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
