use std::collections::HashSet;

use anyhow::{anyhow, bail, Context, Result};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::{info, warn};

use crate::{
    config::AuthMethod,
    config::Config,
    protect::{ArchiveTask, ArchiveVideoRequest, Camera, ProtectClient},
};

const DEFAULT_LENS: u8 = 0;
const DEFAULT_CHANNEL: u8 = 0;

#[derive(Debug, Clone)]
pub struct ArchiveReport {
    pub camera_count: usize,
    pub archive_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ArchiveOptions {
    pub camera_filters: Vec<String>,
    pub range: Option<ArchiveRange>,
}

#[derive(Debug, Clone, Copy)]
pub struct ArchiveRange {
    pub start_ms: i64,
    pub end_ms: i64,
}

pub async fn run_once(config: &Config) -> Result<ArchiveReport> {
    run_once_with_options(config, ArchiveOptions::default()).await
}

pub async fn run_once_with_options(
    config: &Config,
    options: ArchiveOptions,
) -> Result<ArchiveReport> {
    validate_config(config, &options)?;

    let api_key = match config.auth_method {
        AuthMethod::ApiKey | AuthMethod::Auto => config.api_key(),
        AuthMethod::Password => None,
    };
    let client = ProtectClient::new(&config.controller, config.verify_tls, api_key)?;
    if config.auth_method == AuthMethod::Password || !client.uses_api_key() {
        client.login(&config.credentials()?).await?;
    }

    let camera_filters = if options.camera_filters.is_empty() {
        config.camera_ids.as_slice()
    } else {
        options.camera_filters.as_slice()
    };
    let cameras = selected_cameras(client.cameras().await?, camera_filters)?;
    let archive_range = options.range.unwrap_or_else(|| {
        let end_ms = (OffsetDateTime::now_utc()
            - Duration::seconds(config.minimum_age_seconds as i64))
        .unix_timestamp()
            * 1000;
        let start_ms = end_ms.saturating_sub((config.lookback_seconds * 1000) as i64);
        ArchiveRange { start_ms, end_ms }
    });
    let segment_ms = (config.segment_seconds * 1000) as i64;

    let mut archive_count = 0_usize;

    for camera in &cameras {
        if camera.is_connected == Some(false) {
            warn!(camera = %camera.name, "camera is disconnected; skipping");
            continue;
        }

        let mut start_ms = archive_range.start_ms;
        while start_ms + 1000 < archive_range.end_ms {
            let end_ms = (start_ms + segment_ms).min(archive_range.end_ms);
            let request = archive_request(config, camera, start_ms, end_ms)?;
            let task = submit_archive(&client, config, &request).await?;

            archive_count += 1;
            if config.wait_for_archive_completion {
                wait_for_archive_completion(&client, config, &task).await?;
            }

            start_ms = end_ms;
        }
    }

    Ok(ArchiveReport {
        camera_count: cameras.len(),
        archive_count,
    })
}

fn validate_config(config: &Config, options: &ArchiveOptions) -> Result<()> {
    if config.segment_seconds == 0 {
        bail!("segment_seconds must be greater than zero");
    }

    if config.archive_destination.trim().is_empty() {
        bail!("archive_destination must be set");
    }

    if config.archive_destination == "NAS" {
        if config.archive_host.trim().is_empty() {
            bail!("archive_host must be set for NAS/UniFi Drive archives");
        }
        if config.archive_shared_drive.trim().is_empty() {
            bail!("archive_shared_drive must be set for NAS/UniFi Drive archives");
        }
    }

    if let Some(range) = options.range {
        if range.start_ms >= range.end_ms {
            bail!("archive range start must be before end");
        }
    }

    Ok(())
}

fn selected_cameras(cameras: Vec<Camera>, requested: &[String]) -> Result<Vec<Camera>> {
    if requested.is_empty() {
        return Ok(cameras);
    }

    let requested = requested.iter().cloned().collect::<HashSet<_>>();
    let selected = cameras
        .into_iter()
        .filter(|camera| requested.contains(&camera.id) || requested.contains(&camera.name))
        .collect::<Vec<_>>();

    if selected.is_empty() {
        return Err(anyhow!(
            "none of the configured camera_ids matched Protect camera ids or names"
        ));
    }

    Ok(selected)
}

async fn submit_archive(
    client: &ProtectClient,
    config: &Config,
    request: &ArchiveVideoRequest,
) -> Result<ArchiveTask> {
    info!(
        camera_id = %request.camera_id,
        start_ms = request.start,
        end_ms = request.end,
        destination = %request.destination,
        host = %request.host,
        shared_drive = %request.shared_drive,
        "creating Protect video archive task"
    );

    let task = client.archive_video_to_provider(request).await?;
    info!(
        file_id = ?task.file_id,
        filename = ?task.filename,
        poll_for_completion = config.wait_for_archive_completion,
        "Protect video archive task created"
    );
    Ok(task)
}

async fn wait_for_archive_completion(
    client: &ProtectClient,
    config: &Config,
    task: &ArchiveTask,
) -> Result<()> {
    let Some(file_id) = task.file_id.as_deref() else {
        warn!("Protect archive response did not include a fileId; skipping completion wait");
        return Ok(());
    };

    loop {
        sleep(TokioDuration::from_secs(config.archive_status_poll_seconds)).await;
        let pending = client.pending_archives().await?;
        let Some(archive) = pending.iter().find(|archive| archive.id == file_id) else {
            info!(file_id, "archive task is no longer pending");
            return Ok(());
        };

        match archive.status.as_deref() {
            Some("failed") => bail!("archive task {file_id} failed"),
            Some("canceled") | Some("canceledPending") => {
                bail!("archive task {file_id} was canceled")
            }
            status => info!(
                file_id,
                status = ?status,
                filename = ?archive.filename,
                "archive task still pending"
            ),
        }
    }
}

fn archive_request(
    config: &Config,
    camera: &Camera,
    start_ms: i64,
    end_ms: i64,
) -> Result<ArchiveVideoRequest> {
    Ok(ArchiveVideoRequest {
        start: start_ms,
        end: end_ms,
        filename: archive_filename(camera, start_ms, end_ms)?,
        lens: DEFAULT_LENS,
        destination: config.archive_destination.clone(),
        camera_id: camera.id.clone(),
        archive_type: "rotating".to_string(),
        channel: DEFAULT_CHANNEL,
        fps: None,
        host: config.archive_host.clone(),
        shared_drive: config.archive_shared_drive.clone(),
    })
}

fn archive_filename(camera: &Camera, start_ms: i64, end_ms: i64) -> Result<String> {
    let safe_camera_name = camera
        .name
        .replace(['<', '>', ':', '"', '/', '\\', '|', '?', '*'], "_");
    Ok(format!(
        "{} {} - {}.mp4",
        safe_camera_name,
        file_timestamp(start_ms)?,
        file_timestamp(end_ms)?
    ))
}

fn file_timestamp(timestamp_ms: i64) -> Result<String> {
    Ok(datetime_from_ms(timestamp_ms)?
        .format(&Rfc3339)
        .context("failed to format timestamp")?
        .replace(':', "-")
        .replace('.', "-"))
}

fn datetime_from_ms(timestamp_ms: i64) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(timestamp_ms / 1000).context("failed to convert timestamp")
}
