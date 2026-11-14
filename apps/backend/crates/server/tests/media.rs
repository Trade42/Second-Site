//! Media routes, on both stores. Expected statuses, error codes and messages are the ones the
//! Python app answers (probed with FastAPI's TestClient on the same requests); the documented
//! differences are marked.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::json;
use ss_server::security::sign_media_key;
use uuid::Uuid;

use common::mock_http::{Canned, MockServer};
use common::*;

/// `<tmp>/ss-media-<id>/objects`: the storage root, with room for a file just outside it.
fn objects_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir()
        .join(format!("ss-media-{}", Uuid::new_v4().simple()))
        .join("objects");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cleanup(objects: &std::path::Path) {
    if let Some(parent) = objects.parent() {
        let _ = std::fs::remove_dir_all(parent);
    }
}

fn part(name: &str, filename: Option<&str>, content_type: Option<&str>, data: &[u8]) -> Vec<u8> {
    let mut out = b"--BND\r\n".to_vec();
    let mut disp = format!("Content-Disposition: form-data; name=\"{name}\"");
    if let Some(f) = filename {
        disp.push_str(&format!("; filename=\"{f}\""));
    }
    out.extend_from_slice(disp.as_bytes());
    out.extend_from_slice(b"\r\n");
    if let Some(ct) = content_type {
        out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

fn form(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out: Vec<u8> = parts.concat();
    out.extend_from_slice(b"--BND--\r\n");
    out
}

async fn upload(
    t: &TestApp,
    pid: &str,
    token: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> Resp {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/projects/{pid}/media"))
        .header("content-type", content_type);
    if let Some(tok) = token {
        b = b.header("authorization", format!("Bearer {tok}"));
    }
    t.send(b.body(Body::from(body)).unwrap()).await
}

const MP: &str = "multipart/form-data; boundary=BND";

both_backends!(upload_route, |db| {
    let dir = objects_dir();
    let dir_s = dir.to_string_lossy().into_owned();
    let t = TestApp::new(db, &[("OBJECT_STORAGE_LOCAL_DIR", dir_s.as_str())]);
    let (tok, _) = t.dev_token().await;
    let pid = t
        .create_project(
            &tok,
            &json!({"project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let r = upload(
        &t,
        &pid,
        Some(&tok),
        MP,
        form(&[part("file", Some("a b.mp3"), Some("audio/mpeg"), b"abc")]),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    // sha256("abc")[:16], the filename without the space.
    assert_eq!(
        r.body,
        json!({"object_key": format!("{pid}/ba7816bf8f01cfea-ab.mp3"), "content_type": "audio/mpeg", "size_bytes": 3})
    );
    assert_eq!(
        std::fs::read(dir.join(format!("{pid}/ba7816bf8f01cfea-ab.mp3"))).unwrap(),
        b"abc"
    );
    assert_conforms_openapi("MediaStored", &r.body);

    let r = upload(
        &t,
        &pid,
        Some(&tok),
        MP,
        form(&[part("file", Some("a.txt"), Some("text/plain"), b"abc")]),
    )
    .await;
    assert_problem(&r, 400, "UNSUPPORTED_MEDIA");
    assert_eq!(r.body["message"], "unsupported content type text/plain");
    // No Content-Type on the part: Python prints None.
    let r = upload(
        &t,
        &pid,
        Some(&tok),
        MP,
        form(&[part("file", Some("a.mp3"), None, b"abc")]),
    )
    .await;
    assert_problem(&r, 400, "UNSUPPORTED_MEDIA");
    assert_eq!(r.body["message"], "unsupported content type None");

    // No `file` part, a JSON body, an empty body: 422 missing body.file.
    for (ct, body) in [
        (
            MP,
            form(&[part("other", Some("a.mp3"), Some("audio/mpeg"), b"abc")]),
        ),
        ("application/json", br#"{"file": "x"}"#.to_vec()),
        ("application/json", Vec::new()),
    ] {
        let r = upload(&t, &pid, Some(&tok), ct, body).await;
        assert_problem(&r, 422, "VALIDATION_ERROR");
        assert_eq!(
            r.body["details"]["errors"],
            json!([{"type": "missing", "loc": ["body", "file"], "msg": "Field required", "input": null}])
        );
    }
    // A plain form field named `file` (Python answers 500 here; see the module docs).
    let r = upload(
        &t,
        &pid,
        Some(&tok),
        MP,
        form(&[part("file", None, None, b"notafile")]),
    )
    .await;
    assert_problem(&r, 422, "VALIDATION_ERROR");
    assert_eq!(r.body["details"]["errors"][0]["type"], "value_error");

    // Size: exactly 15 MiB is accepted, one byte more is refused.
    let exact = vec![b'x'; 15 * 1024 * 1024];
    let r = upload(
        &t,
        &pid,
        Some(&tok),
        MP,
        form(&[part("file", Some("a.pdf"), Some("application/pdf"), &exact)]),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.body["size_bytes"], 15 * 1024 * 1024);
    let mut big = exact.clone();
    big.push(b'x');
    let r = upload(
        &t,
        &pid,
        Some(&tok),
        MP,
        form(&[part("file", Some("a.pdf"), Some("application/pdf"), &big)]),
    )
    .await;
    assert_problem(&r, 400, "FILE_TOO_LARGE");
    assert_eq!(r.body["message"], "file too large");

    // An empty filename is stored as `upload`; of two `file` parts the last one counts.
    let r = upload(
        &t,
        &pid,
        Some(&tok),
        MP,
        form(&[part("file", Some(""), Some("audio/mpeg"), b"abc")]),
    )
    .await;
    assert_eq!(
        r.body["object_key"],
        format!("{pid}/ba7816bf8f01cfea-upload")
    );
    let r = upload(
        &t,
        &pid,
        Some(&tok),
        MP,
        form(&[
            part("file", Some("1.mp3"), Some("audio/mpeg"), b"one"),
            part("file", Some("2.wav"), Some("audio/wav"), b"two2"),
        ]),
    )
    .await;
    assert_eq!(r.body["content_type"], "audio/wav");
    assert_eq!(r.body["size_bytes"], 4);
    assert!(r.body["object_key"].as_str().unwrap().ends_with("-2.wav"));

    // Malformed multipart (Python: Starlette's 400 with a `detail` body).
    let r = upload(&t, &pid, Some(&tok), MP, b"garbage".to_vec()).await;
    assert_problem(&r, 400, "BAD_REQUEST");
    assert_eq!(r.body["message"], "Invalid multipart data.");
    let r = upload(
        &t,
        &pid,
        Some(&tok),
        "multipart/form-data",
        b"garbage".to_vec(),
    )
    .await;
    assert_problem(&r, 400, "BAD_REQUEST");
    assert_eq!(r.body["message"], "Missing boundary in multipart.");

    // Authentication and ownership.
    let ok = form(&[part("file", Some("a.mp3"), Some("audio/mpeg"), b"abc")]);
    let r = upload(&t, &pid, None, MP, ok.clone()).await;
    assert_problem(&r, 401, "UNAUTHORIZED");
    let other = t.token_for(Uuid::new_v4());
    let r = upload(&t, &pid, Some(&other), MP, ok.clone()).await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "project not found");
    let r = upload(&t, &pid, Some(&other), MP, b"garbage".to_vec()).await;
    assert_problem(&r, 404, "NOT_FOUND");
    cleanup(&dir);
});

both_backends!(signed_object_route, |db| {
    let dir = objects_dir();
    let dir_s = dir.to_string_lossy().into_owned();
    let t = TestApp::new(db, &[("OBJECT_STORAGE_LOCAL_DIR", dir_s.as_str())]);
    let (tok, _) = t.dev_token().await;
    let pid = t
        .create_project(
            &tok,
            &json!({"project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}}),
        )
        .await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = upload(
        &t,
        &pid,
        Some(&tok),
        MP,
        form(&[part("file", Some("intro.wav"), Some("audio/wav"), b"RIFF")]),
    )
    .await;
    let key = r.body["object_key"].as_str().unwrap().to_owned();
    std::fs::write(dir.join(format!("{pid}/rec.mp3")), b"ID3").unwrap();
    std::fs::write(dir.join(format!("{pid}/notes.pdf")), b"%PDF").unwrap();
    std::fs::write(dir.parent().unwrap().join("secret.txt"), b"s").unwrap();

    let get = |token: String| {
        let t = &t;
        async move { t.get(&format!("/api/v1/media/public/{token}"), None).await }
    };
    let r = get(sign_media_key(SECRET, &key, 900)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "audio/wav");
    assert_eq!(r.headers["cache-control"], "no-store");
    assert_eq!(r.text, "RIFF");
    let r = get(sign_media_key(SECRET, &format!("{pid}/rec.mp3"), 900)).await;
    assert_eq!(r.headers["content-type"], "audio/mpeg");
    let r = get(sign_media_key(SECRET, &format!("{pid}/notes.pdf"), 900)).await;
    assert_eq!(r.headers["content-type"], "application/octet-stream");

    let r = get("junk".into()).await;
    assert_problem(&r, 401, "UNAUTHORIZED");
    assert_eq!(r.body["message"], "invalid media token");
    let r = get(sign_media_key("other-secret", &key, 900)).await;
    assert_problem(&r, 401, "UNAUTHORIZED");
    assert_eq!(r.body["message"], "invalid or expired media token");
    let r = get(sign_media_key(SECRET, &key, -10)).await;
    assert_problem(&r, 401, "UNAUTHORIZED");
    // An access token is not a media token.
    let r = get(tok.clone()).await;
    assert_problem(&r, 401, "UNAUTHORIZED");
    for k in [
        format!("{pid}/nope.mp3"),
        "../secret.txt".to_owned(),
        "../../etc/passwd".to_owned(),
        // A directory inside the root (Python: 500).
        pid.clone(),
    ] {
        let r = get(sign_media_key(SECRET, &k, 900)).await;
        assert_problem(&r, 404, "NOT_FOUND");
        assert_eq!(r.body["message"], "object not found", "{k}");
    }
    cleanup(&dir);
});

both_backends!(vexcel_ortho_route, |db| {
    // A stored property with coordinates, from a fixture search.
    let t = TestApp::new(db, &[]);
    let (tok, _) = t.dev_token().await;
    let pid = t.create_project(&tok, &demo_project_body()).await["project"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = t
        .post(
            &format!("/api/v1/projects/{pid}/properties/search"),
            Some(&tok),
            &json!({}),
        )
        .await;
    assert_eq!(r.body["task"]["status"], "COMPLETED", "{}", r.text);
    let props = t
        .get(&format!("/api/v1/projects/{pid}/properties"), Some(&tok))
        .await
        .body;
    let prop = &props[0];
    let prop_id = prop["property_id"].as_str().unwrap();
    let (lat, lon) = (
        prop["property_identity"]["latitude"].as_f64().unwrap(),
        prop["property_identity"]["longitude"].as_f64().unwrap(),
    );

    // Fixture mode: no imagery.
    let r = t
        .get(&format!("/api/v1/media/vexcel/ortho/{prop_id}"), None)
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "no imagery in this mode");
    let r = t
        .get(
            &format!("/api/v1/media/vexcel/ortho/{}", Uuid::new_v4()),
            None,
        )
        .await;
    assert_problem(&r, 404, "NOT_FOUND");
    assert_eq!(r.body["message"], "property not found");
    let r = t.get("/api/v1/media/vexcel/ortho/not-a-uuid", None).await;
    assert_problem(&r, 422, "VALIDATION_ERROR");

    // Live mode against a mock Vexcel: the image, then a provider failure.
    let jpeg = b"\xff\xd8\xff\xe0 jpeg".to_vec();
    let server = MockServer::start(vec![
        Canned {
            status: 200,
            content_type: Some("image/jpeg".into()),
            body: jpeg.clone(),
        },
        Canned {
            status: 401,
            content_type: None,
            body: Vec::new(),
        },
    ]);
    let base = format!("{}/v2", server.base);
    let live = TestApp::new(
        db,
        &[
            ("VEXCEL_PROVIDER_MODE", "live"),
            ("VEXCEL_BASE_URL", base.as_str()),
            ("VEXCEL_API_KEY", "vk-live-secret"),
        ],
    );
    let resp = live
        .router
        .clone()
        .oneshot_get(&format!("/api/v1/media/vexcel/ortho/{prop_id}"))
        .await;
    assert_eq!(resp.0, StatusCode::OK);
    assert_eq!(resp.1["content-type"], "image/jpeg");
    assert_eq!(resp.1["cache-control"], "private, max-age=86400");
    assert_eq!(resp.2, jpeg);
    let reqs = server.requests();
    assert_eq!(reqs[0].path, "/v2/ortho/extract");
    assert_eq!(reqs[0].headers["x-api-key"], "vk-live-secret");
    let q: std::collections::HashMap<_, _> = reqs[0].query.iter().cloned().collect();
    assert_eq!(q["wkt"], format!("POINT({lon:.7} {lat:.7})"));
    assert_eq!(q["layer"], "urban");
    assert_eq!(q["bbox-dimensions"], "80,80");
    assert_eq!(q["destination-dimensions"], "600,600");
    // A different property (not cached) gets the 401: 400 PROVIDER_UNAVAILABLE, as Python maps it.
    let other_id = props[1]["property_id"].as_str().unwrap();
    let r = live
        .get(&format!("/api/v1/media/vexcel/ortho/{other_id}"), None)
        .await;
    assert_problem(&r, 400, "PROVIDER_UNAVAILABLE");
    assert_eq!(r.body["message"], "imagery unavailable (AUTH)");
    assert!(!r.text.contains("vk-live-secret"));
});

trait OneshotGet {
    async fn oneshot_get(self, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>);
}

impl OneshotGet for axum::Router {
    async fn oneshot_get(self, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let resp = self
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, headers, bytes)
    }
}
