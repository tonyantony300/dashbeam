import UIKit
import UniformTypeIdentifiers

@MainActor
class FilePickingSession: NSObject {
  private let contentTypes: [UTType]
  private let completion: ([URL]?) -> Void

  /// `completion` gets the picked URLs, or nil if the user cancelled.
  init(contentTypes: [UTType], completion: @escaping ([URL]?) -> Void) {
    self.contentTypes = contentTypes
    self.completion = completion
  }

  func start(on viewController: UIViewController) {
    // `asCopy: false` so files and folders take the same path through `stageCopy`,
    // and so a picked download folder can be written to in place.
    let picker = UIDocumentPickerViewController(forOpeningContentTypes: contentTypes, asCopy: false)
    picker.delegate = self
    picker.popoverPresentationController?.sourceView = viewController.view
    viewController.present(picker, animated: true)
  }
}

extension FilePickingSession: UIDocumentPickerDelegate {
  func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
    completion(nil)
  }

  func documentPicker(
    _ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]
  ) {
    completion(urls)
  }
}
