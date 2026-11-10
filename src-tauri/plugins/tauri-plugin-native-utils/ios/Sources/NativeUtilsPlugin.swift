import Tauri
import WebKit

// iOS side of the native-utils plugin. Each command in ../../src/mobile.rs
// (`run_mobile_plugin("<name>", ...)`) dispatches to the `@objc` method of
// the same name here. The Kotlin implementation in ../../android is the
// reference for what each should do.
//
// Commands are grouped by feature in the NativeUtilsPlugin+*.swift extensions;
// this file holds the state they share.

class NativeUtilsPlugin: Plugin {
  // Commands with no method (export_to_tree, export_to_media_store,
  // write_text_to_uri, fetch_update_manifest and the presence service) are
  // Android-only: their callers are behind `cfg(target_os = "android")` or
  // `IS_ANDROID`.

  var pickingSession: FilePickingSession?
  var previewSession: PreviewSession?

  /// Running copies by channel id, with the folder each stages into, so
  /// `cancel_job` can stop one and clean up after it. Main actor only.
  var jobs = [UInt64: (task: Task<Void, Never>, folder: URL)]()

  /// The chosen download folder, held open for the life of the process so
  /// Rust can write received files into it with plain `std::fs` calls.
  var accessedDownloadFolder: URL?

  weak var webview: WKWebView?

  override func load(webview: WKWebView) {
    super.load(webview: webview)
    self.webview = webview
    restoreDownloadFolderAccess()
  }
}

// Public: in optimized builds Swift 6.4 gives an internal `@_cdecl` function
// hidden visibility, and the Rust side then can't link against it.
@_cdecl("init_plugin_native_utils")
public func initPlugin() -> Plugin {
  return NativeUtilsPlugin()
}
