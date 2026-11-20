import Foundation
import UniformTypeIdentifiers

/// Saves shared files into the app group's `share_inbox/<timestamp>/<index>/<name>`,
/// where the native-utils plugin's `consume_share_intent` picks them up once
/// the app is open; this mirrors what the Android share intent hands the app.
enum ShareInbox {
  /// Saves every attachment that has a file representation and returns how
  /// many were saved.
  static func save(_ providers: [NSItemProvider]) async throws -> Int {
    let fileManager = FileManager.default
    guard let groupID = Bundle.main.object(forInfoDictionaryKey: "DashBeamAppGroup") as? String,
      let container = fileManager.containerURL(forSecurityApplicationGroupIdentifier: groupID)
    else {
      throw ShareError.noAppGroup
    }

    // Written under a `.partial` name and renamed once complete, so the app
    // never picks up a share that is still being saved.
    let inbox = container.appendingPathComponent("share_inbox")
    let batchName = String(Int(Date().timeIntervalSince1970 * 1000))
    let partial = inbox.appendingPathComponent(batchName + ".partial")
    try fileManager.createDirectory(at: partial, withIntermediateDirectories: true)

    do {
      var index = 0
      for provider in providers {
        // Highest-fidelity type first, as the provider orders them.
        guard
          let type = provider.registeredTypeIdentifiers.first(where: {
            UTType($0)?.conforms(to: .data) == true
          })
        else { continue }

        let destination = partial.appendingPathComponent(String(index))
        try fileManager.createDirectory(at: destination, withIntermediateDirectories: true)
        try await saveFile(from: provider, type: type, into: destination)
        index += 1
      }
      guard index > 0 else { throw ShareError.nothingToShare }

      try fileManager.moveItem(at: partial, to: inbox.appendingPathComponent(batchName))
      return index
    } catch {
      try? fileManager.removeItem(at: partial)
      throw error
    }
  }

  private static func saveFile(from provider: NSItemProvider, type: String, into folder: URL)
    async throws
  {
    let suggestedName = provider.suggestedName
    try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
      // The URL is a temporary copy that is deleted once this handler returns.
      _ = provider.loadFileRepresentation(forTypeIdentifier: type) { url, error in
        guard let url else {
          return continuation.resume(throwing: error ?? ShareError.nothingToShare)
        }
        do {
          try FileManager.default.copyItem(
            at: url, to: folder.appendingPathComponent(fileName(suggested: suggestedName, at: url)))
          continuation.resume()
        } catch {
          continuation.resume(throwing: error)
        }
      }
    }
  }

  /// The provider's suggested name (e.g. a Photos asset's `IMG_0001`) reads
  /// better than the temporary file's, which is often a UUID.
  private static func fileName(suggested: String?, at url: URL) -> String {
    // Set by the sharing app, so it must stay a single path component.
    let suggested = suggested?
      .replacingOccurrences(of: "/", with: "_")
      .replacingOccurrences(of: ":", with: "_")
    guard let suggested, !suggested.isEmpty, suggested != ".", suggested != ".." else {
      return url.lastPathComponent
    }
    if (suggested as NSString).pathExtension.isEmpty, !url.pathExtension.isEmpty {
      return suggested + "." + url.pathExtension
    }
    return suggested
  }
}

enum ShareError: LocalizedError {
  case noAppGroup
  case nothingToShare

  var errorDescription: String? {
    switch self {
    case .noAppGroup: return "DashBeam's shared storage is unavailable"
    case .nothingToShare: return "Nothing that DashBeam can send was shared"
    }
  }
}
