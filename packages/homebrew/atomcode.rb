# typed: false
# frozen_string_literal: true

# AtomCode — open-source terminal AI coding agent written in Rust.
#
# This Formula is maintained at https://atomgit.com/atomgit_atomcode/homebrew-tap
# and mirrors the pre-compiled binaries published at:
#   https://atomgit.com/atomgit_atomcode/atomcode/releases
#
# After each AtomCode release, this file's version + sha256 are updated
# automatically by the CI pipeline. See RELEASE.md for details.

class Atomcode < Formula
  desc "Open-source terminal AI coding agent — connect any LLM, edit code, run commands, verify autonomously"
  homepage "https://atomgit.com/atomgit_atomcode/atomcode"
  version "4.23.1"

  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://atomgit.com/atomgit_atomcode/atomcode/releases/download/v4.23.1/atomcode-v4.23.1-darwin-arm64"
      sha256 "3eccfe1b29c1a67e1f7ef76ba207ddac681c9fd6a519f81a9e591979e323d75a"
    else
      url "https://atomgit.com/atomgit_atomcode/atomcode/releases/download/v4.23.1/atomcode-v4.23.1-darwin-x64"
      sha256 "298749a272e99ad4c770481b068bf60ac266f0d13cbf5a2a21d771e847e0ee7c"
    end
  end

  on_linux do
    if Hardware::CPU.arm? && Hardware::CPU.is_64_bit?
      url "https://atomgit.com/atomgit_atomcode/atomcode/releases/download/v4.23.1/atomcode-v4.23.1-linux-arm64"
      sha256 "99cf2ccfccb001c022a1afa590eab2af5aeee0436a022ce6218dacca545ca8db"
    else
      url "https://atomgit.com/atomgit_atomcode/atomcode/releases/download/v4.23.1/atomcode-v4.23.1-linux-x64"
      sha256 "e93eae813b8184e448984f0cd514389c243c96f6139755fbe577ba46db63b413"
    end
  end

  # ── no autoupload / autoupdate — the binary is the release asset ──
  # `head` builds are unsupported because the binary is pre-compiled;
  # developers should use `cargo install --path crates/atomcode-cli` instead.

  def install
    # The downloaded file has the release-format name; we rename it to
    # "atomcode" for the bin/ slot so the user gets a clean `atomcode`
    # command regardless of platform.
    found = Dir["atomcode-v#{version}-*"]
    raise "AtomCode binary not found (version #{version}) in #{Dir.pwd}" if found.empty?
    bin.install found.first => "atomcode"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/atomcode --version")
  end
end
