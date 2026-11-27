//! Configuration loading: defaults, file, environment, overrides, secrets and `config show`.

use std::path::PathBuf;
use std::time::Duration;

use trackside_core::config::{
    ConfigError, ConfigSource, ConfigSources, KEYS, REDACTED, RETIRED_KEYS, ValueKind, load_config,
    locked_keys, render_show, show,
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
        (_, "server.local_networks") => "2001:db8:1::/64",
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
                || k.ends_with("max_bytes")
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
    assert_eq!(secrets.len(), 2);
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
    assert_eq!(config.scanner.missing_grace, Duration::from_hours(24));
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
        .find(|l| l.key == "tls.cert_file")
        .map(|l| l.value.as_str());
    assert_eq!(unset, Some("unset"));
    assert!(!format!("{config:?}").contains("hunter2"));
    Ok(())
}

fn file_sources(text: &str) -> Result<(tempfile::TempDir, ConfigSources), std::io::Error> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("config.toml");
    std::fs::write(&file, text)?;
    Ok((
        dir,
        ConfigSources {
            file: Some(file),
            env: env(&[]),
            ..ConfigSources::default()
        },
    ))
}

#[test]
fn retired_keys_are_not_keys() {
    for retired in RETIRED_KEYS {
        assert!(
            KEYS.iter()
                .all(|k| k.key != retired.key && k.env != retired.env),
            "{} is both current and retired",
            retired.key
        );
    }
}

#[test]
fn retired_keys_in_the_file_are_ignored_with_one_warning_each() -> TestResult {
    let (_dir, sources) = file_sources(
        "[acquire.soulseek]\nenabled = true\nurl = \"http://slskd:5030\"\napi_key = \"secret\"\n",
    )?;
    let (_, report) = load_config(&sources)?;
    assert_eq!(report.warnings.len(), 3, "{:?}", report.warnings);
    for key in [
        "acquire.soulseek.enabled",
        "acquire.soulseek.url",
        "acquire.soulseek.api_key",
    ] {
        let named: Vec<_> = report
            .warnings
            .iter()
            .filter(|w| w.starts_with(&format!("{key} is deprecated and ignored")))
            .collect();
        assert_eq!(named.len(), 1, "{key}: {:?}", report.warnings);
    }
    assert!(!report.warnings.iter().any(|w| w.contains("secret")));
    Ok(())
}

#[test]
fn a_retired_key_set_in_the_file_and_the_environment_warns_once() -> TestResult {
    let (_dir, mut sources) = file_sources("[acquire.soulseek]\nurl = \"http://slskd:5030\"\n")?;
    sources.env.push((
        String::from("TRACKSIDE_ACQUIRE__SOULSEEK__URL"),
        String::from("http://other:5030"),
    ));
    let (_, report) = load_config(&sources)?;
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("TRACKSIDE_ACQUIRE__SOULSEEK__URL"));
    assert!(report.warnings[0].contains("the configuration file"));
    Ok(())
}

#[test]
fn retired_environment_variables_and_secret_files_are_ignored_with_a_warning() -> TestResult {
    let (_, report) = load_config(&ConfigSources {
        env: env(&[
            ("TRACKSIDE_ACQUIRE__SOULSEEK__ENABLED", "true"),
            (
                "TRACKSIDE_ACQUIRE__SOULSEEK__API_KEY_FILE",
                "/nonexistent/key",
            ),
        ]),
        ..ConfigSources::default()
    })?;
    assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    assert!(
        report
            .warnings
            .iter()
            .all(|w| w.contains("is deprecated and ignored") && !w.contains("unknown")),
        "{:?}",
        report.warnings
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.starts_with("acquire.soulseek.api_key "))
    );
    Ok(())
}

#[test]
fn a_retired_key_on_the_command_line_is_ignored_with_a_warning() -> TestResult {
    let (_, report) = load_config(&ConfigSources {
        env: env(&[]),
        overrides: vec![(
            String::from("acquire.soulseek.concurrency"),
            String::from("3"),
        )],
        ..ConfigSources::default()
    })?;
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].starts_with("acquire.soulseek.concurrency is deprecated"));
    Ok(())
}

#[test]
fn the_old_min_bitrate_sets_the_new_key_when_it_is_unset() -> TestResult {
    let (_dir, sources) = file_sources("[acquire.soulseek]\nmin_bitrate = 192\n")?;
    let (config, report) = load_config(&sources)?;
    assert_eq!(config.acquire.min_bitrate, 192);
    assert_eq!(report.source("acquire.min_bitrate"), ConfigSource::File);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("sets acquire.min_bitrate"));
    Ok(())
}

#[test]
fn the_old_min_bitrate_from_the_environment_sets_the_new_key() -> TestResult {
    let (config, report) = load_config(&ConfigSources {
        env: env(&[("TRACKSIDE_ACQUIRE__SOULSEEK__MIN_BITRATE", "160")]),
        ..ConfigSources::default()
    })?;
    assert_eq!(config.acquire.min_bitrate, 160);
    assert_eq!(report.source("acquire.min_bitrate"), ConfigSource::Env);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    Ok(())
}

#[test]
fn the_new_min_bitrate_wins_over_the_old_one() -> TestResult {
    let (_dir, mut sources) = file_sources("[acquire.soulseek]\nmin_bitrate = 192\n")?;
    sources.env.push((
        String::from("TRACKSIDE_ACQUIRE__MIN_BITRATE"),
        String::from("320"),
    ));
    let (config, report) = load_config(&sources)?;
    assert_eq!(config.acquire.min_bitrate, 320);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("ignored because acquire.min_bitrate is set"));
    Ok(())
}

#[test]
fn the_old_min_bitrate_must_still_be_a_number() {
    let bad = load_config(&ConfigSources {
        env: env(&[("TRACKSIDE_ACQUIRE__SOULSEEK__MIN_BITRATE", "high")]),
        ..ConfigSources::default()
    });
    assert!(
        matches!(bad, Err(ConfigError::Invalid { key, .. }) if key == "acquire.soulseek.min_bitrate")
    );
}

#[test]
fn min_bitrate_defaults_to_256() -> TestResult {
    let (config, _) = load_config(&ConfigSources {
        env: env(&[]),
        ..ConfigSources::default()
    })?;
    assert_eq!(config.acquire.min_bitrate, 256);
    assert_eq!(config.acquire.prefer, ["ytdlp"]);
    Ok(())
}
