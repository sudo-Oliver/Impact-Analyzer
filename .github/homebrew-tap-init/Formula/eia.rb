class Eia < Formula
  desc "Enterprise Impact Analyzer for Terraform / OpenTofu plans"
  homepage "https://github.com/sudo-Oliver/Impact-Analyzer"
  version "0.1.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/sudo-Oliver/Impact-Analyzer/releases/download/v0.1.0/eia-v0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "FILL_IN_AFTER_FIRST_RELEASE"
    else
      url "https://github.com/sudo-Oliver/Impact-Analyzer/releases/download/v0.1.0/eia-v0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "FILL_IN_AFTER_FIRST_RELEASE"
    end
  end

  on_linux do
    url "https://github.com/sudo-Oliver/Impact-Analyzer/releases/download/v0.1.0/eia-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "FILL_IN_AFTER_FIRST_RELEASE"
  end

  def install
    bin.install "eia"
  end

  test do
    system "#{bin}/eia", "--version"
  end
end
