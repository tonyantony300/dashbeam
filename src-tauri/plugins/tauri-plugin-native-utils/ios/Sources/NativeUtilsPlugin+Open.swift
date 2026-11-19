import Foundation
import Tauri
import UIKit

// Showing received files: Quick Look for a file, the Files app for a folder.

extension NativeUtilsPlugin {
  @objc func open_download_folder(_ invoke: Invoke) {
    let args: OpenDownloadFolderArgs
    do {
      args = try invoke.parseArgs(OpenDownloadFolderArgs.self)
    } catch {
      return invoke.reject(error.localizedDescription)
    }

    guard let folder = fileURL(from: args.treeUri) else {
      return invoke.reject("No download folder URI available")
    }

    Task { @MainActor in
      self.openInFiles(folder, invoke: invoke)
    }
  }

  /// Shows a received file in Quick Look, else the folder it landed in in the
  /// Files app. Unlike Android there's no system Downloads list to fall back
  /// on, so with neither given this opens the app's own Documents folder.
  @objc func open_download_target(_ invoke: Invoke) {
    let args: OpenDownloadTargetArgs
    do {
      args = try invoke.parseArgs(OpenDownloadTargetArgs.self)
    } catch {
      return invoke.reject(error.localizedDescription)
    }

    let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    let relativePath = args.relativePath.trimmingCharacters(in: CharacterSet(charactersIn: "/ "))

    Task { @MainActor in
      if let file = self.fileURL(from: args.uri), !self.isDirectory(file) {
        return self.preview(file, invoke: invoke)
      }
      let folder =
        self.fileURL(from: args.uri)
        ?? (relativePath.isEmpty ? documents : documents.appendingPathComponent(relativePath))
      self.openInFiles(folder, invoke: invoke)
    }
  }

  @MainActor
  func openInFiles(_ folder: URL, invoke: Invoke) {
    // The Files app opens at a local folder given this scheme. It isn't
    // documented, but is what apps showing their own files use.
    guard
      var components = URLComponents(url: folder, resolvingAgainstBaseURL: false)
    else {
      return invoke.reject("Invalid folder path: \(folder.path)")
    }
    components.scheme = "shareddocuments"
    guard let filesURL = components.url else {
      return invoke.reject("Invalid folder path: \(folder.path)")
    }

    UIApplication.shared.open(filesURL) { opened in
      if opened {
        invoke.resolve()
      } else {
        invoke.reject("No app available to open the downloaded files")
      }
    }
  }

  @MainActor
  func preview(_ file: URL, invoke: Invoke) {
    guard let parentVC = manager.viewController else {
      return invoke.reject("Unable to get parent view controller")
    }
    let session = PreviewSession(file: file) { [weak self] in self?.previewSession = nil }
    previewSession = session
    session.present(on: parentVC)
    invoke.resolve()
  }

  /// Accepts a `file://` URL or an absolute path, as stored by either side.
  func fileURL(from string: String) -> URL? {
    let trimmed = string.trimmingCharacters(in: .whitespaces)
    if trimmed.isEmpty { return nil }
    if let url = URL(string: trimmed), url.isFileURL { return url }
    return trimmed.hasPrefix("/") ? URL(fileURLWithPath: trimmed) : nil
  }

  func isDirectory(_ url: URL) -> Bool {
    (try? url.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true
  }
}
