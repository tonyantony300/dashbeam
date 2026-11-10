import QuickLook
import UIKit

/// Quick Look preview of one file. `QLPreviewController` holds its data source
/// weakly, so this lives on the plugin until the preview is dismissed.
class PreviewSession: NSObject {
  private let file: URL
  private let onDismiss: () -> Void

  init(file: URL, onDismiss: @escaping () -> Void) {
    self.file = file
    self.onDismiss = onDismiss
  }

  @MainActor
  func present(on viewController: UIViewController) {
    let preview = QLPreviewController()
    preview.dataSource = self
    preview.delegate = self
    viewController.present(preview, animated: true)
  }
}

extension PreviewSession: QLPreviewControllerDataSource, QLPreviewControllerDelegate {
  func numberOfPreviewItems(in controller: QLPreviewController) -> Int { 1 }

  func previewController(_ controller: QLPreviewController, previewItemAt index: Int)
    -> QLPreviewItem
  {
    file as NSURL
  }

  func previewControllerDidDismiss(_ controller: QLPreviewController) {
    onDismiss()
  }
}
