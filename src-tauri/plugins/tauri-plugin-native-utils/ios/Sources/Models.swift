import Foundation
import Tauri

// Argument and response types for the commands, matching ../../src/models.rs
// and the Kotlin `@InvokeArg` classes.

struct SelectorArgs: Decodable {
  let channel: Channel
}

struct CancelJobArgs: Decodable {
  let channelId: UInt64
}

struct OpenDownloadFolderArgs: Decodable {
  let treeUri: String
}

struct OpenDownloadTargetArgs: Decodable {
  let uri: String
  let relativePath: String
}

struct DownloadFolderSelectionResponse: Encodable {
  /// The folder's `file://` URL; on Android this is the SAF tree URI.
  let uri: String
  let path: String
}

/// System bar and sensor housing insets, in CSS pixels.
struct WindowInsetsResponse: Encodable {
  let top: Double
  let right: Double
  let bottom: Double
  let left: Double
}

struct CopyProgress: Encodable {
  let copiedBytes: String
  let totalBytes: String
  let cachedPath: String?
  let cachedPaths: String?
  let completed: Bool
  let progress: Double
}

struct CopyFailure: Encodable {
  let error: String
  var progress: Double = -1
  var copiedBytes = "0"
  var totalBytes = "0"
}
