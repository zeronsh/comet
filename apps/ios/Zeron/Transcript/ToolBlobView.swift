import SwiftUI

private struct ToolBlobLoaderKey: EnvironmentKey {
    static let defaultValue: (@MainActor (String) async throws -> String)? = nil
}
extension EnvironmentValues {
    var toolBlobLoader: (@MainActor (String) async throws -> String)? {
        get { self[ToolBlobLoaderKey.self] }
        set { self[ToolBlobLoaderKey.self] = newValue }
    }
}
struct ToolBlobSelection: Identifiable {
    let id: String
    let title: String
}
struct ToolBlobView: View {
    let selection: ToolBlobSelection
    let load: @MainActor (String) async throws -> String
    @Environment(\.dismiss) private var dismiss
    @State private var output: String?
    @State private var error: String?
    @State private var attempt = 0
    var body: some View {
        NavigationStack {
            Group {
                if let output {
                    ScrollView([.vertical, .horizontal]) {
                        Text(output).font(Theme.mono(12)).textSelection(.enabled).padding()
                    }
                } else if let error {
                    ContentUnavailableView {
                        Label("Output unavailable", systemImage: "exclamationmark.triangle")
                    } description: { Text(error) } actions: {
                        Button("Retry") { attempt += 1 }
                    }
                } else { ProgressView("Loading…") }
            }
            .navigationTitle(selection.title.replacingOccurrences(of: "Show full ", with: "").capitalized)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
                if let output {
                    ToolbarItem(placement: .topBarLeading) {
                        Button("Copy", systemImage: "doc.on.doc") { UIPasteboard.general.string = output }
                    }
                }
            }
            .task(id: attempt) {
                error = nil
                do {
                    let text = try await load(selection.id)
                    if !Task.isCancelled {
                        if selection.id.hasSuffix(".diff") {
                            output = try Self.readableDiff(text)
                        } else { output = text }
                    }
                } catch {
                    if !Task.isCancelled { self.error = error.localizedDescription }
                }
            }
        }
    }

    private static func readableDiff(_ text: String) throws -> String {
        struct Diff: Decodable { let path: String; let oldText: String?; let newText: String }
        let diff = try JSONDecoder().decode(Diff.self, from: Data(text.utf8))
        if let old = diff.oldText {
            return "\(diff.path)\n\nBefore\n\(old)\n\nAfter\n\(diff.newText)"
        }
        return "\(diff.path) (new file)\n\n\(diff.newText)"
    }

}
