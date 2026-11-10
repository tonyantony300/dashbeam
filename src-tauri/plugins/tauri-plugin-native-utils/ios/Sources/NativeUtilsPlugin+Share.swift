import Foundation
import Tauri

// Files shared into DashBeam through the share extension.

extension NativeUtilsPlugin {
  /// Hands the oldest share saved by the share extension
  /// (src-tauri/ios/ShareExtension) to the frontend, which polls this.
  /// Resolves `false` when nothing is waiting.
  @objc func consume_share_intent(_ invoke: Invoke) {
    let args: SelectorArgs
    do {
      args = try invoke.parseArgs(SelectorArgs.self)
    } catch {
      return invoke.reject(error.localizedDescription)
    }

    Task { @MainActor in
      guard let folder = takeSharedBatch() else { return invoke.resolve(false) }
      reportSharedBatch(in: folder, channel: args.channel)
      invoke.resolve(true)
    }
  }

  /// Moves the oldest complete share out of the app group's `share_inbox`
  /// into a fresh cache folder, keeping its `<index>/<name>` layout.
  @MainActor
  func takeSharedBatch() -> URL? {
    let fileManager = FileManager.default
    guard let groupID = Bundle.main.object(forInfoDictionaryKey: "DashBeamAppGroup") as? String,
      let container = fileManager.containerURL(forSecurityApplicationGroupIdentifier: groupID)
    else { return nil }

    let inbox = container.appendingPathComponent("share_inbox")
    // Batches are named by millisecond timestamp; `.partial` ones are still
    // being written by the extension.
    let entries = (try? fileManager.contentsOfDirectory(at: inbox, includingPropertiesForKeys: nil)) ?? []
    // An extension killed mid-save leaves its `.partial` behind; anything
    // this old was abandoned.
    let abandonedBefore = Int(Date().addingTimeInterval(-60 * 60).timeIntervalSince1970 * 1000)
    for partial in entries where partial.pathExtension == "partial" {
      if let started = Int(partial.deletingPathExtension().lastPathComponent),
        started < abandonedBefore
      {
        try? fileManager.removeItem(at: partial)
      }
    }
    let oldest = entries
      .filter { $0.pathExtension != "partial" }
      .min { $0.lastPathComponent < $1.lastPathComponent }
    guard let batch = oldest else { return nil }

    let folder = newCacheFolder()
    do {
      try fileManager.createDirectory(
        at: folder.deletingLastPathComponent(), withIntermediateDirectories: true)
      try fileManager.moveItem(at: batch, to: folder)
      return folder
    } catch {
      // Dropped rather than retried: the frontend polls every second, and a
      // batch that can't be moved would fail the same way each time.
      NSLog("native-utils: dropping unreadable share \(batch.lastPathComponent): \(error)")
      try? fileManager.removeItem(at: batch)
      return nil
    }
  }

  /// Sends the messages Android's `startUriCopy` does: one file reports its own
  /// path, several report the folder first and then every file's path.
  @MainActor
  func reportSharedBatch(in folder: URL, channel: Channel) {
    do {
      let fileManager = FileManager.default
      let paths = try fileManager.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil)
        .sorted { (Int($0.lastPathComponent) ?? 0) < (Int($1.lastPathComponent) ?? 0) }
        .compactMap { try fileManager.contentsOfDirectory(at: $0, includingPropertiesForKeys: nil).first }
      let size = String(try paths.reduce(0) { $0 + (try totalSize(of: $1)) })

      if paths.count == 1 {
        let path = paths[0].path
        try channel.send(
          CopyProgress(
            copiedBytes: "0", totalBytes: size, cachedPath: path, cachedPaths: nil,
            completed: false, progress: 0))
        try channel.send(
          CopyProgress(
            copiedBytes: size, totalBytes: size, cachedPath: path, cachedPaths: nil,
            completed: true, progress: 1))
      } else {
        let cachedPaths = String(
          decoding: try JSONEncoder().encode(paths.map(\.path)), as: UTF8.self)
        try channel.send(
          CopyProgress(
            copiedBytes: "0", totalBytes: size, cachedPath: folder.path, cachedPaths: nil,
            completed: false, progress: 0))
        try channel.send(
          CopyProgress(
            copiedBytes: size, totalBytes: size, cachedPath: nil, cachedPaths: cachedPaths,
            completed: true, progress: 1))
      }
    } catch {
      try? FileManager.default.removeItem(at: folder)
      try? channel.send(CopyFailure(error: error.localizedDescription))
    }
  }
}
