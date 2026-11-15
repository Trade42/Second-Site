//! The owner's recorded introduction, on both stores.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::Value;
use ss_server::security::{issue_token_at, issue_token_with};
use ss_server::services::intro_recording::{self as intro, check_wav, LINK_PURPOSE};
use uuid::Uuid;

use common::*;

fn objects_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir()
        .join(format!("ss-intro-{}", Uuid::new_v4().simple()))
        .join("objects");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A PCM WAV: 16-bit, `channels`, `rate` Hz, `secs` seconds of a quiet tone.
fn wav(rate: u32, channels: u16, secs: f64) -> Vec<u8> {
    let n = (f64::from(rate) * secs) as usize * usize::from(channels);
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + n as u32 * 2).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2 * u32::from(channels)).to_le_bytes());
    b.extend_from_slice(&(2 * channels).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(n as u32 * 2).to_le_bytes());
    for i in 0..n {
        let s = ((i as f64 * 0.05).sin() * 3000.0) as i16;
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

fn form(file: Option<(&str, &[u8])>, affirmed: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if affirmed {
        out.extend_from_slice(
            b"--BND\r\nContent-Disposition: form-data; name=\"affirmed\"\r\n\r\ntrue\r\n",
        );
    }
    if let Some((ct, data)) = file {
        out.extend_from_slice(
            format!(
                "--BND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"intro.wav\"\r\nContent-Type: {ct}\r\n\r\n"
            )
            .as_bytes(),
        );
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--BND--\r\n");
    out
}

async fn send(
    t: &TestApp,
    method: Method,
    path: &str,
    bearer: Option<&str>,
    link: Option<&str>,
    body: Option<Vec<u8>>,
) -> Resp {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(tok) = bearer {
        b = b.header("authorization", format!("Bearer {tok}"));
    }
    if let Some(l) = link {
        b = b.header("x-recording-link", l);
    }
    let req = match body {
        Some(body) => b
            .header("content-type", "multipart/form-data; boundary=BND")
            .body(Body::from(body))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    t.send(req).await
}

const PATH: &str = "/api/v1/owner/intro-recording";

async fn put(t: &TestApp, tok: &str, data: &[u8]) -> Resp {
    send(
        t,
        Method::POST,
        PATH,
        Some(tok),
        None,
        Some(form(Some(("audio/wav", data)), true)),
    )
    .await
}

/// The path part of a listen URL.
fn listen_path(v: &Value) -> String {
    let url = v["listen_url"].as_str().expect("listen_url");
    let at = url.find("/api/v1/media/public/").expect("public path");
    url[at..].to_owned()
}

#[test]
fn wav_checks() {
    assert_eq!(check_wav(&wav(16000, 1, 3.0)).unwrap().duration_ms, 3000);
    assert_eq!(check_wav(&wav(8000, 1, 2.5)).unwrap().duration_ms, 2500);
    assert!(check_wav(&wav(16000, 2, 3.0)).is_err(), "stereo");
    assert!(check_wav(&wav(44100, 1, 3.0)).is_err(), "rate");
    assert!(check_wav(&wav(16000, 1, 1.0)).is_err(), "too short");
    assert!(check_wav(&wav(8000, 1, 61.0)).is_err(), "too long");
    assert!(check_wav(b"ID3\x03mp3 data").is_err());
    let mut cut = wav(16000, 1, 3.0);
    cut.truncate(1000);
    assert!(check_wav(&cut).is_err(), "truncated");
}

both_backends!(lifecycle, |db| {
    let dir = objects_dir();
    let t = TestApp::new(db, &[("OBJECT_STORAGE_LOCAL_DIR", dir.to_str().unwrap())]);
    let (tok, owner) = t.dev_token().await;

    assert_eq!(t.get(PATH, Some(&tok)).await.status, StatusCode::NOT_FOUND);
    assert_eq!(t.get(PATH, None).await.status, StatusCode::UNAUTHORIZED);

    // The affirmation is required.
    let r = send(
        &t,
        Method::POST,
        PATH,
        Some(&tok),
        None,
        Some(form(Some(("audio/wav", &wav(16000, 1, 3.0))), false)),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
    // Wrong content type, too long, oversize.
    let r = send(
        &t,
        Method::POST,
        PATH,
        Some(&tok),
        None,
        Some(form(Some(("audio/mpeg", &wav(16000, 1, 3.0))), true)),
    )
    .await;
    assert_problem(&r, 400, "UNSUPPORTED_MEDIA");
    assert_problem(
        &put(&t, &tok, &wav(8000, 1, 70.0)).await,
        400,
        "INVALID_RECORDING",
    );
    let mut big = wav(16000, 1, 3.0);
    big.resize(intro::MAX_BYTES + 10, 0);
    assert_problem(&put(&t, &tok, &big).await, 400, "FILE_TOO_LARGE");

    // Upload and read back.
    let r = put(&t, &tok, &wav(16000, 1, 3.0)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.body["status"], "ACTIVE");
    assert_eq!(r.body["duration_ms"], 3000);
    assert_eq!(r.body["voice_verified"], false);
    assert_eq!(
        r.body["affirmation_statement"],
        intro::AFFIRMATION_STATEMENT
    );
    assert!(
        !r.text.contains("intro-recordings/"),
        "no storage key: {}",
        r.text
    );
    let first_id = r.body["recording_id"].as_str().unwrap().to_owned();
    let cur = t.get(PATH, Some(&tok)).await;
    assert_eq!(cur.status, StatusCode::OK);
    assert_eq!(cur.body["recording_id"], first_id.as_str());
    let first_link = listen_path(&cur.body);
    let heard = t.get(&first_link, None).await;
    assert_eq!(heard.status, StatusCode::OK);
    assert_eq!(heard.headers["content-type"], "audio/wav");

    // Another owner sees nothing.
    let other = t.token_for(Uuid::new_v4());
    assert_eq!(
        t.get(PATH, Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    let r = send(&t, Method::DELETE, PATH, Some(&other), None, None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // The orchestrator function sees the active one.
    let call = {
        let mut tx = t.app.store.begin().await.unwrap();
        let c = intro::active_for_call(&mut *tx, owner).await.unwrap();
        tx.commit().await.unwrap();
        c
    };
    assert_eq!(call.unwrap().recording_id.to_string(), first_id);

    // Replace: the old link stops working.
    let r = put(&t, &tok, &wav(8000, 1, 4.0)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let second_id = r.body["recording_id"].as_str().unwrap().to_owned();
    assert_eq!(t.get(&first_link, None).await.status, StatusCode::NOT_FOUND);
    let cur = t.get(PATH, Some(&tok)).await;
    assert_eq!(cur.body["recording_id"], second_id.as_str());
    let second_link = listen_path(&cur.body);
    assert_eq!(t.get(&second_link, None).await.status, StatusCode::OK);

    // Revoke: the record is kept, the audio and its links are gone.
    let r = send(&t, Method::DELETE, PATH, Some(&tok), None, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["status"], "REVOKED");
    assert_eq!(r.body["audio_deleted"], true);
    assert_eq!(
        t.get(&second_link, None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(t.get(PATH, Some(&tok)).await.status, StatusCode::NOT_FOUND);
    let mut tx = t.app.store.begin().await.unwrap();
    assert!(intro::active_for_call(&mut *tx, owner)
        .await
        .unwrap()
        .is_none());
    tx.commit().await.unwrap();

    let h = t.get(&format!("{PATH}/history"), Some(&tok)).await;
    let items = h.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["status"], "REVOKED");
    assert_eq!(items[1]["status"], "REPLACED");
    assert!(items.iter().all(|i| i.get("listen_url").is_none()));
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
});

both_backends!(recording_link, |db| {
    let dir = objects_dir();
    let t = TestApp::new(db, &[("OBJECT_STORAGE_LOCAL_DIR", dir.to_str().unwrap())]);
    let (tok, owner) = t.dev_token().await;

    let r = send(
        &t,
        Method::POST,
        &format!("{PATH}/link"),
        Some(&tok),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["expires_in_s"], 1800);
    let url = r.body["url"].as_str().unwrap();
    let link = url.split_once("/record#").unwrap().1.to_owned();

    // The page: headers and no external reference.
    let p = t.get("/record", None).await;
    assert_eq!(p.status, StatusCode::OK);
    let csp = p.headers["content-security-policy"].to_str().unwrap();
    assert!(csp.starts_with("default-src 'none'"), "{csp}");
    assert!(csp.contains("connect-src 'self'"));
    assert!(!csp.contains("unsafe-inline"));
    assert_eq!(p.headers["cache-control"], "no-store");
    assert_eq!(p.headers["referrer-policy"], "no-referrer");
    assert!(
        !p.text.contains("http://") && !p.text.contains("https://"),
        "external reference"
    );
    assert!(!p.text.contains(" src=\"http") && !p.text.contains("<link"));
    assert!(p.text.contains(intro::AFFIRMATION_STATEMENT));
    assert!(!p.text.contains("{NONCE}"));

    let check = |l: Option<String>| {
        let t = &t;
        async move {
            send(t, Method::POST, "/record/check", None, l.as_deref(), None)
                .await
                .body["valid"]
                .clone()
        }
    };
    assert_eq!(check(Some(link.clone())).await, true);
    assert_eq!(check(None).await, false);
    // Wrong purpose: an access token is not a link, and a link is not an access token.
    assert_eq!(check(Some(tok.clone())).await, false);
    assert_eq!(
        t.get(PATH, Some(&link)).await.status,
        StatusCode::UNAUTHORIZED
    );
    // Another (unknown) owner, and expired.
    let stranger = issue_token_with(SECRET, Uuid::new_v4(), 1800, LINK_PURPOSE);
    assert_eq!(check(Some(stranger)).await, false);
    let expired = issue_token_at(SECRET, owner, LINK_PURPOSE, 1_000);
    assert_eq!(check(Some(expired.clone())).await, false);
    let body = || Some(form(Some(("audio/wav", &wav(16000, 1, 3.0))), true));
    let r = send(
        &t,
        Method::POST,
        "/record/upload",
        None,
        Some(&expired),
        body(),
    )
    .await;
    assert_problem(&r, 401, "LINK_INVALID");

    // Upload once, then the link is used.
    let r = send(
        &t,
        Method::POST,
        "/record/upload",
        None,
        Some(&link),
        body(),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let cur = t.get(PATH, Some(&tok)).await;
    assert_eq!(cur.body["source"], "recording_link");
    assert_eq!(check(Some(link.clone())).await, false);
    let r = send(
        &t,
        Method::POST,
        "/record/upload",
        None,
        Some(&link),
        body(),
    )
    .await;
    assert_problem(&r, 401, "LINK_INVALID");
    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
});
