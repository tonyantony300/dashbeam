import Foundation
import Tauri
import UniformTypeIdentifiers

// Picking files or folders to send and staging them in the cache.

extension NativeUtilsPlugin {
  @objc func select_send_document(_ invoke: Invoke) {
    selectAndCopy(invoke, contentTypes: [.content])
  }

  @objc func select_send_folder(_ invoke: Invoke) {
    selectAndCopy(invoke, contentTypes: [.folder])
  }

  @objc func cancel_job(_ invoke: Invoke) {
    let args: CancelJobArgs
    do {
      args = try invoke.parseArgs(CancelJobArgs.self)
    } catch {
      return invoke.reject(error.localizedDescription)
    }

    Task { @MainActor in
      guard let job = self.jobs.removeValue(forKey: args.channelId) else {
        return invoke.reject("Trying to cancel a non existing job")
      }
      // `copyItem` can't be interrupted, so this waits for the current item.
      job.task.cancel()
      await job.task.value
      try? FileManager.default.removeItem(at: job.folder)
      invoke.resolve()
    }
  }

  private func selectAndCopy(_ invoke: Invoke, contentTypes: [UTType]) {
    let args: SelectorArgs
    do {
      args = try invoke.parseArgs(SelectorArgs.self)
    } catch {
      return invoke.reject(error.localizedDescription)
    }

    Task { @MainActor in
      guard let parentVC = self.manager.viewController else {
        return invoke.reject("Unable to get parent view controller")
      }

      let session = FilePickingSession(contentTypes: contentTypes) { urls in
        // Also breaks the plugin -> session -> closure retain cycle.
        self.pickingSession = nil
        // Like Android: `false` when cancelled, `true` once the copy has started;
        // the file itself arrives through the channel.
        guard let url = urls?.first else { return invoke.resolve(false) }
        self.startCopy(of: url, channel: args.channel)
        invoke.resolve(true)
      }
      self.pickingSession = session
      session.start(on: parentVC)
    }
  }

  /// Copies a picked file or folder into `Caches/file_cache/<timestamp>/`,
  /// matching where Android stages them, and reports it through `channel`.
  @MainActor
  private func startCopy(of source: URL, channel: Channel) {
    let folder = newCacheFolder()

    let task = Task.detached {
      do {
        try stageCopy(of: source, into: folder, channel: channel)
      } catch is CancellationError {
        // `cancel_job` asked for this and cleans up the folder itself.
      } catch {
        try? FileManager.default.removeItem(at: folder)
        try? channel.send(CopyFailure(error: error.localizedDescription))
      }
      await MainActor.run { _ = self.jobs.removeValue(forKey: channel.id) }
    }
    // Registered before the task's own removal can run: that hops back onto
    // the main actor, which is busy here until this returns.
    jobs[channel.id] = (task, folder)
  }
}

/// `Caches/file_cache/<timestamp>`, where Android stages files too.
func newCacheFolder() -> URL {
  FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
    .appendingPathComponent("file_cache")
    .appendingPathComponent(String(Int(Date().timeIntervalSince1970 * 1000)))
}

/// Copies `source` into `folder`, sending Android's start and completion
/// `CopyProgress` messages. Throws `CancellationError` if cancelled meanwhile.
private func stageCopy(of source: URL, into folder: URL, channel: Channel) throws {
  let fileManager = FileManager.default

  // The picker hands out the original item (`asCopy: false`), which is only
  // readable while security-scoped access is held.
  let accessing = source.startAccessingSecurityScopedResource()
  defer { if accessing { source.stopAccessingSecurityScopedResource() } }

  try fileManager.createDirectory(at: folder, withIntermediateDirectories: true)
  let target = folder.appendingPathComponent(source.lastPathComponent)

  // Coordinating the read makes File Provider / iCloud items download
  // first; the block runs once they are available locally.
  var coordinationError: NSError?
  var copyError: Error?
  NSFileCoordinator().coordinate(readingItemAt: source, error: &coordinationError) { readURL in
    do {
      try Task.checkCancellation()
      let size = String(try totalSize(of: readURL))

      // Like Android's first `copyUri` emit: announces the size and destination.
      try channel.send(
        CopyProgress(
          copiedBytes: "0",
          totalBytes: size,
          cachedPath: target.path,
          cachedPaths: nil,
          completed: false,
          progress: 0
        ))

      // Recursive for folders, and a clone when on the same APFS volume.
      try fileManager.copyItem(at: readURL, to: target)
      try Task.checkCancellation()

      try channel.send(
        CopyProgress(
          copiedBytes: size,
          totalBytes: size,
          cachedPath: target.path,
          cachedPaths: nil,
          completed: true,
          progress: 1
        ))
    } catch {
      copyError = error
    }
  }
  if let error = coordinationError ?? copyError { throw error }
}

/// Size of a file, or the summed size of the regular files under a folder.
func totalSize(of url: URL) throws -> Int {
  let keys: Set<URLResourceKey> = [.isDirectoryKey, .isRegularFileKey, .fileSizeKey]
  let values = try url.resourceValues(forKeys: keys)
  guard values.isDirectory == true else { return values.fileSize ?? 0 }

  var total = 0
  let enumerator = FileManager.default.enumerator(
    at: url, includingPropertiesForKeys: Array(keys))
  while let child = enumerator?.nextObject() as? URL {
    let childValues = try child.resourceValues(forKeys: keys)
    if childValues.isRegularFile == true { total += childValues.fileSize ?? 0 }
  }
  return total
}
