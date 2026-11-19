import Foundation
import Tauri

// Where received files are saved, and keeping access to it across launches.

extension NativeUtilsPlugin {
  static let downloadFolderBookmarkKey = "nativeUtils.downloadFolderBookmark"

  @objc func select_download_folder(_ invoke: Invoke) {
    Task { @MainActor in
      guard let parentVC = self.manager.viewController else {
        return invoke.reject("Unable to get parent view controller")
      }

      let session = FilePickingSession(contentTypes: [.folder]) { urls in
        self.pickingSession = nil
        // Like Android, a cancelled picker resolves with nothing.
        guard let url = urls?.first else { return invoke.resolve() }

        do {
          try self.setDownloadFolder(url)
          invoke.resolve(DownloadFolderSelectionResponse(uri: url.absoluteString, path: url.path))
        } catch {
          invoke.reject(error.localizedDescription)
        }
      }
      self.pickingSession = session
      session.start(on: parentVC)
    }
  }

  @MainActor
  func setDownloadFolder(_ url: URL) throws {
    guard url.startAccessingSecurityScopedResource() else {
      throw CocoaError(.fileReadNoPermission)
    }
    let bookmark = try url.bookmarkData()

    accessedDownloadFolder?.stopAccessingSecurityScopedResource()
    accessedDownloadFolder = url
    UserDefaults.standard.set(bookmark, forKey: Self.downloadFolderBookmarkKey)
  }

  /// Picker access ends with the process; the bookmark saved by
  /// `select_download_folder` restores it on the next launch.
  func restoreDownloadFolderAccess() {
    let defaults = UserDefaults.standard
    guard let bookmark = defaults.data(forKey: Self.downloadFolderBookmarkKey) else { return }

    var isStale = false
    guard let url = try? URL(resolvingBookmarkData: bookmark, bookmarkDataIsStale: &isStale),
      url.startAccessingSecurityScopedResource()
    else {
      NSLog("native-utils: download folder bookmark no longer resolves")
      return
    }
    if isStale, let refreshed = try? url.bookmarkData() {
      defaults.set(refreshed, forKey: Self.downloadFolderBookmarkKey)
    }
    accessedDownloadFolder = url
  }
}
