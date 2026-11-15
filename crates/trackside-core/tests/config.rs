//! Configuration loading: defaults, file, environment, overrides, secrets and `config show`.

use std::path::PathBuf;
use std::time::Duration;

use trackside_core::config::{
    ConfigError, ConfigSource, ConfigSources, KEYS, REDACTED, ValueKind, load_config, locked_keys,
    render_show, show,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut all = vec![(
        String::from("TRACKSIDE_DATA_DIR"),
        String::from("/srv/trackside"),
    )];
    all.extend(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    all
}

fn sample(kind: ValueKind, key: &str) -> &'static str {
    match (kind, key) {
        (_, "tls.mode") => "self-signed",
        (_, "log.format") => "json",
        (_, "backup.schedule") => "weekly sun 04:00",
        (_, "metrics.listen") => "127.0.0.1:9090",
        (_, "server.listen") => "0.0.0.0:4747,[::1]:4748",
        (_, "server.trusted_proxies") => "10.0.0.0/8,fd00::/8",
        (_, "auth.login_rate" | "acquire.ytdlp.search_rate") => "5/30s",
        (_, "scanner.quick_interval" | "scanner.full_interval") => "off",
        (_, k)
            if k.ends_with("timeout")
                || k.ends_with("grace")
                || k.ends_with("debounce")
                || k.ends_with("settle")
                || k.ends_with("segment")
                || k.ends_with("idle")
                || k.ends_with("max") && k.starts_with("auth.")
                || k.ends_with("ttl")
                || k.ends_with("retention") =>
        {
            "45s"
        }
        (_, k)
            if k.ends_with("cache_max")
                || k.ends_with("min_free")
                || k.ends_with("max_file")
                || k.ends_with("max_file_bytes")
                || k.ends_with("memory")
                || k == "database.cache"
                || k == "database.mmap" =>
        {
            "64MiB"
        }
        (_, "scanner.empty_guard") => "0.25",
        (ValueKind::Bool, _) => "true",
        (ValueKind::Integer, _) => "3",
        (ValueKind::Float, _) => "0.5",
        (ValueKind::List | ValueKind::CommaList, _) => "a,b",
        (ValueKind::Text, _) => "/tmp/x",
    }
}

#[test]
fn every_key_has_a_default_and_an_env_name_that_parses() -> TestResult {
    let (defaults, report) = load_config(&ConfigSources {
        env: env(&[]),
        ..ConfigSources::default()
    })?;
    assert_eq!(defaults.data_dir, PathBuf::from("/srv/trackside"));
    assert_eq!(
        defaults.database.path,
        PathBuf::from("/srv/trackside/trackside.db")
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let mut pairs: Vec<(String, String)> = Vec::new();
    for info in KEYS {
        assert!(info.env.starts_with("TRACKSIDE_"), "{}", info.key);
        if info.key == "data_dir" {
            continue;
        }
        let value = if info.key == "tls.mode" {
            "off"
        } else {
            sample(info.kind, info.key)
        };
        pairs.push((info.env.to_owned(), value.to_owned()));
    }
    let (config, report) = load_config(&ConfigSources {
        env: env(&[]).into_iter().chain(pairs).collect(),
        ..ConfigSources::default()
    })?;
    for info in KEYS {
        assert_eq!(report.source(info.key), ConfigSource::Env, "{}", info.key);
    }
    assert_eq!(config.server.listen.len(), 2);
    assert_eq!(config.scanner.watch_debounce, Duration::from_secs(45));
    assert_eq!(config.scanner.quick_interval, None);
    assert_eq!(config.database.cache, 64 << 20);
    assert_eq!(config.backup.keep, 3);
    assert_eq!(config.acquire.prefer, ["a", "b"]);
    assert_eq!(
        config
            .auth
            .admin_password
            .as_ref()
            .map(trackside_core::config::Secret::expose),
        Some("/tmp/x")
    );
    Ok(())
}

#[test]
fn secrets_also_come_from_files() -> TestResult {
    let dir = tempfile::tempdir()?;
    let secrets = KEYS.iter().filter(|k| k.secret).collect::<Vec<_>>();
    assert_eq!(secrets.len(), 3);
    let mut pairs = vec![(
        String::from("TRACKSIDE_ADMIN_USERNAME"),
        String::from("admin"),
    )];
    for info in &secrets {
        let path = dir.path().join(info.key);
        std::fs::write(&path, format!("from-file-{}\n", info.key))?;
        pairs.push((
            format!("{}_FILE", info.env),
            path.to_string_lossy().into_owned(),
        ));
    }
    let refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let (config, report) = load_config(&ConfigSources {
        env: env(&refs),
        ..ConfigSources::default()
    })?;
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(
        config
            .auth
            .admin_password
            .as_ref()
            .map(trackside_core::config::Secret::expose),
        Some("from-file-auth.admin_password")
    );
    assert_eq!(
        config
            .acquire
            .soulseek
            .api_key
            .as_ref()
            .map(trackside_core::config::Secret::expose),
        Some("from-file-acquire.soulseek.api_key")
    );
    assert_eq!(
        config
            .enrichment
            .lastfm_api_key
            .as_ref()
            .map(trackside_core::config::Secret::expose),
        Some("from-file-enrichment.lastfm_api_key")
    );
    let missing = load_config(&ConfigSources {
        env: env(&[("TRACKSIDE_ADMIN_PASSWORD_FILE", "/nonexistent/secret")]),
        ..ConfigSources::default()
    });
    assert!(matches!(missing, Err(ConfigError::Io { .. })));
    Ok(())
}

#[test]
fn file_env_and_overrides_layer_in_order() -> TestResult {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("config.toml");
    std::fs::write(
        &file,
        "data_dir = \"/data\"\n[server]\nname = \"From file\"\nbase_path = \"music/\"\n[scanner]\nwatch = false\nmissing_grace = \"7d\"\nbogus = 1\n",
    )?;
    let sources = ConfigSources {
        file: Some(file),
        env: vec![
            (
                String::from("TRACKSIDE_SERVER__NAME"),
                String::from("From env"),
            ),
            (String::from("TRACKSIDE_NOPE"), String::from("1")),
            (String::from("RUST_LOG"), String::from("debug")),
        ],
        overrides: vec![(String::from("scanner.missing_grace"), String::from("1d"))],
    };
    let (config, report) = load_config(&sources)?;
    assert_eq!(config.data_dir, PathBuf::from("/data"));
    assert_eq!(config.cache_dir, PathBuf::from("/data/cache"));
    assert_eq!(config.server.name, "From env");
    assert_eq!(config.server.base_path, "/music");
    assert!(!config.scanner.watch);
    assert_eq!(config.scanner.missing_grace, Duration::from_secs(86_400));
    assert_eq!(config.log.level, "debug");
    assert_eq!(report.source("scanner.watch"), ConfigSource::File);
    assert_eq!(
        report.source("scanner.missing_grace"),
        ConfigSource::Override
    );
    assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    let locked = locked_keys(&report);
    assert!(locked.contains("server.name") && locked.contains("scanner.watch"));
    assert!(!locked.contains("scanner.missing_grace"));
    Ok(())
}

#[test]
fn bad_values_name_their_key() {
    let bad = load_config(&ConfigSources {
        env: env(&[("TRACKSIDE_SCANNER__WATCH", "maybe")]),
        ..ConfigSources::default()
    });
    assert!(matches!(bad, Err(ConfigError::Invalid { key, .. }) if key == "scanner.watch"));
    let bad = load_config(&ConfigSources {
        env: env(&[("TRACKSIDE_SYNC__MAX_PAGE", "90000")]),
        ..ConfigSources::default()
    });
    assert!(matches!(bad, Err(ConfigError::Invalid { key, .. }) if key == "sync.max_page"));
    let bad = load_config(&ConfigSources {
        env: env(&[("TRACKSIDE_TLS__MODE", "files")]),
        ..ConfigSources::default()
    });
    assert!(matches!(bad, Err(ConfigError::Invalid { key, .. }) if key == "tls.mode"));
    let bad = load_config(&ConfigSources {
        env: env(&[]),
        overrides: vec![(String::from("nope"), String::from("1"))],
        ..ConfigSources::default()
    });
    assert!(matches!(bad, Err(ConfigError::Invalid { .. })));
}

#[test]
fn config_show_redacts_secrets() -> TestResult {
    let (config, report) = load_config(&ConfigSources {
        env: env(&[
            ("TRACKSIDE_ADMIN_USERNAME", "admin"),
            ("TRACKSIDE_ADMIN_PASSWORD", "hunter2"),
            ("TRACKSIDE_ENRICHMENT__LASTFM_API_KEY", "lastfm-secret"),
        ]),
        ..ConfigSources::default()
    })?;
    let text = render_show(&config, &report);
    assert!(
        !text.contains("hunter2") && !text.contains("lastfm-secret"),
        "{text}"
    );
    assert!(text.contains(&format!("auth.admin_password = {REDACTED}")) || text.contains(REDACTED));
    assert!(text.contains("auth.admin_username = \"admin\"  # environment"));
    let lines = show(&config, &report);
    assert_eq!(lines.len(), KEYS.len());
    let unset = lines
        .iter()
        .find(|l| l.key == "acquire.soulseek.api_key")
        .map(|l| l.value.as_str());
    assert_eq!(unset, Some("unset"));
    assert!(!format!("{config:?}").contains("hunter2"));
    Ok(())
}
