use std::collections::HashSet;

use anyhow::{anyhow, bail, Context, Result};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
use tokio::time::{sleep, Duration as TokioDuration};
use tracing::{info, warn};

use crate::{
    config::AuthMethod,
    config::Config,
    protect::{ArchiveTask, ArchiveVideoRequest, Camera, EventQuery, ProtectClient, ProtectEvent},
};

const DEFAULT_LENS: u8 = 0;
const DEFAULT_CHANNEL: u8 = 0;
const EVENTS_PAGE_LIMIT: usize = 500;
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

#[derive(Debug, Clone)]
pub struct EventArchiveOptions {
    pub camera_filters: Vec<String>,
    pub range: ArchiveRange,
    pub event_types: Vec<String>,
    pub smart_detect_types: Vec<String>,
    pub pre_roll_seconds: u64,
    pub post_roll_seconds: u64,
    pub merge_gap_seconds: u64,
    pub delete_after_archive: bool,
    pub delete_source_range_after_archive: bool,
    pub confirm_delete_after_archive: bool,
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

pub async fn archive_events_with_options(
    config: &Config,
    options: EventArchiveOptions,
) -> Result<ArchiveReport> {
    validate_event_archive(config, &options)?;

    let client = authenticated_client(config).await?;
    let cameras = selected_cameras(
        client.cameras().await?,
        event_camera_filters(config, &options),
    )?;
    let camera_ids = cameras
        .iter()
        .map(|camera| camera.id.clone())
        .collect::<Vec<_>>();
    let events = fetch_events(&client, &options, camera_ids).await?;
    let clips = event_clip_windows(&events, options.range, &options);
    let delete_after_archive = options.delete_after_archive;
    let delete_source_range_after_archive = options.delete_source_range_after_archive;

    info!(
        cameras = cameras.len(),
        events = events.len(),
        clips = clips.len(),
        "planned event archive clips"
    );

    let mut archive_count = 0_usize;
    let mut delete_count = 0_usize;
    let total_clips = clips.len();

    for (index, clip) in clips.iter().enumerate() {
        let Some(camera) = cameras.iter().find(|camera| camera.id == clip.camera_id) else {
            warn!(
                camera_id = %clip.camera_id,
                "event clip references a camera that is not selected; skipping"
            );
            continue;
        };

        info!(
            clip = index + 1,
            total_clips,
            camera = %camera.name,
            start_ms = clip.start_ms,
            end_ms = clip.end_ms,
            delete_clip_after_archive = delete_after_archive,
            "archiving event clip"
        );
        archive_segment(
            &client,
            config,
            camera,
            clip.start_ms,
            clip.end_ms,
            delete_after_archive,
        )
        .await?;
        archive_count += 1;

        if delete_after_archive {
            delete_count += 1;
        }

        if delete_source_range_after_archive {
            let delete_range = source_delete_range_for_clip(options.range, clip);
            delete_source_range_until(&client, camera, delete_range.start_ms, delete_range.end_ms)
                .await?;
            delete_count += 1;
        }

        info!(
            clip = index + 1,
            total_clips,
            archives = archive_count,
            deletes = delete_count,
            "event clip archived"
        );
    }

    if delete_source_range_after_archive && archive_count == 0 {
        bail!(
            "refusing to delete source footage because no event clips were archived; check event filters first"
        );
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

fn validate_event_archive(config: &Config, options: &EventArchiveOptions) -> Result<()> {
    validate_archive_settings(config)?;
    validate_range(options.range)?;
    validate_event_delete_mode(options)?;
    validate_delete_request(
        config,
        options.delete_after_archive || options.delete_source_range_after_archive,
        options.confirm_delete_after_archive,
    )
}

fn validate_event_delete_mode(options: &EventArchiveOptions) -> Result<()> {
    if options.delete_after_archive && options.delete_source_range_after_archive {
        bail!(
            "--delete-after-archive and --delete-source-range-after-archive cannot be used together"
        );
    }

    Ok(())
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
        validate_range(range)?;
    }

    Ok(())
}

fn validate_range(range: ArchiveRange) -> Result<()> {
    if range.start_ms >= range.end_ms {
        bail!("archive range start must be before end");
    }

    Ok(())
}

fn validate_delete_settings(config: &Config, options: &ArchiveOptions) -> Result<()> {
    validate_delete_request(
        config,
        should_delete_after_archive(config, options),
        delete_after_archive_confirmed(config, options),
    )
}

fn validate_delete_request(
    config: &Config,
    delete_after_archive: bool,
    delete_after_archive_confirmed: bool,
) -> Result<()> {
    if !delete_after_archive {
        return Ok(());
    }

    if !config.wait_for_archive_completion {
        bail!("delete_after_archive requires wait_for_archive_completion = true");
    }

    if !delete_after_archive_confirmed {
        bail!(
            "Protect footage deletion is destructive; pass --i-understand-this-deletes-protect-footage or set delete_after_archive_confirmation = \"{DELETE_CONFIRMATION}\" in local config"
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

fn event_camera_filters<'a>(config: &'a Config, options: &'a EventArchiveOptions) -> &'a [String] {
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
    let total_segments = segment_count(archive_range, segment_ms);

    while start_ms + 1000 < archive_range.end_ms {
        let end_ms = (start_ms + segment_ms).min(archive_range.end_ms);
        info!(
            camera = %camera.name,
            segment = archive_count + 1,
            total_segments,
            start_ms,
            end_ms,
            delete_after_archive,
            "archiving camera segment"
        );
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

        info!(
            camera = %camera.name,
            segment = archive_count,
            total_segments,
            archives = archive_count,
            deletes = delete_count,
            "camera segment archived"
        );

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

fn segment_count(archive_range: ArchiveRange, segment_ms: i64) -> usize {
    let duration_ms = archive_range.end_ms.saturating_sub(archive_range.start_ms);
    if segment_ms <= 0 || duration_ms <= 1000 {
        return 0;
    }

    ((duration_ms + segment_ms - 1) / segment_ms) as usize
}

async fn fetch_events(
    client: &ProtectClient,
    options: &EventArchiveOptions,
    camera_ids: Vec<String>,
) -> Result<Vec<ProtectEvent>> {
    let mut all_events = Vec::new();
    let mut offset = 0_usize;
    let mut page_number = 1_usize;

    info!(
        cameras = camera_ids.len(),
        start_ms = options.range.start_ms,
        end_ms = options.range.end_ms,
        event_types = ?options.event_types,
        smart_detect_types = ?options.smart_detect_types,
        page_limit = EVENTS_PAGE_LIMIT,
        "fetching Protect events"
    );

    loop {
        let page = client
            .events(&EventQuery {
                start_ms: options.range.start_ms,
                end_ms: options.range.end_ms,
                camera_ids: camera_ids.clone(),
                event_types: options.event_types.clone(),
                smart_detect_types: options.smart_detect_types.clone(),
                limit: EVENTS_PAGE_LIMIT,
                offset,
            })
            .await?;

        let page_len = page.len();
        all_events.extend(page);
        info!(
            page = page_number,
            offset,
            events = page_len,
            total_events = all_events.len(),
            "fetched Protect events page"
        );

        if page_len < EVENTS_PAGE_LIMIT {
            info!(
                events = all_events.len(),
                "finished fetching Protect events"
            );
            return Ok(all_events);
        }

        offset += page_len;
        page_number += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipWindow {
    camera_id: String,
    start_ms: i64,
    end_ms: i64,
}

fn event_clip_windows(
    events: &[ProtectEvent],
    archive_range: ArchiveRange,
    options: &EventArchiveOptions,
) -> Vec<ClipWindow> {
    let mut clips = events
        .iter()
        .filter_map(|event| event_clip_window(event, archive_range, options))
        .collect::<Vec<_>>();

    clips.sort_by(|left, right| {
        left.camera_id
            .cmp(&right.camera_id)
            .then(left.start_ms.cmp(&right.start_ms))
            .then(left.end_ms.cmp(&right.end_ms))
    });
    merge_clip_windows(clips, (options.merge_gap_seconds * 1000) as i64)
}

fn event_clip_window(
    event: &ProtectEvent,
    archive_range: ArchiveRange,
    options: &EventArchiveOptions,
) -> Option<ClipWindow> {
    if !event_matches_filters(event, options) || event.camera_id.is_empty() {
        return None;
    }

    let event_end_ms = event
        .end
        .unwrap_or(event.start + 1000)
        .max(event.start + 1000);
    let start_ms = event
        .start
        .saturating_sub((options.pre_roll_seconds * 1000) as i64)
        .max(archive_range.start_ms);
    let end_ms = event_end_ms
        .saturating_add((options.post_roll_seconds * 1000) as i64)
        .min(archive_range.end_ms);

    (start_ms < end_ms).then(|| ClipWindow {
        camera_id: event.camera_id.clone(),
        start_ms,
        end_ms,
    })
}

fn source_delete_range_for_clip(archive_range: ArchiveRange, clip: &ClipWindow) -> ArchiveRange {
    ArchiveRange {
        start_ms: archive_range.start_ms,
        end_ms: clip.end_ms,
    }
}

fn event_matches_filters(event: &ProtectEvent, options: &EventArchiveOptions) -> bool {
    event_type_matches(event, &options.event_types)
        && smart_detect_type_matches(event, &options.smart_detect_types)
}

fn event_type_matches(event: &ProtectEvent, event_types: &[String]) -> bool {
    event_types.is_empty()
        || event
            .event_type
            .as_ref()
            .is_some_and(|event_type| event_types.contains(event_type))
}

fn smart_detect_type_matches(event: &ProtectEvent, smart_detect_types: &[String]) -> bool {
    smart_detect_types.is_empty()
        || event
            .smart_detect_types
            .iter()
            .any(|event_type| smart_detect_types.contains(event_type))
}

fn merge_clip_windows(clips: Vec<ClipWindow>, merge_gap_ms: i64) -> Vec<ClipWindow> {
    let mut merged = Vec::<ClipWindow>::new();

    for clip in clips {
        let Some(last) = merged.last_mut() else {
            merged.push(clip);
            continue;
        };

        if last.camera_id == clip.camera_id && clip.start_ms <= last.end_ms + merge_gap_ms {
            last.end_ms = last.end_ms.max(clip.end_ms);
        } else {
            merged.push(clip);
        }
    }

    merged
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

async fn delete_source_range_until(
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
        "deleting Protect source footage through archived clip end"
    );
    client
        .delete_video_range(&camera.id, start_ms, end_ms)
        .await
        .with_context(|| format!("failed to delete source footage for {}", camera.name))
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

    fn event_options() -> EventArchiveOptions {
        EventArchiveOptions {
            camera_filters: Vec::new(),
            range: ArchiveRange {
                start_ms: 0,
                end_ms: 120_000,
            },
            event_types: Vec::new(),
            smart_detect_types: Vec::new(),
            pre_roll_seconds: 10,
            post_roll_seconds: 20,
            merge_gap_seconds: 5,
            delete_after_archive: false,
            delete_source_range_after_archive: false,
            confirm_delete_after_archive: false,
        }
    }

    fn event(camera_id: &str, start_ms: i64, end_ms: Option<i64>) -> ProtectEvent {
        ProtectEvent {
            camera_id: camera_id.to_string(),
            start: start_ms,
            end: end_ms,
            event_type: Some("smartDetectZone".to_string()),
            smart_detect_types: vec!["person".to_string()],
        }
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
    fn validate_event_archive_requires_delete_confirmation() {
        let options = EventArchiveOptions {
            delete_after_archive: true,
            ..event_options()
        };

        let error = validate_event_archive(&config(), &options).unwrap_err();

        assert!(error.to_string().contains("destructive"));
    }

    #[test]
    fn validate_event_archive_accepts_confirmed_delete() {
        let options = EventArchiveOptions {
            delete_after_archive: true,
            confirm_delete_after_archive: true,
            ..event_options()
        };

        validate_event_archive(&config(), &options).unwrap();
    }

    #[test]
    fn validate_event_archive_accepts_confirmed_source_range_delete() {
        let options = EventArchiveOptions {
            delete_source_range_after_archive: true,
            confirm_delete_after_archive: true,
            ..event_options()
        };

        validate_event_archive(&config(), &options).unwrap();
    }

    #[test]
    fn validate_event_archive_rejects_both_delete_modes() {
        let options = EventArchiveOptions {
            delete_after_archive: true,
            delete_source_range_after_archive: true,
            confirm_delete_after_archive: true,
            ..event_options()
        };

        let error = validate_event_archive(&config(), &options).unwrap_err();

        assert!(error.to_string().contains("cannot be used together"));
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
    fn event_clip_window_applies_pre_and_post_roll_with_range_clamp() {
        let options = event_options();

        let clip = event_clip_window(
            &event("camera-1", 5_000, Some(20_000)),
            options.range,
            &options,
        )
        .unwrap();

        assert_eq!(
            clip,
            ClipWindow {
                camera_id: "camera-1".to_string(),
                start_ms: 0,
                end_ms: 40_000
            }
        );
    }

    #[test]
    fn source_delete_range_starts_at_requested_range_and_ends_at_clip_end() {
        let archive_range = ArchiveRange {
            start_ms: 10_000,
            end_ms: 120_000,
        };
        let clip = ClipWindow {
            camera_id: "camera-1".to_string(),
            start_ms: 40_000,
            end_ms: 75_000,
        };

        let delete_range = source_delete_range_for_clip(archive_range, &clip);

        assert_eq!(delete_range.start_ms, 10_000);
        assert_eq!(delete_range.end_ms, 75_000);
    }

    #[test]
    fn event_clip_windows_merge_nearby_events_for_same_camera() {
        let options = EventArchiveOptions {
            merge_gap_seconds: 10,
            ..event_options()
        };
        let events = vec![
            event("camera-1", 10_000, Some(20_000)),
            event("camera-1", 45_000, Some(50_000)),
            event("camera-2", 45_000, Some(50_000)),
        ];

        let clips = event_clip_windows(&events, options.range, &options);

        assert_eq!(
            clips,
            vec![
                ClipWindow {
                    camera_id: "camera-1".to_string(),
                    start_ms: 0,
                    end_ms: 70_000,
                },
                ClipWindow {
                    camera_id: "camera-2".to_string(),
                    start_ms: 35_000,
                    end_ms: 70_000,
                }
            ]
        );
    }

    #[test]
    fn event_clip_windows_filter_by_event_and_smart_detect_type() {
        let options = EventArchiveOptions {
            event_types: vec!["smartDetectZone".to_string()],
            smart_detect_types: vec!["vehicle".to_string()],
            ..event_options()
        };
        let mut person = event("camera-1", 10_000, Some(20_000));
        person.smart_detect_types = vec!["person".to_string()];
        let mut vehicle = event("camera-1", 30_000, Some(40_000));
        vehicle.smart_detect_types = vec!["vehicle".to_string()];
        let mut motion = event("camera-1", 50_000, Some(60_000));
        motion.event_type = Some("motion".to_string());
        motion.smart_detect_types = vec!["vehicle".to_string()];

        let clips = event_clip_windows(&[person, vehicle, motion], options.range, &options);

        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].start_ms, 20_000);
        assert_eq!(clips[0].end_ms, 60_000);
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

    #[test]
    fn segment_count_rounds_up_partial_segments() {
        assert_eq!(
            segment_count(
                ArchiveRange {
                    start_ms: 0,
                    end_ms: 60_000,
                },
                15_000,
            ),
            4
        );
        assert_eq!(
            segment_count(
                ArchiveRange {
                    start_ms: 0,
                    end_ms: 61_000,
                },
                15_000,
            ),
            5
        );
    }

    #[test]
    fn segment_count_ignores_tiny_ranges() {
        assert_eq!(
            segment_count(
                ArchiveRange {
                    start_ms: 0,
                    end_ms: 1000,
                },
                15_000,
            ),
            0
        );
    }
}
