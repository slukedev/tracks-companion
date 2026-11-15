# Homebrew formula for Trackside. The release workflow fills in @VERSION@ and @SHA256@ and
# attaches the result to the GitHub release; it is then copied into the tap repository
# (slukedev/homebrew-tap). Install with:
#
#   brew install slukedev/tap/trackside
#   brew services start trackside

class Trackside < Formula
  desc "Self-hosted music server for the Tracks app"
  homepage "https://github.com/slukedev/tracks/tree/main/Server"
  url "https://github.com/slukedev/tracks/archive/refs/tags/trackside-v@VERSION@.tar.gz"
  sha256 "@SHA256@"
  license "GPL-3.0-only"
  head "https://github.com/slukedev/tracks.git", branch: "main"

  livecheck do
    url :stable
    regex(/^trackside-v(\d+(?:\.\d+)+)$/i)
  end

  depends_on "rust" => :build
  depends_on "ffmpeg"

  def install
    cd "Server" do
      system "cargo", "install", *std_cargo_args(path: "crates/trackside-server")
    end
  end

  def post_install
    (var/"trackside").mkpath
    (var/"log").mkpath
  end

  def caveats
    <<~EOS
      Trackside keeps its database, keys and backups in:
        #{var}/trackside
      To add your music on the first start, set TRACKSIDE_MUSIC_DIR before starting the
      service, or add a library in the dashboard afterwards.

      After `brew services start trackside`, the setup token is in:
        #{var}/log/trackside.log
      and in #{var}/trackside/setup-token. Then open http://localhost:4747/setup.
    EOS
  end

  service do
    run [opt_bin/"trackside", "serve"]
    environment_variables TRACKSIDE_DATA_DIR: var/"trackside", TRACKSIDE_LOG__FORMAT: "pretty"
    keep_alive crashed: true
    working_dir var/"trackside"
    log_path var/"log/trackside.log"
    error_log_path var/"log/trackside.log"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/trackside version")

    port = free_port
    ENV["TRACKSIDE_DATA_DIR"] = testpath/"data"
    ENV["TRACKSIDE_SERVER__LISTEN"] = "127.0.0.1:#{port}"
    pid = spawn bin/"trackside", "serve"
    begin
      sleep 3
      assert_equal "ok", shell_output("curl -fsS http://127.0.0.1:#{port}/healthz").strip
      assert_path_exists testpath/"data/setup-token"
    ensure
      Process.kill("TERM", pid)
      Process.wait(pid)
    end
  end
end
