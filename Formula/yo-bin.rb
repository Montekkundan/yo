class YoBin < Formula
  version '1.3.5'
  desc "Ask your terminal anything using AI."
  homepage "https://github.com/montekkundan/yo"

  if OS.mac?
    url "https://github.com/lershi-devlabs/yo/releases/download/1.3.5/yo-1.3.5-x86_64-apple-darwin.tar.gz"
    sha256 "d670c52ec3bedfbe97bd89f9c9550065dab1560592bd7a841b362e982ae66b3d"
  elsif OS.linux?
    url "https://github.com/lershi-devlabs/yo/releases/download/1.3.5/yo-1.3.5-x86_64-unknown-linux-musl.tar.gz"
    sha256 "d670c52ec3bedfbe97bd89f9c9550065dab1560592bd7a841b362e982ae66b3d"
  end

  def install
    bin.install "yo"
  end
end
