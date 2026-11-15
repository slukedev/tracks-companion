//! `trackside-e2e`: the performance harness. It generates a library, runs the release
//! `trackside` binary against it, measures the budgets of architecture section 15, writes a
//! JSON report and fails on a regression against a stored baseline.
//!
//! `cargo run --release -p trackside-e2e -- --songs 50000 --baseline tests-e2e/baseline-50k.json
//! --max-regression 25 --report target/e2e-50k.json`

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Parser;
use serde_json::{Value, json};
use trackside_e2e::client::{device, send};
use trackside_e2e::perf::{self, Better, Report, regressions};
use trackside_e2e::{Auth, Client, Server, ServerSpec};

#[derive(Debug, Parser)]
#[command(name = "trackside-e2e", about = "Trackside's performance harness")]
struct Args {
    /// Songs in the generated library.
    #[arg(long, default_value_t = 50_000)]
    songs: usize,
    /// A previous report to compare with.
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Fail when a metric is worse than the baseline by more than this many percent.
    #[arg(long, default_value_t = 25.0)]
    max_regression: f64,
    /// Changes in milliseconds up to this size never count as regressions.
    #[arg(long, default_value_t = 3.0)]
    floor_ms: f64,
    /// Where to write the report.
    #[arg(long, default_value = "target/e2e-report.json")]
    report: PathBuf,
    /// Where to generate the library and data directory (deleted afterwards unless --keep).
    #[arg(long)]
    work: Option<PathBuf>,
    /// Keep the work directory.
    #[arg(long)]
    keep: bool,
    /// Reuse a library already generated in the work directory.
    #[arg(long)]
    reuse_library: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let work = args
        .work
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("target/e2e-work-{}", args.songs)));
    let result = run(&args, &work).await;
    if !args.keep {
        let _ = std::fs::remove_dir_all(&work);
    }
    let report = result?;
    std::fs::create_dir_all(args.report.parent().unwrap_or(Path::new(".")))?;
    std::fs::write(&args.report, serde_json::to_string_pretty(&report)? + "\n")?;
    println!("report written to {}", args.report.display());
    for line in &report.over_budget {
        println!("over budget: {line}");
    }
    if !report.regressions.is_empty() {
        for line in &report.regressions {
            eprintln!("regression: {line}");
        }
        bail!(
            "{} metrics regressed by more than {} %",
            report.regressions.len(),
            args.max_regression
        );
    }
    Ok(())
}

/// The release binary: `TRACKSIDE_BIN`, the one next to this executable, or a fresh build.
fn release_binary() -> anyhow::Result<PathBuf> {
    if let Ok(found) = trackside_e2e::binary() {
        return Ok(found);
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.toml");
    println!("building the release trackside binary");
    let status = Command::new(cargo)
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            "trackside-server",
            "--bin",
            "trackside",
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .status()?;
    if !status.success() {
        bail!("cargo build of trackside failed");
    }
    trackside_e2e::binary()
}

fn machine() -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    m.insert(String::from("os"), std::env::consts::OS.to_owned());
    m.insert(String::from("arch"), std::env::consts::ARCH.to_owned());
    m.insert(
        String::from("cpus"),
        std::thread::available_parallelism()
            .map_or(0, std::num::NonZero::get)
            .to_string(),
    );
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo")
        && let Some(model) = info.lines().find_map(|l| l.strip_prefix("model name"))
    {
        m.insert(
            String::from("cpu"),
            model.trim_start_matches([' ', '\t', ':']).to_owned(),
        );
    }
    m
}

#[expect(
    clippy::too_many_lines,
    clippy::many_single_char_names,
    reason = "the measurements read best in order, with short names for the client and timers"
)]
async fn run(args: &Args, work: &Path) -> anyhow::Result<Report> {
    let bin = release_binary()?;
    let ffmpeg = trackside_e2e::library::ffmpeg().context("the performance run needs ffmpeg")?;
    let music = work.join("music");
    let data = work.join("data");
    let _ = std::fs::remove_dir_all(&data);
    let mut report = Report {
        machine: machine(),
        ..Report::default()
    };
    let started = Instant::now();
    let synth = if args.reuse_library && music.exists() {
        let files = count_files(&music);
        perf::Synth {
            songs: files,
            files,
            ..perf::Synth::default()
        }
    } else {
        let _ = std::fs::remove_dir_all(&music);
        perf::synth_library(&ffmpeg, &music, args.songs, 0x7472_6163_6b73)?
    };
    add_covers(&ffmpeg, &music)?;
    report.songs = synth.songs;
    report.files = synth.files;
    report.notes.push(format!(
        "library: {} files, {} artists, {} albums, {} MB, generated in {:.0} s",
        synth.files,
        synth.artists,
        synth.albums,
        synth.bytes / 1_000_000,
        started.elapsed().as_secs_f64()
    ));
    report.notes.push(String::from(
        "times are measured by a client on the same host, so they include loopback HTTP; budgets are for 500k songs on the reference machines unless noted",
    ));

    let spec = ServerSpec::new(bin, data.clone())
        .env("TRACKSIDE_SCANNER__WATCH_DEBOUNCE", "5s")
        .env("TRACKSIDE_LOG__LEVEL", "warn,trackside_server=info");
    let mut server = Server::start(spec)?;
    let c = server.client();
    let token = server.setup_token().context("no setup token")?;
    let setup = c
        .post(
            "/setup",
            &Auth::None,
            &json!({"setup_token": token, "username": "perf", "password": "perf password 123"}),
        )
        .await?;
    anyhow::ensure!(setup.status == 201, "setup: {}", setup.text());
    let admin = Auth::Session(setup.cookie().context("no cookie")?);

    let scan_started = Instant::now();
    let created = c
        .post(
            "/admin/libraries",
            &admin,
            &json!({"name": "Perf", "root": std::fs::canonicalize(&music)?.display().to_string()}),
        )
        .await?;
    anyhow::ensure!(created.status == 201, "library: {}", created.text());
    let library = created.json()["id"].as_str().unwrap_or_default().to_owned();
    let scan = wait_scan(&c, &admin, &library, Duration::from_secs(3600)).await?;
    let scan_secs = scan_started.elapsed().as_secs_f64();
    let seen = scan["files_seen"].as_f64().unwrap_or(0.0);
    report.put(
        "scan_full_files_per_s",
        seen / scan_secs,
        "files/s",
        Better::Higher,
        Some((400.0, "medium machine, local SSD")),
    );
    report.notes.push(format!("first full scan: {scan}"));

    let login = c
        .post("/auth/login", &Auth::None, &json!({"username": "perf", "password": "perf password 123", "device": device("PERF", "Perf")}))
        .await?;
    let key = Auth::Key(
        login.json()["api_key"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    );

    let sync_started = Instant::now();
    let mut pages = Vec::new();
    let mut page_bytes = Vec::new();
    let mut songs: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    let delta_cursor = loop {
        let path = cursor.as_ref().map_or_else(
            || String::from("/sync?limit=5000"),
            |c| format!("/sync?limit=5000&cursor={c}"),
        );
        perf::pace().await;
        let t = Instant::now();
        let reply = send(c.request(reqwest::Method::GET, &c.api(&path), &key)).await?;
        let elapsed = t.elapsed();
        anyhow::ensure!(reply.status == 200, "sync: {}", reply.text());
        let body = decode_json(&reply)?;
        if body["snapshot"]["kind"] == "songs" {
            pages.push(elapsed);
            if page_bytes.is_empty() {
                let gzip = send(
                    c.request(reqwest::Method::GET, &c.api(&path), &key)
                        .header("accept-encoding", "gzip"),
                )
                .await?;
                anyhow::ensure!(
                    gzip.header("content-encoding") == Some("gzip"),
                    "sync pages are compressed"
                );
                page_bytes.push(gzip.body.len());
            }
            songs.extend(body["songs"].as_array().into_iter().flatten().cloned());
        }
        let next = body["next_cursor"].as_str().map(str::to_owned);
        if body["complete"].as_bool() == Some(true) {
            break next.context("no delta cursor")?;
        }
        cursor = Some(next.context("no next cursor")?);
    };
    let full_sync = sync_started.elapsed();
    let snapshot = perf::percentiles(&mut pages);
    report.put(
        "sync_snapshot_page_5000_p50_ms",
        snapshot.p50,
        "ms",
        Better::Lower,
        Some((120.0, "per page of 5000 songs")),
    );
    report.put(
        "sync_snapshot_page_5000_max_ms",
        snapshot.max,
        "ms",
        Better::Lower,
        Some((120.0, "per page of 5000 songs")),
    );
    let avg_kb = page_bytes.iter().sum::<usize>() as f64 / page_bytes.len().max(1) as f64 / 1000.0;
    report.put(
        "sync_snapshot_page_5000_kb_gzip",
        avg_kb,
        "KB",
        Better::Lower,
        Some((400.0, "brotli; this run asks for gzip")),
    );
    report.put(
        "sync_full_initial_s",
        full_sync.as_secs_f64(),
        "s",
        Better::Lower,
        Some((
            45.0 * songs.len() as f64 / 500_000.0,
            "45 s at 500k, scaled to this size",
        )),
    );
    anyhow::ensure!(
        songs.len() == synth.songs,
        "the snapshot holds {} songs of {}",
        songs.len(),
        synth.songs
    );

    let mut rng = perf::Rng::new(42);
    let albums: Vec<String> = c.get("/albums?limit=500", &key).await?.json()["albums"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["id"].as_str().map(str::to_owned))
        .collect();
    let artists: Vec<String> = songs
        .iter()
        .filter_map(|s| s["artist_id"].as_str().map(str::to_owned))
        .collect();
    let n = 300;
    let p = perf::sample(n, 20, |i| {
        let c = c.clone();
        let key = key.clone();
        async move {
            let path = if i % 2 == 0 {
                "/albums?limit=100"
            } else {
                "/artists?limit=100"
            };
            expect_ok(&c.get(path, &key).await?)
        }
    })
    .await?;
    put_latency(&mut report, "list_albums_artists", p, (5.0, 40.0));
    let album_ids = albums.clone();
    let picks: Vec<usize> = (0..n + 20).map(|_| rng.below(album_ids.len())).collect();
    let p = perf::sample(n, 20, |i| {
        let (c, key) = (c.clone(), key.clone());
        let path = format!("/albums/{}", album_ids[picks[i]]);
        async move { expect_ok(&c.get(&path, &key).await?) }
    })
    .await?;
    put_latency(&mut report, "album_page", p, (5.0, 40.0));
    let picks: Vec<usize> = (0..n + 20).map(|_| rng.below(artists.len())).collect();
    let p = perf::sample(n, 20, |i| {
        let (c, key) = (c.clone(), key.clone());
        let path = format!("/artists/{}", artists[picks[i]]);
        async move { expect_ok(&c.get(&path, &key).await?) }
    })
    .await?;
    put_latency(&mut report, "artist_page", p, (5.0, 40.0));

    let titles: Vec<String> = songs
        .iter()
        .filter_map(|s| s["title"].as_str().map(str::to_lowercase))
        .collect();
    let mut one_word = Vec::new();
    let mut multi = Vec::new();
    let mut typing = Vec::new();
    for _ in 0..n + 20 {
        let t = &titles[rng.below(titles.len())];
        let words: Vec<&str> = t.split_whitespace().collect();
        one_word.push(words[0].to_owned());
        if words.len() >= 2 {
            multi.push(format!(
                "{} {}",
                words[0],
                &words[1][..words[1].len().min(3)]
            ));
        } else {
            multi.push(format!("the {}", words[0]));
        }
        let w = words[words.len() - 1];
        for len in 1..=w.len().min(5) {
            typing.push(w[..len].to_owned());
        }
    }
    for (name, queries) in [
        ("search_one_word", &one_word),
        ("search_multi_word", &multi),
    ] {
        let p = perf::sample(n, 20, |i| {
            let (c, key) = (c.clone(), key.clone());
            let path = format!("/search?q={}&limit=20", queries[i].replace(' ', "%20"));
            async move { expect_ok(&c.get(&path, &key).await?) }
        })
        .await?;
        put_latency(&mut report, name, p, (5.0, 40.0));
    }
    let typing_n = typing.len().min(600);
    let p = perf::sample(typing_n - 20, 20, |i| {
        let (c, key) = (c.clone(), key.clone());
        let path = format!("/search?q={}&limit=20&types=song,album,artist", typing[i]);
        async move { expect_ok(&c.get(&path, &key).await?) }
    })
    .await?;
    report.put(
        "search_as_you_type_p99_ms",
        p.p99,
        "ms",
        Better::Lower,
        Some((25.0, "p99 at 500k")),
    );
    report.put(
        "search_as_you_type_p50_ms",
        p.p50,
        "ms",
        Better::Lower,
        None,
    );
    let hardest = [
        "the love",
        "love the",
        "night love",
        "a b",
        "the the",
        "love night dream",
    ];
    let mut worst: f64 = 0.0;
    for q in hardest {
        let p = perf::sample(20, 3, |_| {
            let (c, key) = (c.clone(), key.clone());
            let path = format!("/search?q={}&limit=20", q.replace(' ', "%20"));
            async move { expect_ok(&c.get(&path, &key).await?) }
        })
        .await?;
        worst = worst.max(p.p99);
    }
    report.put(
        "search_common_words_worst_p99_ms",
        worst,
        "ms",
        Better::Lower,
        Some((40.0, "list endpoint budget")),
    );

    let star_ids: Vec<String> = songs
        .iter()
        .take(100)
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect();
    for id in &star_ids {
        perf::pace().await;
        expect_ok(
            &c.put(&format!("/me/stars/song/{id}"), &key, &json!({}))
                .await?,
        )?;
    }
    let t = Instant::now();
    let delta = c.get(&format!("/sync?cursor={delta_cursor}"), &key).await?;
    let delta_ms = t.elapsed().as_secs_f64() * 1000.0;
    expect_ok(&delta)?;
    let stars = delta.json()["upserts"]["stars"]
        .as_array()
        .map_or(0, Vec::len);
    anyhow::ensure!(stars == 100, "the delta holds {stars} stars");
    report.put(
        "sync_delta_100_changes_ms",
        delta_ms,
        "ms",
        Better::Lower,
        Some((10.0, "server time")),
    );

    let song_ids: Vec<String> = songs
        .iter()
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect();
    let p = perf::sample(10, 2, |i| {
        let (c, key) = (c.clone(), key.clone());
        let listens: Vec<Value> = (0..500)
            .map(|j| {
                let id = &song_ids[(i * 500 + j) % song_ids.len()];
                json!({"client_listen_id": format!("perf-{i}-{j}"), "song_id": id,
                       "started_at": trackside_e2e::rfc3339(trackside_e2e::now_ms() - 86_400_000 + (j as i64) * 1000),
                       "played_ms": 1000, "title": "x", "artist": "y", "duration_ms": 1000})
            })
            .collect();
        async move {
            expect_ok(&c.post("/listens", &key, &json!({"client_time": trackside_e2e::rfc3339(trackside_e2e::now_ms()), "listens": listens})).await?)
        }
    })
    .await?;
    report.put(
        "listens_500_p50_ms",
        p.p50,
        "ms",
        Better::Lower,
        Some((80.0, "per batch")),
    );
    report.put(
        "listens_500_max_ms",
        p.max,
        "ms",
        Better::Lower,
        Some((80.0, "per batch")),
    );

    let mp3s: Vec<String> = songs
        .iter()
        .filter(|s| s["format"]["container"] == "mp3")
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect();
    let p = perf::sample(200, 10, |i| {
        let (c, key) = (c.clone(), key.clone());
        let path = format!("/songs/{}/stream", mp3s[i % mp3s.len()]);
        async move { ttfb(&c, &path, &key, Some("bytes=0-1")).await.map(|_| ()) }
    })
    .await?;
    report.put(
        "stream_ttfb_direct_p99_ms",
        p.p99,
        "ms",
        Better::Lower,
        Some((25.0, "LAN, warm disk")),
    );
    report.put(
        "stream_ttfb_direct_p50_ms",
        p.p50,
        "ms",
        Better::Lower,
        None,
    );

    let flacs: Vec<String> = songs
        .iter()
        .filter(|s| s["format"]["container"] == "flac")
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect();
    let t = Instant::now();
    for id in flacs.iter().take(5) {
        expect_ok(
            &c.get(&format!("/songs/{id}/download?profile=aac-128"), &key)
                .await?,
        )?;
    }
    report.put(
        "transcode_aac128_half_second_song_ms",
        t.elapsed().as_secs_f64() * 200.0,
        "ms",
        Better::Lower,
        None,
    );
    report.ungate("transcode_aac128_half_second_song_ms");
    let p = perf::sample(100, 5, |i| {
        let (c, key) = (c.clone(), key.clone());
        let path = format!("/songs/{}/stream?profile=aac-128", flacs[i % 5]);
        async move { ttfb(&c, &path, &key, Some("bytes=0-1")).await.map(|_| ()) }
    })
    .await?;
    report.put(
        "stream_ttfb_cached_transcode_p99_ms",
        p.p99,
        "ms",
        Better::Lower,
        Some((30.0, "cached transcode")),
    );

    let mut hls_times = Vec::new();
    for id in flacs.iter().skip(10).take(8) {
        let t = Instant::now();
        let decision = c
            .post("/playback/decisions", &key, &json!({"song_id": id, "network": "cellular",
                "client": {"direct": [{"container": "mp3", "codecs": ["mp3"]}], "hls": true, "max_bitrate_kbps": 256}}))
            .await?
            .json();
        anyhow::ensure!(decision["mode"] == "hls", "{decision}");
        let url = decision["url"].as_str().unwrap_or_default();
        let base = url.rsplit_once('/').map_or("", |(b, _)| b);
        expect_ok(&c.get(url, &key).await?)?;
        expect_ok(&c.get(&format!("{base}/init.mp4"), &key).await?)?;
        expect_ok(&c.get(&format!("{base}/seg-0.m4s"), &key).await?)?;
        hls_times.push(t.elapsed());
    }
    let p = perf::percentiles(&mut hls_times);
    report.put(
        "hls_first_segment_uncached_max_ms",
        p.max,
        "ms",
        Better::Lower,
        Some((700.0, "medium machine")),
    );

    let art_song = songs
        .iter()
        .find(|s| s["artwork_id"].is_string())
        .and_then(|s| s["artwork_id"].as_str())
        .unwrap_or_default()
        .to_owned();
    if art_song.is_empty() {
        report.notes.push(String::from(
            "no artwork found; artwork budgets not measured",
        ));
    } else {
        let t = Instant::now();
        expect_ok(&c.get(&format!("/art/{art_song}?size=600"), &key).await?)?;
        report.put(
            "artwork_600_first_resize_ms",
            t.elapsed().as_secs_f64() * 1000.0,
            "ms",
            Better::Lower,
            Some((120.0, "first resize")),
        );
        report.ungate("artwork_600_first_resize_ms");
        let p = perf::sample(100, 5, |_| {
            let (c, key) = (c.clone(), key.clone());
            let path = format!("/art/{art_song}?size=600");
            async move { expect_ok(&c.get(&path, &key).await?) }
        })
        .await?;
        report.put(
            "artwork_600_cached_p99_ms",
            p.p99,
            "ms",
            Better::Lower,
            Some((5.0, "cached")),
        );
    }

    let t = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..50 {
        let (c, key) = (c.clone(), key.clone());
        let path = format!("/songs/{}/stream", mp3s[i % mp3s.len()]);
        tasks.push(tokio::spawn(
            async move { ttfb(&c, &path, &key, None).await },
        ));
    }
    let mut worst = Duration::ZERO;
    for task in tasks {
        worst = worst.max(task.await??);
    }
    report.put(
        "concurrent_50_streams_worst_ttfb_ms",
        worst.as_secs_f64() * 1000.0,
        "ms",
        Better::Lower,
        None,
    );
    report.notes.push(format!(
        "50 concurrent streams finished in {:?}",
        t.elapsed()
    ));

    let t = Instant::now();
    let quick = c
        .post(
            &format!("/admin/libraries/{library}/scan"),
            &admin,
            &json!({"kind": "quick"}),
        )
        .await?;
    anyhow::ensure!(quick.status == 202, "{}", quick.text());
    wait_scan(&c, &admin, &library, Duration::from_secs(600)).await?;
    report.put(
        "scan_quick_unchanged_s",
        t.elapsed().as_secs_f64(),
        "s",
        Better::Lower,
        Some((
            20.0 * synth.files as f64 / 500_000.0,
            "20 s at 500k on SSD, scaled",
        )),
    );

    let folder = synth
        .paths
        .get(synth.paths.len() / 2)
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut track = trackside_e2e::library::Track::new(
        folder.join("99 Watcher Probe.mp3"),
        "Watcher Probe",
        "Probe Artist",
        "Probe Album",
    );
    track.freq = 777;
    track.seconds = 0.5;
    let before = latest_cursor(&c, &key, &delta_cursor).await?;
    let t = Instant::now();
    trackside_e2e::library::write_track(&ffmpeg, &music, &track)?;
    let mut cursor = before;
    loop {
        perf::pace().await;
        let reply = c.get(&format!("/sync?cursor={cursor}"), &key).await?.json();
        let found = reply["upserts"]["songs"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["title"] == "Watcher Probe");
        if found {
            break;
        }
        cursor = reply["next_cursor"].as_str().unwrap_or(&cursor).to_owned();
        anyhow::ensure!(
            t.elapsed() < Duration::from_secs(60),
            "the watcher never imported the probe"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    report.put(
        "watcher_to_sync_s",
        t.elapsed().as_secs_f64(),
        "s",
        Better::Lower,
        Some((8.0, "5 s debounce plus import")),
    );

    tokio::time::sleep(Duration::from_secs(3)).await;
    if let Some(kib) = server.anon_rss_kib() {
        report.put(
            "idle_rss_anon_mb",
            kib as f64 / 1024.0,
            "MB",
            Better::Lower,
            Some((120.0, "500k songs, excluding the page cache")),
        );
    }
    if let Some(kib) = server.rss_kib() {
        report.put(
            "idle_rss_total_mb",
            kib as f64 / 1024.0,
            "MB",
            Better::Lower,
            None,
        );
        report.ungate("idle_rss_total_mb");
    }
    let t = Instant::now();
    server.stop()?;
    report.put(
        "graceful_stop_ms",
        t.elapsed().as_secs_f64() * 1000.0,
        "ms",
        Better::Lower,
        None,
    );
    report.ungate("graceful_stop_ms");
    let mut cold = Vec::new();
    for _ in 0..3 {
        let spec = server
            .spec
            .clone()
            .env("TRACKSIDE_SERVER__LISTEN", server.addr.to_string());
        let t = Instant::now();
        let next = Server::start(spec)?;
        let c2 = next.client();
        loop {
            let ready = c2.http.get(c2.url("/readyz")).send().await;
            if ready.is_ok_and(|r| r.status() == 200) {
                break;
            }
            anyhow::ensure!(t.elapsed() < Duration::from_secs(30), "never ready");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        cold.push(t.elapsed());
        server = next;
        server.stop()?;
    }
    let p = perf::percentiles(&mut cold);
    report.put(
        "cold_start_to_ready_ms",
        p.p50,
        "ms",
        Better::Lower,
        Some((300.0, "no migrations, 500k songs")),
    );

    report.check_budgets();
    if let Some(path) = &args.baseline {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let baseline: Report = serde_json::from_str(&text)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                if baseline.songs != report.songs {
                    report.notes.push(format!(
                        "the baseline has {} songs, this run {}",
                        baseline.songs, report.songs
                    ));
                }
                report.regressions =
                    regressions(&report, &baseline, args.max_regression, args.floor_ms);
            }
            Err(e) => report
                .notes
                .push(format!("no baseline at {}: {e}", path.display())),
        }
    }
    Ok(report)
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => {
                    let audio = entry
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| ["mp3", "flac", "m4a"].contains(&e));
                    n += usize::from(audio);
                }
                _ => {}
            }
        }
    }
    n
}

/// Sidecar covers in a few album folders, for the artwork budgets.
fn add_covers(ffmpeg: &Path, music: &Path) -> anyhow::Result<()> {
    let mut added = 0;
    for artist in std::fs::read_dir(music)?.flatten() {
        if added >= 10 {
            break;
        }
        if !artist.file_type()?.is_dir() {
            continue;
        }
        if let Some(album) = std::fs::read_dir(artist.path())?
            .flatten()
            .find(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        {
            trackside_e2e::library::write_cover(ffmpeg, &album.path().join("cover.jpg"), "teal")?;
            added += 1;
        }
    }
    Ok(())
}

fn put_latency(report: &mut Report, name: &str, p: perf::Percentiles, budget: (f64, f64)) {
    report.put(
        &format!("{name}_p50_ms"),
        p.p50,
        "ms",
        Better::Lower,
        Some((budget.0, "p50 at 500k")),
    );
    report.put(
        &format!("{name}_p99_ms"),
        p.p99,
        "ms",
        Better::Lower,
        Some((budget.1, "p99 at 500k")),
    );
}

fn expect_ok(reply: &trackside_e2e::Reply) -> anyhow::Result<()> {
    if reply.status >= 300 {
        bail!("{}: {}", reply.status, reply.text());
    }
    Ok(())
}

fn decode_json(reply: &trackside_e2e::Reply) -> anyhow::Result<Value> {
    if reply.header("content-encoding") == Some("gzip") {
        bail!("gzip bodies need decoding; reqwest's gzip feature is off");
    }
    Ok(serde_json::from_slice(&reply.body)?)
}

/// Time to the first body byte.
async fn ttfb(c: &Client, path: &str, key: &Auth, range: Option<&str>) -> anyhow::Result<Duration> {
    let t = Instant::now();
    let mut request = c.request(reqwest::Method::GET, &c.api(path), key);
    if let Some(range) = range {
        request = request.header("range", range);
    }
    let mut response = request.send().await?;
    if !response.status().is_success() {
        bail!("{path}: {}", response.status());
    }
    let first = response.chunk().await?;
    let at = t.elapsed();
    anyhow::ensure!(
        first.is_some_and(|b| !b.is_empty()),
        "{path}: an empty body"
    );
    while response.chunk().await?.is_some() {}
    Ok(at)
}

async fn latest_cursor(c: &Client, key: &Auth, cursor: &str) -> anyhow::Result<String> {
    let mut cursor = cursor.to_owned();
    loop {
        let reply = c.get(&format!("/sync?cursor={cursor}"), key).await?.json();
        cursor = reply["next_cursor"].as_str().unwrap_or(&cursor).to_owned();
        if reply["has_more"].as_bool() != Some(true) {
            return Ok(cursor);
        }
    }
}

async fn wait_scan(
    c: &Client,
    admin: &Auth,
    library: &str,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let scans = c
            .get(&format!("/admin/libraries/{library}/scans?limit=1"), admin)
            .await?
            .json();
        let newest = scans["scans"][0].clone();
        let overview = c.get("/admin/overview", admin).await?.json();
        let scanning = overview["libraries"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|l| l["scanning"] == true);
        let state = newest["state"].as_str().unwrap_or("");
        if !state.is_empty() && state != "running" && !scanning {
            return Ok(newest);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the scan did not finish: {newest}"
        );
    }
}
