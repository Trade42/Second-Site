//! The owner's recorded introduction: the short clip, **in the owner's own voice**, that the
//! carrier plays first on an authorized call ("Hi, this is ... I'm authorizing my assistant to
//! handle this call on my behalf today ...").
//!
//! **Truth.** This service never generates, clones, edits, transcodes or synthesizes an
//! introduction, and there is no text-to-speech fallback here: it stores the bytes the owner sent,
//! after checking only that they are a WAV file the carrier can play. A record says the voice was
//! *affirmed by the owner* (the statement they affirmed, word for word, with its time); code cannot
//! verify whose voice it is, and no field says "verified".
//!
//! **Format.** Telnyx `playback_start` plays "either a WAV or MP3 file"
//! (<https://developers.telnyx.com/api-reference/call-commands/play-audio-url>). Only WAV is
//! accepted here, as linear PCM, 16-bit, mono, at 8000 or 16000 Hz (the recording page encodes
//! 16000 Hz in the browser), so the header gives the exact duration and nothing is transcoded.
//! Bounds: [`MIN_DURATION_MS`] to [`MAX_DURATION_MS`], at most [`MAX_BYTES`].
//!
//! **Storage.** The audio goes to object storage under
//! `intro-recordings/{owner_id}/{recording_id}/...-intro.wav`; the record is a row of
//! `owner_intro_recordings`, a Rust-owned table alembic does not know (as Nessie's). One `ACTIVE`
//! record per owner: a new one makes the earlier one `REPLACED`; `DELETE` makes it `REVOKED`. In
//! both cases the audio file is deleted (after the commit; a failed deletion is retried at the
//! owner's next change), `object_key` is cleared and the record kept. A signed listen link
//! (`/api/v1/media/public/{token}`) resolves the key to the file, so once the file is gone every
//! link to it answers 404: a replaced or revoked introduction cannot be played again.
//!
//! **Recording link.** A token of purpose [`LINK_PURPOSE`] for one owner, valid
//! [`LINK_TTL_S`] seconds, used once (its SHA-256 is stored on the record it created). It can do
//! nothing but upload that owner's introduction. It travels in the URL fragment (`/record#token`),
//! so it never reaches a request log or a `Referer`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ss_core::{utcnow, Checker, Contract, UtcDateTime, Validate};
use uuid::Uuid;

use crate::app::App;
use crate::error::{AppError, AppResult};
use crate::integrations::blocking;
use crate::logging;
use crate::services::design::lock_key;
use crate::store::repo::{users, Document, Repo};
use crate::store::schema::{self, TableDef};
use crate::store::{Query, Row, Tx};

pub const STATUS_ACTIVE: &str = "ACTIVE";
pub const STATUS_REPLACED: &str = "REPLACED";
pub const STATUS_REVOKED: &str = "REVOKED";

pub const CONTENT_TYPE: &str = "audio/wav";
pub const SAMPLE_RATES_HZ: [u32; 2] = [8000, 16000];
pub const MIN_DURATION_MS: i64 = 2_000;
pub const MAX_DURATION_MS: i64 = 60_000;
/// 60 s of 16 kHz 16-bit mono is 1,920,000 bytes; the rest is header room.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

pub const LINK_PURPOSE: &str = "intro_recording_link";
pub const LINK_TTL_S: i64 = 30 * 60;
/// A listen link for the owner's own review.
pub const LISTEN_TTL_S: i64 = 300;

/// What the owner affirms, stored word for word on the record.
pub const AFFIRMATION_STATEMENT: &str = "This recording is my own voice, recorded by me. It \
authorizes my assistant to speak for me on calls about business locations.";
pub const AFFIRMATION_BASIS: &str = "affirmed by the owner (the voice is not verified by code)";
pub const EXAMPLE_WORDING: &str = "Hi, this is [your name]. I'm authorizing my assistant to \
handle this call on my behalf today about your space.";

pub const SOURCE_UPLOAD: &str = "api_upload";
pub const SOURCE_LINK: &str = "recording_link";

/// A row of `owner_intro_recordings` (`id` is `recording_id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntroRecording {
    pub recording_id: Uuid,
    pub owner_id: Uuid,
    pub status: String,
    /// Cleared when the audio is deleted. Never sent to a client.
    pub object_key: Option<String>,
    pub content_type: String,
    pub size_bytes: i64,
    pub duration_ms: i64,
    pub sample_rate_hz: i64,
    pub source: String,
    pub affirmation_statement: String,
    pub affirmed_at: UtcDateTime,
    pub affirmation_basis: String,
    /// SHA-256 (hex) of the recording-link token that uploaded it, so a link works once.
    pub link_token_sha256: Option<String>,
    pub created_at: UtcDateTime,
    pub replaced_at: Option<UtcDateTime>,
    pub replaced_by: Option<Uuid>,
    pub revoked_at: Option<UtcDateTime>,
    pub revoked_by: Option<Uuid>,
    pub audio_deleted_at: Option<UtcDateTime>,
}

impl Validate for IntroRecording {
    fn check(&self, c: &mut Checker) {
        c.max_len("status", &self.status, 16);
        if ![STATUS_ACTIVE, STATUS_REPLACED, STATUS_REVOKED].contains(&self.status.as_str()) {
            c.field_value_error("status", "status must be ACTIVE, REPLACED or REVOKED");
        }
        if self.status == STATUS_ACTIVE && self.object_key.is_none() {
            c.value_error("an ACTIVE recording has its audio");
        }
    }
}

impl Contract for IntroRecording {
    const NAME: &'static str = "IntroRecording";
}

impl Document for IntroRecording {
    const TABLE: &'static TableDef = &schema::OWNER_INTRO_RECORDINGS;
    fn id(&self) -> Uuid {
        self.recording_id
    }
    fn columns(&self) -> Row {
        Row::new()
            .with("owner_id", self.owner_id)
            .with("status", self.status.as_str())
            .with("link_token_sha256", self.link_token_sha256.clone())
    }
}

pub const INTRO_RECORDINGS: Repo<IntroRecording> = Repo::new();

/// What the call orchestrator needs to play the owner's introduction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallIntro {
    pub recording_id: Uuid,
    /// The storage key `on_answered` signs into a `/api/v1/media/public/{token}` link.
    pub object_key: String,
    pub duration_ms: i64,
}

/// The facts read from a WAV header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavInfo {
    pub sample_rate_hz: u32,
    pub duration_ms: i64,
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}
fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

/// Checks that `data` is a PCM WAV the carrier plays (16-bit, mono, 8 or 16 kHz) of an allowed
/// length. The error is the sentence the owner is shown.
pub fn check_wav(data: &[u8]) -> Result<WavInfo, String> {
    let bad = || "not a WAV file (RIFF/WAVE PCM expected)".to_owned();
    if data.len() > MAX_BYTES {
        return Err(format!("the recording is larger than {MAX_BYTES} bytes"));
    }
    if data.get(0..4) != Some(b"RIFF") || data.get(8..12) != Some(b"WAVE") {
        return Err(bad());
    }
    let mut at = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data_len: Option<usize> = None;
    while let (Some(id), Some(len)) = (data.get(at..at + 4), le32(data, at + 4)) {
        let body = at + 8;
        let len = len as usize;
        if id == b"fmt " {
            fmt = Some((
                le16(data, body).ok_or_else(bad)?,
                le16(data, body + 2).ok_or_else(bad)?,
                le32(data, body + 4).ok_or_else(bad)?,
                le16(data, body + 14).ok_or_else(bad)?,
            ));
        } else if id == b"data" {
            if body.checked_add(len).is_none_or(|end| end > data.len()) {
                return Err("the WAV data chunk is truncated".to_owned());
            }
            data_len = Some(len);
            break;
        }
        at = body.checked_add(len + (len & 1)).ok_or_else(bad)?;
    }
    let (format, channels, rate, bits) = fmt.ok_or_else(bad)?;
    if format != 1 || bits != 16 {
        return Err("the WAV must be 16-bit linear PCM".to_owned());
    }
    if channels != 1 {
        return Err("the WAV must be mono".to_owned());
    }
    if !SAMPLE_RATES_HZ.contains(&rate) {
        return Err("the WAV sample rate must be 8000 or 16000 Hz".to_owned());
    }
    let len = data_len.ok_or_else(bad)? as i64;
    let duration_ms = len * 1000 / (i64::from(rate) * 2);
    if duration_ms < MIN_DURATION_MS {
        return Err(format!(
            "the recording is shorter than {} seconds",
            MIN_DURATION_MS / 1000
        ));
    }
    if duration_ms > MAX_DURATION_MS {
        return Err(format!(
            "the recording is longer than {} seconds",
            MAX_DURATION_MS / 1000
        ));
    }
    Ok(WavInfo {
        sample_rate_hz: rate,
        duration_ms,
    })
}

pub fn token_digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

async fn lock_owner(tx: &mut dyn Tx, owner_id: Uuid) -> AppResult<()> {
    tx.advisory_xact_lock(lock_key("intro-recording", owner_id))
        .await?;
    Ok(())
}

async fn of_owner(tx: &mut dyn Tx, owner_id: Uuid) -> AppResult<Vec<IntroRecording>> {
    INTRO_RECORDINGS
        .list(tx, Query::new().eq("owner_id", owner_id))
        .await
}

/// The owner's `ACTIVE` recording, if any.
pub async fn active(tx: &mut dyn Tx, owner_id: Uuid) -> AppResult<Option<IntroRecording>> {
    Ok(of_owner(tx, owner_id)
        .await?
        .into_iter()
        .rev()
        .find(|r| r.status == STATUS_ACTIVE && r.object_key.is_some()))
}

/// Replaced and revoked records, newest first.
pub async fn history(tx: &mut dyn Tx, owner_id: Uuid) -> AppResult<Vec<IntroRecording>> {
    let mut v: Vec<_> = of_owner(tx, owner_id)
        .await?
        .into_iter()
        .filter(|r| r.status != STATUS_ACTIVE)
        .collect();
    v.reverse();
    Ok(v)
}

/// **For the call orchestrator.** The introduction a call about `owner_id` may play: the owner's
/// `ACTIVE` recording, or `None` (no recording, or it was replaced or revoked). Call it in the
/// transaction that dials, after the approval check, and again when the call is answered; a
/// `None` means the call must not start (or must not play anything), never a substitute voice.
pub async fn active_for_call(tx: &mut dyn Tx, owner_id: Uuid) -> AppResult<Option<CallIntro>> {
    Ok(active(tx, owner_id).await?.and_then(|r| {
        r.object_key.map(|object_key| CallIntro {
            recording_id: r.recording_id,
            object_key,
            duration_ms: r.duration_ms,
        })
    }))
}

/// Whether a link token has already created a record.
pub async fn link_used(tx: &mut dyn Tx, token: &str) -> AppResult<bool> {
    let rows = INTRO_RECORDINGS
        .list(
            tx,
            Query::new().eq("link_token_sha256", token_digest(token).as_str()),
        )
        .await?;
    Ok(!rows.is_empty())
}

/// Deletes one stored object of the local object storage. `ObjectStorage` has no delete, so this
/// resolves the key against the same root (`OBJECT_STORAGE_LOCAL_DIR`) with the same containment
/// rule as `LocalObjectStorage::get`. A missing file counts as deleted.
fn delete_object(root: &Path, key: &str) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| e.kind().to_string())?;
    let path: PathBuf = match root.join(key).canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.kind().to_string()),
    };
    if path == root || !path.starts_with(&root) || !path.is_file() {
        return Err("key does not name a stored file".to_owned());
    }
    std::fs::remove_file(&path).map_err(|e| e.kind().to_string())?;
    // The record's own directory, when now empty.
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// Deletes the audio of every non-active record of the owner that still has some, and marks it.
async fn purge_inactive_audio(app: &App, owner_id: Uuid) -> AppResult<()> {
    let pending: Vec<IntroRecording> = crate::in_tx!(app, |tx| {
        of_owner(tx, owner_id)
            .await?
            .into_iter()
            .filter(|r| r.status != STATUS_ACTIVE && r.object_key.is_some())
            .collect()
    });
    let root = PathBuf::from(&app.settings.object_storage_local_dir);
    for rec in pending {
        let Some(key) = rec.object_key.clone() else {
            continue;
        };
        let r = root.clone();
        let deleted = blocking(move || delete_object(&r, &key)).await?;
        if let Err(kind) = deleted {
            logging::warning(
                "app.intro_recording",
                "audio deletion failed; retried at the next change",
                &[
                    ("recording_id", rec.recording_id.to_string().into()),
                    ("error_kind", kind.into()),
                ],
            );
            continue;
        }
        crate::in_tx!(app, |tx| {
            lock_owner(tx, owner_id).await?;
            if let Some(mut cur) = INTRO_RECORDINGS.get(tx, rec.recording_id).await? {
                if cur.status != STATUS_ACTIVE {
                    cur.object_key = None;
                    cur.audio_deleted_at = Some(utcnow());
                    INTRO_RECORDINGS.upsert(tx, &cur).await?;
                }
            }
        });
    }
    Ok(())
}

/// One upload, already read: the audio bytes and the affirmation.
pub struct NewRecording {
    pub owner_id: Uuid,
    pub data: Vec<u8>,
    pub source: &'static str,
    /// The recording-link token, for the one-use check.
    pub link_token: Option<String>,
}

/// Stores the audio and makes it the owner's one `ACTIVE` recording; the earlier one becomes
/// `REPLACED` and its audio is deleted. The caller has checked the affirmation and the format.
pub async fn store(app: &App, new: NewRecording, info: WavInfo) -> AppResult<IntroRecording> {
    let owner_id = new.owner_id;
    let exists = crate::in_tx!(app, |tx| { users::get(tx, owner_id).await?.is_some() });
    if !exists {
        return Err(AppError::unauthorized("unknown user"));
    }
    let recording_id = Uuid::new_v4();
    let size = new.data.len() as i64;
    let storage = app.integrations().storage.clone();
    let prefix = format!("intro-recordings/{owner_id}/{recording_id}");
    let data = new.data;
    let key = blocking(move || storage.put(&prefix, "intro.wav", &data, CONTENT_TYPE))
        .await?
        .map_err(|e| AppError::internal_kind("OSError", e.to_string()))?;
    let now = utcnow();
    let rec = IntroRecording {
        recording_id,
        owner_id,
        status: STATUS_ACTIVE.to_owned(),
        object_key: Some(key.clone()),
        content_type: CONTENT_TYPE.to_owned(),
        size_bytes: size,
        duration_ms: info.duration_ms,
        sample_rate_hz: i64::from(info.sample_rate_hz),
        source: new.source.to_owned(),
        affirmation_statement: AFFIRMATION_STATEMENT.to_owned(),
        affirmed_at: now,
        affirmation_basis: AFFIRMATION_BASIS.to_owned(),
        link_token_sha256: new.link_token.as_deref().map(token_digest),
        created_at: now,
        replaced_at: None,
        replaced_by: None,
        revoked_at: None,
        revoked_by: None,
        audio_deleted_at: None,
    };
    let link_token = new.link_token;
    let saved: AppResult<()> = async {
        crate::in_tx!(app, |tx| {
            lock_owner(tx, owner_id).await?;
            if let Some(t) = link_token.as_deref() {
                if link_used(tx, t).await? {
                    return Err(AppError::unauthorized("recording link already used")
                        .code("LINK_USED"));
                }
            }
            for mut old in of_owner(tx, owner_id).await? {
                if old.status == STATUS_ACTIVE {
                    old.status = STATUS_REPLACED.to_owned();
                    old.replaced_at = Some(now);
                    old.replaced_by = Some(recording_id);
                    INTRO_RECORDINGS.upsert(tx, &old).await?;
                }
            }
            INTRO_RECORDINGS.add(tx, &rec).await?;
        });
        Ok(())
    }
    .await;
    if let Err(e) = saved {
        let root = PathBuf::from(&app.settings.object_storage_local_dir);
        let _ = blocking(move || delete_object(&root, &key)).await;
        return Err(e);
    }
    logging::info(
        "app.intro_recording",
        "intro recording stored",
        &[
            ("recording_id", recording_id.to_string().into()),
            ("source", new.source.into()),
            ("duration_ms", info.duration_ms.into()),
        ],
    );
    purge_inactive_audio(app, owner_id).await?;
    Ok(rec)
}

/// Revokes the owner's `ACTIVE` recording and deletes its audio; `None` when there is none.
pub async fn revoke(app: &App, owner_id: Uuid) -> AppResult<Option<IntroRecording>> {
    let revoked = crate::in_tx!(app, |tx| {
        lock_owner(tx, owner_id).await?;
        match active(tx, owner_id).await? {
            Some(mut r) => {
                r.status = STATUS_REVOKED.to_owned();
                r.revoked_at = Some(utcnow());
                r.revoked_by = Some(owner_id);
                INTRO_RECORDINGS.upsert(tx, &r).await?;
                Some(r.recording_id)
            }
            None => None,
        }
    });
    purge_inactive_audio(app, owner_id).await?;
    let Some(id) = revoked else {
        return Ok(None);
    };
    logging::info(
        "app.intro_recording",
        "intro recording revoked",
        &[("recording_id", id.to_string().into())],
    );
    Ok(crate::in_tx!(app, |tx| {
        INTRO_RECORDINGS.get(tx, id).await?
    }))
}
