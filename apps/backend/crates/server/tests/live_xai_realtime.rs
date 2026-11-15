//! Opt-in live probe: does the product's own xAI realtime client reach xAI from this machine?
//!
//! Never runs by default: the test is `#[ignore]`, and even with `--ignored` it returns early
//! unless `LIVE_XAI=1` is set and `XAI_API_KEY` is configured. The key comes only from the crate's
//! settings loader, reading the `.env` files exactly as the service does when it runs from
//! `apps/backend`; it goes into the handshake header through [`XaiConfig`] and nowhere else. The
//! test prints event types, the audio formats xAI reports, counts, and an error's `code` and
//! `message` (with the key replaced, should a text ever carry it). It never prints the key, the
//! instructions or any audio.
//!
//! One run opens exactly one realtime session of a few seconds. It sends no audio
//! (`input_audio_buffer.append` is never sent), places no call and sends nothing to Telnyx.
//!
//! Run (from `apps/backend`):
//! `LIVE_XAI=1 cargo test -p ss-server --test live_xai_realtime -- --ignored --nocapture`
//! `LIVE_XAI_STEP=1` (default): connect, `session.update`, wait for `session.updated` or an
//! error, close. `LIVE_XAI_STEP=2`: the same, then the opening line as the bridge has it spoken
//! (`force_message`, no `response.create`), and up to ten seconds of the answer: audio deltas are
//! counted, not kept.
//!
//! What it uses of the product: `xai_realtime::connect` (URL from `XAI_REALTIME_URL` or the
//! default, the bearer header, the frame limits, the rustls connector), the `session.update` of
//! `voice_session_config::build_from` for a synthetic context, `force_message`, and
//! `parse_server_event` (so the output shows how the bridge would read each frame).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use ss_domain::communications::call_purposes::Approved;
use ss_domain::communications::response_agent::CallContext;
use ss_server::config::Settings;
use ss_server::integrations::xai_realtime::{self as xai, ServerEvent, XaiConfig, XaiSocket};
use ss_server::services::voice_session_config as vsc;
use tokio_tungstenite::tungstenite::Message;

/// How long each wait may take.
const FIRST_EVENT_WAIT: Duration = Duration::from_secs(8);
const SESSION_WAIT: Duration = Duration::from_secs(8);
const SPEECH_WAIT: Duration = Duration::from_secs(10);
const CLOSE_WAIT: Duration = Duration::from_secs(4);

/// `apps/backend`, where the service runs; `../../.env` from there is the repository `.env`.
fn service_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn short(s: &str, key: &str) -> String {
    let s = if key.is_empty() {
        s.to_owned()
    } else {
        s.replace(key, "<key>")
    };
    s.chars().take(300).collect()
}

/// One frame from xAI, or why there is none.
enum Frame {
    Event(Value, String),
    Closed(String),
    Timeout,
}

async fn next_frame(ws: &mut XaiSocket, until: Instant) -> Frame {
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Frame::Timeout;
        }
        match tokio::time::timeout(left, ws.next()).await {
            Err(_) => return Frame::Timeout,
            Ok(None) => return Frame::Closed("stream ended".into()),
            Ok(Some(Err(e))) => return Frame::Closed(format!("socket error: {e}")),
            Ok(Some(Ok(Message::Close(c)))) => {
                return Frame::Closed(match c {
                    Some(c) => format!("close frame code={} reason={:?}", c.code, c.reason),
                    None => "close frame without a code".into(),
                })
            }
            Ok(Some(Ok(Message::Text(t)))) => {
                let text = t.to_string();
                match serde_json::from_str::<Value>(&text) {
                    Ok(v) => return Frame::Event(v, text),
                    Err(_) => println!("  (a text frame that is not JSON, {} bytes)", text.len()),
                }
            }
            Ok(Some(Ok(other))) => println!("  (a non-text frame, {} bytes)", other.len()),
        }
    }
}

fn ty(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("<no type>")
}

/// Print one server event: its type, how the bridge's parser reads it, and for an error its
/// `type`, `code` and `message`.
fn show(n: usize, at: Instant, v: &Value, text: &str, key: &str) {
    let parsed = match xai::parse_server_event(text) {
        Ok(ServerEvent::Other) => "bridge: ignored (Other)".to_owned(),
        Ok(ServerEvent::AudioDelta { audio, .. }) => format!("bridge: AudioDelta {}B", audio.len()),
        Ok(e) => {
            let d = format!("{e:?}");
            format!("bridge: {}", d.split([' ', '{', '(']).next().unwrap_or(""))
        }
        Err(_) => "bridge: MALFORMED".to_owned(),
    };
    println!(
        "[event {n} +{}ms] {} ({parsed})",
        at.elapsed().as_millis(),
        ty(v)
    );
    if ty(v) == "error" {
        let e = v.get("error").unwrap_or(v);
        let f = |k: &str| e.get(k).map(|x| short(&x.to_string(), key));
        println!(
            "    error type={:?} code={:?} param={:?} message={:?}",
            f("type"),
            f("code"),
            f("param"),
            f("message")
        );
    }
}

/// The parts of a `session.created` / `session.updated` body that say what xAI will send and
/// accept (never the instructions).
fn show_session(v: &Value) {
    let Some(s) = v.get("session") else {
        println!("    (no `session` object)");
        return;
    };
    let keys: Vec<&str> = s
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    println!("    session keys: {keys:?}");
    for k in [
        "model",
        "voice",
        "turn_detection",
        "audio",
        "input_audio_format",
        "output_audio_format",
        "modalities",
        "output_modalities",
    ] {
        if let Some(x) = s.get(k) {
            println!("    session.{k} = {x}");
        }
    }
    if let Some(t) = s.get("tools").and_then(Value::as_array) {
        let names: Vec<&str> = t
            .iter()
            .filter_map(|x| x.get("name").and_then(Value::as_str))
            .collect();
        let keys: Vec<&str> = t
            .first()
            .and_then(Value::as_object)
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        println!(
            "    session.tools: {} entries, names {names:?}, first entry's keys {keys:?}",
            t.len()
        );
    }
    if let Some(i) = s.get("instructions").and_then(Value::as_str) {
        println!("    session.instructions: {} chars", i.chars().count());
    }
}

#[tokio::test]
#[ignore = "live: opens one real xAI realtime session; needs LIVE_XAI=1 and XAI_API_KEY"]
async fn the_products_client_reaches_xai_realtime() {
    if std::env::var("LIVE_XAI").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set LIVE_XAI=1 to open one real xAI realtime session");
        return;
    }
    let step: u8 = std::env::var("LIVE_XAI_STEP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let settings = Settings::from_sources(&service_dir(), std::env::vars()).expect("settings load");
    let key = match settings.xai_api_key.as_ref().map(|k| k.expose().to_owned()) {
        Some(k) if !k.is_empty() => k,
        _ => {
            eprintln!("SKIPPED: XAI_API_KEY is not configured");
            return;
        }
    };

    // The session the bridge would configure, for a synthetic call (nobody's data).
    let context = CallContext {
        property_summary: "a synthetic test listing, not a real property".into(),
        ..CallContext::default()
    };
    let built = vsc::build_from(
        "the Second Site connection probe",
        &["A synthetic business used only to test the connection.".to_owned()],
        context,
        &[],
        &settings.xai_voice,
        &Approved::default(),
    );
    let update = built.session.session_update();
    let url = settings.xai_realtime_url.clone();
    println!("url: {}", short(url.split('?').next().unwrap_or(""), &key));
    println!(
        "query: {}",
        short(url.split_once('?').map_or("", |(_, q)| q), &key)
    );
    println!(
        "session.update: voice={:?} audio={} turn_detection={} tools={} instructions={} chars",
        built.session.voice,
        update["session"]["audio"],
        update["session"]["turn_detection"],
        built.session.tools.len(),
        built.session.instructions.chars().count()
    );

    // 1. The socket: TCP, TLS and the upgrade, with the product's connect.
    let cfg = XaiConfig::new(url.clone(), key.clone());
    let t0 = Instant::now();
    println!("[session 1 of this run] connecting");
    let mut ws = match xai::connect(&cfg).await {
        Ok(ws) => ws,
        Err(e) => {
            // Which layer: can the name be resolved and the port reached at all? (No request.)
            let host = url
                .split("://")
                .nth(1)
                .and_then(|r| r.split(['/', '?']).next())
                .unwrap_or("");
            let addr = if host.contains(':') {
                host.to_owned()
            } else {
                format!("{host}:443")
            };
            let tcp = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::TcpStream::connect(addr.as_str()),
            )
            .await;
            let tcp = match tcp {
                Ok(Ok(_)) => "TCP connect to the host works".to_owned(),
                Ok(Err(e)) => format!("TCP connect fails: {e}"),
                Err(_) => "TCP connect timed out".to_owned(),
            };
            panic!(
                "STEP connect FAILED after {}ms: {e:?} ({e}); {tcp}",
                t0.elapsed().as_millis()
            );
        }
    };
    println!(
        "connected (TLS and upgrade) in {}ms",
        t0.elapsed().as_millis()
    );

    // The bridge sends `session.update` at once, without waiting for a first event.
    ws.send(Message::text(update.to_string()))
        .await
        .expect("send session.update");
    println!("sent session.update at +{}ms", t0.elapsed().as_millis());

    // 2. The first server event, then `session.updated` or an error.
    let mut n = 0usize;
    let mut types: Vec<String> = Vec::new();
    let mut updated = false;
    let mut error_seen = false;
    let first_until = Instant::now() + FIRST_EVENT_WAIT;
    let session_until = Instant::now() + SESSION_WAIT;
    while !updated && !error_seen {
        let until = if n == 0 { first_until } else { session_until };
        match next_frame(&mut ws, until).await {
            Frame::Event(v, text) => {
                n += 1;
                show(n, t0, &v, &text, &key);
                types.push(ty(&v).to_owned());
                match ty(&v) {
                    "session.updated" => {
                        updated = true;
                        show_session(&v);
                    }
                    "session.created" | "conversation.created" => show_session(&v),
                    "error" => error_seen = true,
                    _ => {}
                }
            }
            Frame::Closed(why) => panic!(
                "STEP session FAILED: xAI closed the socket: {why}; events so far: {types:?}"
            ),
            Frame::Timeout => panic!(
                "STEP session FAILED: no {} within the wait; events so far: {types:?}",
                if n == 0 {
                    "first server event"
                } else {
                    "session.updated or error"
                }
            ),
        }
    }
    assert!(n >= 1, "a first server event arrived");

    // 3. Step 2 only: the opening line, exactly as the bridge has it spoken.
    let mut speech_ok = true;
    if step >= 2 && updated {
        let line = built.opening_line.clone();
        println!(
            "sent force_message (opening line, {} chars, interruptible=false) at +{}ms; no response.create",
            line.chars().count(),
            t0.elapsed().as_millis()
        );
        ws.send(Message::text(xai::force_message(&line, false).to_string()))
            .await
            .expect("send force_message");
        let sent = Instant::now();
        let until = sent + SPEECH_WAIT;
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let (mut deltas, mut bytes, mut b64_chars) = (0usize, 0usize, 0usize);
        let (mut mulaw_quiet, mut zero) = (0usize, 0usize);
        let mut first_delta: Option<Duration> = None;
        let mut last_delta: Option<Duration> = None;
        let mut created_id: Option<String> = None;
        let mut created_before_delta = false;
        let mut delta_ids: BTreeMap<String, usize> = BTreeMap::new();
        let mut done: Option<(Option<String>, Option<String>)> = None;
        let mut transcript: Option<String> = None;
        loop {
            match next_frame(&mut ws, until).await {
                Frame::Event(v, text) => {
                    n += 1;
                    let t = ty(&v).to_owned();
                    *counts.entry(t.clone()).or_default() += 1;
                    if order.last() != Some(&t) {
                        order.push(t.clone());
                    }
                    let parsed = xai::parse_server_event(&text);
                    match &parsed {
                        Ok(ServerEvent::AudioDelta { response_id, audio }) => {
                            deltas += 1;
                            bytes += audio.len();
                            b64_chars += v["delta"].as_str().map_or(0, str::len);
                            mulaw_quiet += audio
                                .iter()
                                .filter(|b| matches!(**b, 0xFF | 0x7F | 0xFE | 0x7E))
                                .count();
                            zero += audio.iter().filter(|b| **b == 0).count();
                            first_delta.get_or_insert(sent.elapsed());
                            last_delta = Some(sent.elapsed());
                            *delta_ids
                                .entry(response_id.clone().unwrap_or_else(|| "<none>".into()))
                                .or_default() += 1;
                            if deltas == 1 {
                                show(n, t0, &v, &text, &key);
                                let keys: Vec<&str> = v
                                    .as_object()
                                    .map(|o| o.keys().map(String::as_str).collect())
                                    .unwrap_or_default();
                                println!("    first delta keys: {keys:?}");
                            }
                        }
                        _ => {
                            show(n, t0, &v, &text, &key);
                            match parsed {
                                Ok(ServerEvent::ResponseCreated { id }) => {
                                    created_before_delta = deltas == 0;
                                    created_id = Some(id);
                                    if let Some(r) = v.get("response") {
                                        for k in [
                                            "status",
                                            "output_audio_format",
                                            "audio",
                                            "modalities",
                                            "output_modalities",
                                        ] {
                                            if let Some(x) = r.get(k) {
                                                println!("    response.{k} = {x}");
                                            }
                                        }
                                    }
                                }
                                Ok(ServerEvent::OutputTranscriptDone {
                                    transcript: tr, ..
                                }) => {
                                    transcript = Some(tr);
                                }
                                Ok(ServerEvent::ResponseDone { id, status }) => {
                                    if let Some(u) = v.get("response").and_then(|r| r.get("usage"))
                                    {
                                        println!(
                                            "    response.usage = {}",
                                            short(&u.to_string(), &key)
                                        );
                                    }
                                    done = Some((id, status));
                                    break;
                                }
                                Ok(ServerEvent::Error { .. }) => error_seen = true,
                                _ => {}
                            }
                        }
                    }
                }
                Frame::Closed(why) => {
                    println!("xAI closed the socket during the opening line: {why}");
                    speech_ok = false;
                    break;
                }
                Frame::Timeout => {
                    println!(
                        "the {}s wait ended without response.done",
                        SPEECH_WAIT.as_secs()
                    );
                    break;
                }
            }
        }
        println!("--- opening line, as the bridge would see it ---");
        println!("event types in order (runs collapsed): {order:?}");
        println!("event counts: {counts:?}");
        println!(
            "audio deltas: {deltas}, decoded bytes: {bytes} (base64 chars {b64_chars}), first at {:?}, last at {:?}",
            first_delta, last_delta
        );
        if bytes > 0 {
            println!(
                "as 8 kHz mu-law (1 byte per sample) that is {:.2}s of audio; as 24 kHz PCM16 it would be {:.2}s; odd byte count: {}",
                bytes as f64 / 8000.0,
                bytes as f64 / 48000.0,
                bytes % 2 == 1
            );
            println!(
                "byte values: {:.1}% mu-law quiet (0xFF/0x7F/0xFE/0x7E), {:.1}% zero (PCM16 silence would be mostly zero)",
                100.0 * mulaw_quiet as f64 / bytes as f64,
                100.0 * zero as f64 / bytes as f64
            );
        }
        let id_match = created_id
            .as_ref()
            .is_some_and(|c| delta_ids.len() == 1 && delta_ids.contains_key(c));
        println!(
            "response.created seen: {} (before the first delta: {created_before_delta}); deltas by response_id match it: {id_match} ({} distinct ids, none-count {})",
            created_id.is_some(),
            delta_ids.len(),
            delta_ids.get("<none>").copied().unwrap_or(0)
        );
        match &done {
            Some((id, status)) => println!(
                "response.done: status={status:?}, id matches response.created: {}",
                id.is_some() && *id == created_id
            ),
            None => println!("response.done: NOT SEEN within {}s", SPEECH_WAIT.as_secs()),
        }
        if let Some(tr) = &transcript {
            println!(
                "output transcript equals the opening line: {} ({} chars)",
                tr.trim() == line.trim(),
                tr.chars().count()
            );
        }
        speech_ok = speech_ok && deltas > 0 && id_match;
    }

    // 4. A clean close: our close frame, then xAI's.
    let closing = Instant::now();
    let sent_close = ws.close(None).await;
    let mut closed = String::from("no answer to our close frame within the wait");
    let until = Instant::now() + CLOSE_WAIT;
    loop {
        match next_frame(&mut ws, until).await {
            Frame::Event(v, _) => println!("  (event after our close: {})", ty(&v)),
            Frame::Closed(why) => {
                closed = why;
                break;
            }
            Frame::Timeout => break,
        }
    }
    println!(
        "close: sent={:?}, xAI: {closed} (+{}ms); session lasted {}ms",
        sent_close.is_ok(),
        closing.elapsed().as_millis(),
        t0.elapsed().as_millis()
    );

    println!("event types before the opening line: {types:?}");
    assert!(
        updated || error_seen,
        "session.update was neither acknowledged nor answered with an error"
    );
    assert!(
        updated,
        "STEP session FAILED: xAI answered session.update with an error (above)"
    );
    assert!(
        closed.starts_with("close frame") || closed == "stream ended",
        "the socket did not close cleanly: {closed}"
    );
    if step >= 2 {
        assert!(
            speech_ok,
            "STEP opening line FAILED: no audio the bridge would pass on (see the summary above)"
        );
    }
    println!("RESULT: step {step} passed");
}
