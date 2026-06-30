mod archiver;
mod config;
mod protect;

use std::{fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::time::{sleep, Duration};
use tracing::info;

use crate::{
    archiver::{
        archive_events_with_options, run_once, run_once_with_options, ArchiveOptions, ArchiveRange,
        EventArchiveOptions,
    },
    config::{AuthMethod, Config},
    protect::ProtectClient,
};

#[derive(Debug, Parser)]
#[command(version, about = "Create UniFi Protect video archive tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write a sample TOML configuration file.
    InitConfig {
        #[arg(default_value = "protect-archive.toml")]
        path: PathBuf,

        #[arg(long)]
        force: bool,
    },

    /// Authenticate and list visible Protect cameras.
    Cameras {
        #[arg(short, long, default_value = "protect-archive.toml")]
        config: PathBuf,
    },

    /// Create Protect archive tasks for a time range, then exit.
    RunOnce {
        #[arg(short, long, default_value = "protect-archive.toml")]
        config: PathBuf,

        /// Camera ID or camera name to archive. Repeat for multiple cameras.
        #[arg(long = "camera")]
        cameras: Vec<String>,

        /// Range start as RFC 3339, for example 2026-06-30T09:00:00Z.
        #[arg(long)]
        start: Option<String>,

        /// Range end as RFC 3339, for example 2026-06-30T10:00:00+01:00.
        #[arg(long)]
        end: Option<String>,

        /// Delete each Protect video range after its archive task is no longer pending.
        #[arg(long)]
        delete_after_archive: bool,

        /// Required with destructive delete flags because they permanently remove Protect footage.
        #[arg(long)]
        i_understand_this_deletes_protect_footage: bool,
    },

    /// Archive only clips around Protect events/detections for a time range.
    ArchiveEvents {
        #[arg(short, long, default_value = "protect-archive.toml")]
        config: PathBuf,

        /// Camera ID or camera name to archive. Repeat for multiple cameras.
        #[arg(long = "camera")]
        cameras: Vec<String>,

        /// Range start as RFC 3339, for example 2026-06-30T09:00:00Z.
        #[arg(long)]
        start: String,

        /// Range end as RFC 3339, for example 2026-06-30T10:00:00+01:00.
        #[arg(long)]
        end: String,

        /// Protect event type to include, such as motion or smartDetectZone.
        #[arg(long = "type")]
        event_types: Vec<String>,

        /// Smart detection type to include, such as person, vehicle, animal, or package.
        #[arg(long = "smart-detect-type")]
        smart_detect_types: Vec<String>,

        /// Seconds to include before each event.
        #[arg(long, default_value_t = 15)]
        pre_roll_seconds: u64,

        /// Seconds to include after each event.
        #[arg(long, default_value_t = 45)]
        post_roll_seconds: u64,

        /// Merge event clips for the same camera when they are within this many seconds.
        #[arg(long, default_value_t = 60)]
        merge_gap_seconds: u64,

        /// Delete each archived event clip range after its archive task is no longer pending.
        #[arg(long)]
        delete_after_archive: bool,

        /// After all event clips archive successfully, delete the full selected source range.
        #[arg(long)]
        delete_source_range_after_archive: bool,

        /// Required with destructive delete flags because they permanently remove Protect footage.
        #[arg(long)]
        i_understand_this_deletes_protect_footage: bool,
    },

    /// Keep archiving in a polling loop.
    Daemon {
        #[arg(short, long, default_value = "protect-archive.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::InitConfig { path, force } => init_config(path, force),
        Command::Cameras { config } => list_cameras(config).await,
        Command::RunOnce {
            config,
            cameras,
            start,
            end,
            delete_after_archive,
            i_understand_this_deletes_protect_footage,
        } => {
            let config = Config::load(&config)?;
            let options = ArchiveOptions {
                camera_filters: cameras,
                range: parse_range(start, end)?,
                delete_after_archive,
                confirm_delete_after_archive: i_understand_this_deletes_protect_footage,
            };
            let report = run_once_with_options(&config, options).await?;
            info!(
                cameras = report.camera_count,
                archives = report.archive_count,
                deletes = report.delete_count,
                "archive pass complete"
            );
            Ok(())
        }
        Command::ArchiveEvents {
            config,
            cameras,
            start,
            end,
            event_types,
            smart_detect_types,
            pre_roll_seconds,
            post_roll_seconds,
            merge_gap_seconds,
            delete_after_archive,
            delete_source_range_after_archive,
            i_understand_this_deletes_protect_footage,
        } => {
            let config = Config::load(&config)?;
            let report = archive_events_with_options(
                &config,
                EventArchiveOptions {
                    camera_filters: cameras,
                    range: parse_required_range(start, end)?,
                    event_types,
                    smart_detect_types,
                    pre_roll_seconds,
                    post_roll_seconds,
                    merge_gap_seconds,
                    delete_after_archive,
                    delete_source_range_after_archive,
                    confirm_delete_after_archive: i_understand_this_deletes_protect_footage,
                },
            )
            .await?;
            info!(
                cameras = report.camera_count,
                archives = report.archive_count,
                deletes = report.delete_count,
                "event archive pass complete"
            );
            Ok(())
        }
        Command::Daemon { config } => daemon(config).await,
    }
}

fn init_config(path: PathBuf, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }

    let sample = Config::sample()?;
    fs::write(&path, sample).with_context(|| format!("failed to write {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

async fn list_cameras(path: PathBuf) -> Result<()> {
    let config = Config::load(&path)?;
    let api_key = match config.auth_method {
        AuthMethod::ApiKey | AuthMethod::Auto => config.api_key(),
        AuthMethod::Password => None,
    };
    let client = ProtectClient::new(&config.controller, config.verify_tls, api_key)?;
    if config.auth_method == AuthMethod::Password || !client.uses_api_key() {
        client.login(&config.credentials()?).await?;
    }

    for camera in client.cameras().await? {
        println!(
            "{}\t{}\tconnected={}\trecording={}",
            camera.id,
            camera.name,
            camera
                .is_connected
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            camera
                .is_recording
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
    }

    Ok(())
}

fn parse_range(start: Option<String>, end: Option<String>) -> Result<Option<ArchiveRange>> {
    match (start, end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) => {
            let start = parse_timestamp_ms(&start, "start")?;
            let end = parse_timestamp_ms(&end, "end")?;
            Ok(Some(ArchiveRange {
                start_ms: start,
                end_ms: end,
            }))
        }
        _ => bail!("--start and --end must be provided together"),
    }
}

fn parse_required_range(start: String, end: String) -> Result<ArchiveRange> {
    Ok(ArchiveRange {
        start_ms: parse_timestamp_ms(&start, "start")?,
        end_ms: parse_timestamp_ms(&end, "end")?,
    })
}

fn parse_timestamp_ms(value: &str, label: &str) -> Result<i64> {
    let timestamp = OffsetDateTime::parse(value, &Rfc3339)
        .with_context(|| format!("failed to parse --{label}; use RFC 3339 with a timezone"))?;
    Ok(timestamp.unix_timestamp() * 1000 + i64::from(timestamp.millisecond()))
}

async fn daemon(path: PathBuf) -> Result<()> {
    loop {
        let config = Config::load(&path)?;
        match run_once(&config).await {
            Ok(report) => info!(
                cameras = report.camera_count,
                archives = report.archive_count,
                deletes = report.delete_count,
                "archive pass complete"
            ),
            Err(error) => tracing::error!(?error, "archive pass failed"),
        }

        tokio::select! {
            _ = sleep(Duration::from_secs(Config::load(&path)?.poll_seconds)) => {}
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown requested");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_range_requires_both_bounds() {
        let error = parse_range(Some("2026-06-30T09:00:00Z".to_string()), None).unwrap_err();
        assert!(error.to_string().contains("--start and --end"));
    }

    #[test]
    fn parse_range_accepts_rfc3339_with_timezone() {
        let range = parse_range(
            Some("2026-06-30T09:00:00Z".to_string()),
            Some("2026-06-30T10:00:00+01:00".to_string()),
        )
        .unwrap()
        .unwrap();

        assert_eq!(range.start_ms, range.end_ms);
    }

    #[test]
    fn parse_required_range_accepts_bounds() {
        let range = parse_required_range(
            "2026-06-30T09:00:00Z".to_string(),
            "2026-06-30T09:01:00Z".to_string(),
        )
        .unwrap();

        assert_eq!(range.end_ms - range.start_ms, 60_000);
    }
}
