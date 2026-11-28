//! A stand-in for the acquisition provider's outside world: a yt-dlp script that serves a
//! local file. Nothing here touches the network.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;

/// A fake `yt-dlp`: `--version` answers a recent version, a search (`-J`) answers one video
/// titled `<artist> - <title>`, and a download copies `source` into the `-P` directory as
/// `audio.<ext>` while printing progress lines. With a file named `hang` next to the script,
/// downloads wait until it is removed; each run appends its arguments to `calls`.
#[derive(Clone, Debug)]
pub struct FakeYtdlp {
    pub dir: PathBuf,
    pub script: PathBuf,
}

impl FakeYtdlp {
    pub fn write(
        dir: &Path,
        source: &Path,
        artist: &str,
        title: &str,
        seconds: f32,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("m4a");
        let search = json!({
            "entries": [{
                "id": "e2eVideo001",
                "title": format!("{artist} - {title} (Official Audio)"),
                "uploader": artist,
                "duration": seconds,
            }]
        })
        .to_string();
        let script = dir.join("yt-dlp");
        let body = format!(
            r#"#!/bin/sh
dir='{dir}'
echo "$*" >> "$dir/calls"
for a in "$@"; do
  if [ "$a" = "--version" ]; then date -u +%Y.%m.%d; exit 0; fi
done
case " $* " in
  *" -J "*) cat <<'JSON'
{search}
JSON
  exit 0 ;;
  *" --skip-download "*) exit 0 ;;
  *" -o - "*) cat '{source}'; exit 0 ;;
esac
out=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-P" ]; then out="$a"; fi
  prev="$a"
done
[ -n "$out" ] || {{ echo "ERROR: no -P" >&2; exit 2; }}
size=$(wc -c < '{source}' | tr -d ' ')
echo "{{\"downloaded_bytes\": 0, \"total_bytes\": $size}}"
while [ -e "$dir/hang" ]; do sleep 0.1; done
mkdir -p "$out"
cp '{source}' "$out/audio.{ext}"
echo "{{\"downloaded_bytes\": $size, \"total_bytes\": $size}}"
"#,
            dir = dir.display(),
            source = source.display(),
        );
        std::fs::write(&script, body)?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            script,
        })
    }

    /// Makes downloads wait until [`FakeYtdlp::release`].
    pub fn hold(&self) -> std::io::Result<()> {
        std::fs::write(self.dir.join("hang"), b"")
    }

    pub fn release(&self) {
        let _ = std::fs::remove_file(self.dir.join("hang"));
    }

    /// The argument lists it was run with.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}
