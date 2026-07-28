# Homebrew formula for ryll.
#
# This template is used by the release workflow to generate
# the formula published to shakenfist/homebrew-tap. To use
# the tap:
#
#   brew tap shakenfist/tap
#   brew install ryll
#
# Or in one command:
#
#   brew install shakenfist/tap/ryll
#
# The PLACEHOLDER_URL and PLACEHOLDER_SHA256 values are
# replaced by the release automation (Phase 7) with the
# actual GitHub Release asset URL and checksum.

class Ryll < Formula
  desc "A Rust SPICE VDI client"
  homepage "https://github.com/shakenfist/ryll"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "PLACEHOLDER_URL"
      sha256 "PLACEHOLDER_SHA256"
    end
  end

  def install
    bin.install "ryll"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ryll --version")
  end
end
