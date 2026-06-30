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
pub const DELETE_CONFIRMATION: &str = "DELETE_PROTECT_FOOTAGE_AFTER_ARCHIVE";

#[derive(Debug, Clone)]
pub struct ArchiveReport {
    pub camera_count: usize,
    pub archive_count: usize,
    pub delete_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ArchiveOptions {
    pub camera_filters: Vec<String>,
    pub range: Option<ArchiveRange>,
    pub delete_after_archive: bool,
    pub confirm_delete_after_archive: bool,
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

    let client = authenticated_client(config).await?;
    let cameras = selected_cameras(client.cameras().await?, camera_filters(config, &options))?;
    let archive_range = archive_range(config, &options);
    let delete_after_archive = should_delete_after_archive(config, &options);

    let mut archive_count = 0_usize;
    let mut delete_count = 0_usize;

    for camera in &cameras {
        let report =
            archive_camera(&client, config, camera, archive_range, delete_after_archive).await?;
        archive_count += report.archive_count;
        delete_count += report.delete_count;
    }

    Ok(ArchiveReport {
        camera_count: cameras.len(),
        archive_count,
        delete_count,
    })
}

fn validate_config(config: &Config, options: &ArchiveOptions) -> Result<()> {
    validate_archive_settings(config)?;
    validate_requested_range(options)?;
    validate_delete_settings(config, options)
}

fn validate_archive_settings(config: &Config) -> Result<()> {
    ensure_nonzero_segment(config)?;
    ensure_archive_destination(config)?;
    ensure_nas_settings(config)
}

fn ensure_nonzero_segment(config: &Config) -> Result<()> {
    if config.segment_seconds == 0 {
        bail!("segment_seconds must be greater than zero");
    }

    Ok(())
}

fn ensure_archive_destination(config: &Config) -> Result<()> {
    if config.archive_destination.trim().is_empty() {
        bail!("archive_destination must be set");
    }

    Ok(())
}

fn ensure_nas_settings(config: &Config) -> Result<()> {
    if config.archive_destination != "NAS" {
        return Ok(());
    }

    if config.archive_host.trim().is_empty() {
        bail!("archive_host must be set for NAS/UniFi Drive archives");
    }

    if config.archive_shared_drive.trim().is_empty() {
        bail!("archive_shared_drive must be set for NAS/UniFi Drive archives");
    }

    Ok(())
}

fn validate_requested_range(options: &ArchiveOptions) -> Result<()> {
    if let Some(range) = options.range {
        if range.start_ms >= range.end_ms {
            bail!("archive range start must be before end");
        }
    }

    Ok(())
}

fn validate_delete_settings(config: &Config, options: &ArchiveOptions) -> Result<()> {
    if !should_delete_after_archive(config, options) {
        return Ok(());
    }

    if !config.wait_for_archive_completion {
        bail!("delete_after_archive requires wait_for_archive_completion = true");
    }

    if !delete_after_archive_confirmed(config, options) {
        bail!(
            "delete_after_archive is destructive; pass --i-understand-this-deletes-protect-footage for run-once or set delete_after_archive_confirmation = \"{DELETE_CONFIRMATION}\" in local config"
        );
    }

    Ok(())
}

async fn authenticated_client(config: &Config) -> Result<ProtectClient> {
    let client = ProtectClient::new(&config.controller, config.verify_tls, api_key(config))?;
    if login_required(config, &client) {
        client.login(&config.credentials()?).await?;
    }

    Ok(client)
}

fn api_key(config: &Config) -> Option<String> {
    match config.auth_method {
        AuthMethod::ApiKey | AuthMethod::Auto => config.api_key(),
        AuthMethod::Password => None,
    }
}

fn login_required(config: &Config, client: &ProtectClient) -> bool {
    config.auth_method == AuthMethod::Password || !client.uses_api_key()
}

fn camera_filters<'a>(config: &'a Config, options: &'a ArchiveOptions) -> &'a [String] {
    if options.camera_filters.is_empty() {
        config.camera_ids.as_slice()
    } else {
        options.camera_filters.as_slice()
    }
}

fn archive_range(config: &Config, options: &ArchiveOptions) -> ArchiveRange {
    options
        .range
        .unwrap_or_else(|| default_archive_range(config))
}

fn default_archive_range(config: &Config) -> ArchiveRange {
    let end_ms = (OffsetDateTime::now_utc() - Duration::seconds(config.minimum_age_seconds as i64))
        .unix_timestamp()
        * 1000;
    let start_ms = end_ms.saturating_sub((config.lookback_seconds * 1000) as i64);
    ArchiveRange { start_ms, end_ms }
}

async fn archive_camera(
    client: &ProtectClient,
    config: &Config,
    camera: &Camera,
    archive_range: ArchiveRange,
    delete_after_archive: bool,
) -> Result<ArchiveReport> {
    if camera.is_connected == Some(false) {
        warn!(camera = %camera.name, "camera is disconnected; skipping");
        return Ok(ArchiveReport {
            camera_count: 1,
            archive_count: 0,
            delete_count: 0,
        });
    }

    let mut archive_count = 0_usize;
    let mut delete_count = 0_usize;
    let mut start_ms = archive_range.start_ms;
    let segment_ms = (config.segment_seconds * 1000) as i64;

    while start_ms + 1000 < archive_range.end_ms {
        let end_ms = (start_ms + segment_ms).min(archive_range.end_ms);
        archive_segment(
            client,
            config,
            camera,
            start_ms,
            end_ms,
            delete_after_archive,
        )
        .await?;
        archive_count += 1;

        if delete_after_archive {
            delete_count += 1;
        }

        start_ms = end_ms;
    }

    Ok(ArchiveReport {
        camera_count: 1,
        archive_count,
        delete_count,
    })
}

async fn archive_segment(
    client: &ProtectClient,
    config: &Config,
    camera: &Camera,
    start_ms: i64,
    end_ms: i64,
    delete_after_archive: bool,
) -> Result<()> {
    let request = archive_request(config, camera, start_ms, end_ms)?;
    let task = submit_archive(client, config, &request).await?;

    if config.wait_for_archive_completion {
        wait_for_archive_completion(client, config, &task, delete_after_archive).await?;
    }

    if delete_after_archive {
        delete_archived_segment(client, camera, start_ms, end_ms).await?;
    }

    Ok(())
}

fn should_delete_after_archive(config: &Config, options: &ArchiveOptions) -> bool {
    config.delete_after_archive || options.delete_after_archive
}

fn delete_after_archive_confirmed(config: &Config, options: &ArchiveOptions) -> bool {
    options.confirm_delete_after_archive
        || config.delete_after_archive_confirmation.trim() == DELETE_CONFIRMATION
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
    require_trackable_archive: bool,
) -> Result<()> {
    let Some(file_id) = task.file_id.as_deref() else {
        if require_trackable_archive {
            bail!("Protect archive response did not include a fileId; refusing to delete footage");
        }
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

async fn delete_archived_segment(
    client: &ProtectClient,
    camera: &Camera,
    start_ms: i64,
    end_ms: i64,
) -> Result<()> {
    info!(
        camera_id = %camera.id,
        camera = %camera.name,
        start_ms,
        end_ms,
        "deleting archived Protect footage"
    );
    client
        .delete_video_range(&camera.id, start_ms, end_ms)
        .await
        .with_context(|| format!("failed to delete archived footage for {}", camera.name))
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
        .replace([':', '.'], "-"))
}

fn datetime_from_ms(timestamp_ms: i64) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(timestamp_ms / 1000).context("failed to convert timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(id: &str, name: &str) -> Camera {
        Camera {
            id: id.to_string(),
            name: name.to_string(),
            is_connected: Some(true),
            is_recording: Some(true),
        }
    }

    fn disconnected_camera(id: &str, name: &str) -> Camera {
        Camera {
            is_connected: Some(false),
            ..camera(id, name)
        }
    }

    fn config() -> Config {
        Config::default()
    }

    fn options() -> ArchiveOptions {
        ArchiveOptions::default()
    }

    #[test]
    fn validate_config_accepts_defaults() {
        validate_config(&config(), &options()).unwrap();
    }

    #[test]
    fn validate_config_rejects_zero_segment_length() {
        let config = Config {
            segment_seconds: 0,
            ..config()
        };

        let error = validate_config(&config, &options()).unwrap_err();

        assert!(error.to_string().contains("segment_seconds"));
    }

    #[test]
    fn validate_config_requires_nas_host_and_shared_drive() {
        let missing_host = Config {
            archive_host: String::new(),
            ..config()
        };
        let missing_share = Config {
            archive_shared_drive: String::new(),
            ..config()
        };

        assert!(validate_config(&missing_host, &options())
            .unwrap_err()
            .to_string()
            .contains("archive_host"));
        assert!(validate_config(&missing_share, &options())
            .unwrap_err()
            .to_string()
            .contains("archive_shared_drive"));
    }

    #[test]
    fn validate_config_rejects_empty_archive_destination() {
        let config = Config {
            archive_destination: "  ".to_string(),
            ..config()
        };

        let error = validate_config(&config, &options()).unwrap_err();

        assert!(error.to_string().contains("archive_destination"));
    }

    #[test]
    fn validate_config_rejects_reversed_range() {
        let options = ArchiveOptions {
            range: Some(ArchiveRange {
                start_ms: 2000,
                end_ms: 1000,
            }),
            ..options()
        };

        let error = validate_config(&config(), &options).unwrap_err();

        assert!(error.to_string().contains("start must be before end"));
    }

    #[test]
    fn validate_config_requires_completion_wait_before_delete() {
        let config = Config {
            wait_for_archive_completion: false,
            ..config()
        };
        let options = ArchiveOptions {
            delete_after_archive: true,
            confirm_delete_after_archive: true,
            ..options()
        };

        let error = validate_config(&config, &options).unwrap_err();

        assert!(error.to_string().contains("wait_for_archive_completion"));
    }

    #[test]
    fn validate_config_requires_delete_confirmation() {
        let options = ArchiveOptions {
            delete_after_archive: true,
            ..options()
        };

        let error = validate_config(&config(), &options).unwrap_err();

        assert!(error.to_string().contains("destructive"));
    }

    #[test]
    fn validate_config_accepts_cli_delete_confirmation() {
        let options = ArchiveOptions {
            delete_after_archive: true,
            confirm_delete_after_archive: true,
            ..options()
        };

        validate_config(&config(), &options).unwrap();
    }

    #[test]
    fn validate_config_accepts_config_delete_confirmation() {
        let config = Config {
            delete_after_archive: true,
            delete_after_archive_confirmation: DELETE_CONFIRMATION.to_string(),
            ..config()
        };

        validate_config(&config, &options()).unwrap();
    }

    #[test]
    fn selected_cameras_returns_all_when_no_filter_is_configured() {
        let cameras = vec![camera("1", "Front"), camera("2", "Back")];

        let selected = selected_cameras(cameras, &[]).unwrap();

        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn selected_cameras_matches_by_id_or_name() {
        let cameras = vec![camera("1", "Front"), camera("2", "Back")];
        let requested = vec!["1".to_string(), "Back".to_string()];

        let selected = selected_cameras(cameras, &requested).unwrap();

        assert_eq!(
            selected
                .iter()
                .map(|camera| camera.id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2"]
        );
    }

    #[test]
    fn selected_cameras_errors_when_nothing_matches() {
        let cameras = vec![camera("1", "Front")];
        let requested = vec!["Missing".to_string()];

        let error = selected_cameras(cameras, &requested).unwrap_err();

        assert!(error.to_string().contains("none of the configured"));
    }

    #[tokio::test]
    async fn archive_camera_skips_disconnected_cameras() {
        let client =
            ProtectClient::new("https://unifi-console.example.invalid", false, None).unwrap();
        let report = archive_camera(
            &client,
            &config(),
            &disconnected_camera("1", "Offline"),
            ArchiveRange {
                start_ms: 0,
                end_ms: 60_000,
            },
            false,
        )
        .await
        .unwrap();

        assert_eq!(report.camera_count, 1);
        assert_eq!(report.archive_count, 0);
        assert_eq!(report.delete_count, 0);
    }

    #[test]
    fn archive_request_builds_provider_payload() {
        let camera = camera("camera-1", "Front/Door:Cam");

        let request = archive_request(&config(), &camera, 0, 1000).unwrap();

        assert_eq!(request.start, 0);
        assert_eq!(request.end, 1000);
        assert_eq!(request.camera_id, "camera-1");
        assert_eq!(request.destination, "NAS");
        assert_eq!(request.host, "nas.example.invalid");
        assert_eq!(request.shared_drive, "ProtectArchive");
        assert_eq!(
            request.filename,
            "Front_Door_Cam 1970-01-01T00-00-00Z - 1970-01-01T00-00-01Z.mp4"
        );
    }

    #[test]
    fn default_archive_range_uses_lookback_and_minimum_age() {
        let config = Config {
            lookback_seconds: 60,
            minimum_age_seconds: 30,
            ..config()
        };

        let before = OffsetDateTime::now_utc().unix_timestamp() * 1000;
        let range = default_archive_range(&config);
        let after = OffsetDateTime::now_utc().unix_timestamp() * 1000;

        assert_eq!(range.end_ms - range.start_ms, 60_000);
        assert!(range.end_ms <= after - 30_000);
        assert!(range.end_ms >= before - 31_000);
    }
}
