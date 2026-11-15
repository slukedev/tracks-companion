//! Scanning a real library: the first scan, broken files, Unicode names, duplicates, and
//! files changing while the server runs.

mod common;

use common::{boot, snapshot_songs, titles};

#[tokio::test(flavor = "multi_thread")]
async fn the_first_scan_from_music_dir_finds_every_song() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s).await? else {
        return Ok(());
    };
    let (songs, _) = snapshot_songs(&env.client, &env.key).await?;
    let mut expected: Vec<String> = env.lib.tracks.iter().map(|t| t.title.clone()).collect();
    expected.push(String::from("If I Were a Boy"));
    expected.sort();
    assert_eq!(titles(&songs), expected, "{}", env.server.log_text());
    Ok(())
}

/// Files written moments before the first scan are inside `scanner.settle`; the follow-up
/// pass must pick them up without waiting for the next periodic scan.
#[tokio::test(flavor = "multi_thread")]
async fn freshly_written_files_are_imported_by_the_follow_up_pass() -> anyhow::Result<()> {
    let Some(env) = boot(|s| s.env("TRACKSIDE_SCANNER__SETTLE", "3s")).await? else {
        return Ok(());
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let (songs, _) = snapshot_songs(&env.client, &env.key).await?;
        if songs.len() == env.lib.tracks.len() + 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {} songs after a minute: {}",
            songs.len(),
            env.server.log_text()
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Ok(())
}
