import UIKit
import UniformTypeIdentifiers

/// Share Extension entry point — lets any app's share sheet hand a file to
/// Continuity, covering the case iOS's background restrictions otherwise
/// rule out (see docs/protocol.md): the main app doesn't need to be open
/// for the *share* action to work, even though actually transmitting the
/// file still needs a live engine.
///
/// Extensions run in a separate, short-lived, memory-constrained process —
/// running the full tokio/TLS engine here isn't practical or reliable, so
/// this stages the picked file into the shared App Group container and
/// prompts the user to open the main app to finish sending. A future
/// version could have the main app poll the staging directory on launch
/// and offer to send anything found there.
final class ShareViewController: UIViewController {
    private let appGroupId = "group.app.continuity.shared"

    override func viewDidLoad() {
        super.viewDidLoad()
        handleSharedItem()
    }

    private func handleSharedItem() {
        guard
            let item = extensionContext?.inputItems.first as? NSExtensionItem,
            let attachment = item.attachments?.first
        else {
            complete(message: "Nothing to share.")
            return
        }

        attachment.loadFileRepresentation(forTypeIdentifier: UTType.item.identifier) { [weak self] url, error in
            guard let self else { return }
            guard let url, error == nil else {
                self.complete(message: "Couldn't read the shared file.")
                return
            }
            self.stage(url)
        }
    }

    private func stage(_ sourceUrl: URL) {
        guard let container = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroupId) else {
            complete(message: "App Group not configured — see apps/ios/project.yml.")
            return
        }

        let stagingDir = container.appendingPathComponent("PendingShares", isDirectory: true)
        do {
            try FileManager.default.createDirectory(at: stagingDir, withIntermediateDirectories: true)
            let dest = stagingDir.appendingPathComponent(sourceUrl.lastPathComponent)
            if FileManager.default.fileExists(atPath: dest.path) {
                try FileManager.default.removeItem(at: dest)
            }
            try FileManager.default.copyItem(at: sourceUrl, to: dest)
            complete(message: "Saved — open Continuity to send \(sourceUrl.lastPathComponent).")
        } catch {
            complete(message: "Couldn't stage the file: \(error)")
        }
    }

    private func complete(message: String) {
        DispatchQueue.main.async {
            let alert = UIAlertController(title: "Continuity", message: message, preferredStyle: .alert)
            alert.addAction(UIAlertAction(title: "OK", style: .default) { [weak self] _ in
                self?.extensionContext?.completeRequest(returningItems: nil)
            })
            self.present(alert, animated: true)
        }
    }
}
