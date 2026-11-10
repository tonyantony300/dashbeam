import UIKit

/// A spinner while files are saved, then an icon, title, message and Done
/// button describing how it went.
class ShareStatusView: UIView {
  private let spinner = UIActivityIndicatorView(style: .large)
  private let iconView = UIImageView()
  private let titleLabel = UILabel()
  private let messageLabel = UILabel()
  let doneButton = UIButton(type: .system)

  override init(frame: CGRect) {
    super.init(frame: frame)
    backgroundColor = .systemBackground

    iconView.contentMode = .scaleAspectFit
    iconView.preferredSymbolConfiguration = UIImage.SymbolConfiguration(pointSize: 56)
    titleLabel.font = .preferredFont(forTextStyle: .title2)
    titleLabel.adjustsFontForContentSizeCategory = true
    messageLabel.font = .preferredFont(forTextStyle: .body)
    messageLabel.adjustsFontForContentSizeCategory = true
    messageLabel.textColor = .secondaryLabel
    for label in [titleLabel, messageLabel] {
      label.textAlignment = .center
      label.numberOfLines = 0
    }
    doneButton.setTitle("Done", for: .normal)
    doneButton.titleLabel?.font = .preferredFont(forTextStyle: .headline)

    let stack = UIStackView(arrangedSubviews: [
      spinner, iconView, titleLabel, messageLabel, doneButton,
    ])
    stack.axis = .vertical
    stack.alignment = .center
    stack.spacing = 12
    stack.setCustomSpacing(24, after: messageLabel)
    stack.translatesAutoresizingMaskIntoConstraints = false
    addSubview(stack)
    NSLayoutConstraint.activate([
      stack.centerYAnchor.constraint(equalTo: safeAreaLayoutGuide.centerYAnchor),
      stack.leadingAnchor.constraint(equalTo: layoutMarginsGuide.leadingAnchor),
      stack.trailingAnchor.constraint(equalTo: layoutMarginsGuide.trailingAnchor),
    ])

    titleLabel.text = "Preparing files…"
    iconView.isHidden = true
    messageLabel.isHidden = true
    doneButton.isHidden = true
    spinner.startAnimating()
  }

  required init?(coder: NSCoder) {
    fatalError("init(coder:) has not been implemented")
  }

  func showSaved(count: Int) {
    showResult(
      symbol: "checkmark.circle.fill", tint: .systemGreen,
      title: "Ready to send",
      message: count == 1
        ? "Open DashBeam to send this file."
        : "Open DashBeam to send these \(count) files.")
  }

  func showFailure(_ error: Error) {
    showResult(
      symbol: "exclamationmark.triangle.fill", tint: .systemOrange,
      title: "Couldn't add files",
      message: error.localizedDescription)
  }

  private func showResult(symbol: String, tint: UIColor, title: String, message: String) {
    spinner.stopAnimating()
    spinner.isHidden = true
    iconView.image = UIImage(systemName: symbol)
    iconView.tintColor = tint
    iconView.isHidden = false
    titleLabel.text = title
    messageLabel.text = message
    messageLabel.isHidden = false
    doneButton.isHidden = false
  }
}
