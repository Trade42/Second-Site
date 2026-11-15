"""Every route answers only after its transaction has committed (e2e-1 defect D1).

`app.api.deps.db_session` hands each route one session per request. Its commit (or, on an exception, its rollback),
then its close, run when the path function returns and before FastAPI sends the response. Before, `get_db` committed
in dependency teardown *after* the response had been sent: another service reading the database at once could miss a
row the caller had already been told about, and a refused commit was answered with a success.

The proofs use the real app and the real PostgreSQL:
- a probe in front of the app counts the new row through another connection at the moment the answer starts;
- a real refused COMMIT (a deferred constraint trigger matching one private row, as in test_internal_notify) must be
  answered with a non-2xx, and nothing may be stored.
The streaming events route and a media route must keep working exactly as before.
"""

from __future__ import annotations

from uuid import uuid4

import pytest
from fastapi import Depends, FastAPI
from fastapi.testclient import TestClient
from sqlalchemy import text
from sqlalchemy.orm import Session

from app.db import models as m
from app.db.session import get_engine

MARKET = {"city": "Los Angeles", "state": "CA"}


def _count(sql: str, **params) -> int:
    """Read through a connection of its own (what another service would see)."""
    with get_engine().connect() as conn:
        return conn.execute(text(sql), params).scalar_one()


def _projects_named(name: str) -> int:
    return _count("SELECT count(*) FROM projects WHERE data->>'project_name' = :n", n=name)


def _channels_with(handle: str) -> int:
    return _count("SELECT count(*) FROM owner_channels WHERE handle = :h", h=handle)


def _probe(app, count):
    """ASGI wrapper: when the answer starts (`http.response.start`), call `count()` and record its result.

    Deterministic for the old behaviour too: the app's BaseHTTPMiddleware hands messages on through a zero-buffer
    stream, so when the outer answer starts the route is still blocked sending its body and has not reached the
    dependency teardown that used to commit."""
    seen: list[int] = []

    async def probe(scope, receive, send):
        async def send_and_count(message):
            if message["type"] == "http.response.start":
                seen.append(count())
            await send(message)

        await app(scope, receive, send_and_count)

    return probe, seen


@pytest.fixture
def refuse_commit(db):
    """`refuse_commit(table, condition)`: PostgreSQL refuses the COMMIT (not an earlier statement) of a transaction that
    inserted a row of `table` matching the SQL `condition` on NEW, through a deferred constraint trigger. Each test
    matches only a row it names itself. Returns `remove()`, which lets such commits through again; every trigger and
    function is dropped at the end."""
    eng = get_engine()
    installed: list[tuple[str, str]] = []

    def run(*statements: str) -> None:
        db.rollback()  # the test session must not hold a lock on the table while the trigger changes
        with eng.begin() as conn:
            conn.execute(text("SET LOCAL lock_timeout = '5s'"))
            for statement in statements:
                conn.execute(text(statement))

    def drop(table: str, name: str) -> None:
        run(f"DROP TRIGGER IF EXISTS {name} ON {table}", f"DROP FUNCTION IF EXISTS {name}()")

    def install(table: str, condition: str):
        name = f"ss_test_commit_refused_{table}"
        run(
            f"CREATE OR REPLACE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS"
            " $$ BEGIN RAISE EXCEPTION 'commit refused by the test trigger'; END $$",
            f"CREATE CONSTRAINT TRIGGER {name} AFTER INSERT ON {table} DEFERRABLE INITIALLY DEFERRED"
            f" FOR EACH ROW WHEN ({condition}) EXECUTE FUNCTION {name}()",
        )
        installed.append((table, name))
        return lambda: drop(table, name)

    yield install
    for table, name in installed:
        drop(table, name)


@pytest.fixture
def sessions(db, monkeypatch):
    """The sessions `get_db` opens from here on, each as the list of its commit/rollback/close calls in order."""
    import app.db.session as session_module

    real = session_module.session_factory
    opened: list[list[str]] = []

    def factory(engine=None):
        maker = real(engine)

        def make() -> Session:
            s = maker()
            calls: list[str] = []
            opened.append(calls)
            for name in ("commit", "rollback", "close"):

                def recorded(*args, _orig=getattr(s, name), _name=name, **kwargs):
                    calls.append(_name)
                    return _orig(*args, **kwargs)

                setattr(s, name, recorded)
            return s

        return make

    monkeypatch.setattr(session_module, "session_factory", factory)
    return opened


# ---- the row exists when the answer starts ---------------------------------------------------------------------


def test_a_created_project_is_committed_before_the_answer_starts(client, auth, sessions):
    from app.main import app

    h, _ = auth
    name = f"probe-{uuid4().hex}"
    probe, seen = _probe(app, lambda: _projects_named(name))
    sessions.clear()
    r = TestClient(probe).post("/api/v1/projects", json={"project_name": name, "market": MARKET}, headers=h)
    assert r.status_code == 201, r.text
    assert seen == [1]  # before the fix: [0], the row was committed only after the answer had been sent
    assert sessions == [["commit", "close"]]


def test_an_owner_channel_is_committed_before_the_answer_starts(client, auth):
    from app.main import app

    h, _ = auth
    handle = f"probe-{uuid4().hex[:12]}@example.com"
    probe, seen = _probe(app, lambda: _channels_with(handle))
    r = TestClient(probe).put("/api/v1/owner/channel", json={"handle": handle}, headers=h)
    assert r.status_code == 200 and r.json()["channel"]["handle"] == handle, r.text
    assert seen == [1]  # before the fix: [0]


# ---- a refused commit is a non-2xx, and nothing is stored -------------------------------------------------------


def test_a_refused_commit_of_a_new_project_is_a_non_2xx_and_nothing_is_stored(
    client, auth, db, demo_project_body, refuse_commit, sessions
):
    from app.main import app

    h, _ = auth
    name = f"refused-{uuid4().hex}"
    body = {**demo_project_body, "project_name": name}  # with a CONFIRMED profile, so an event is written too
    remove = refuse_commit("projects", f"NEW.data->>'project_name' = '{name}'")
    before = {
        t: _count(f"SELECT count(*) FROM {t}")
        for t in ("projects", "business_requirements", "activity_events")
    }
    sessions.clear()
    r = TestClient(app, raise_server_exceptions=False).post("/api/v1/projects", json=body, headers=h)
    # Before the fix: 201 with the new project, while the teardown commit failed after the answer.
    assert r.status_code == 500, r.text
    problem = r.json()
    assert (
        problem["error_code"] == "INTERNAL_ERROR"
        and problem["retryable"] is True
        and "project" not in problem
    )
    assert sessions == [["commit", "rollback", "close"]]
    after = {t: _count(f"SELECT count(*) FROM {t}") for t in before}
    assert after == before and _projects_named(name) == 0  # project, profile and event: none stored

    remove()  # the same request, with commits let through again, is stored as usual
    again = client.post("/api/v1/projects", json=body, headers=h)
    assert again.status_code == 201, again.text
    assert _projects_named(name) == 1


def test_a_refused_commit_of_the_owner_channel_is_a_non_2xx_and_nothing_is_stored(
    client, auth, refuse_commit
):
    from app.main import app

    h, _ = auth
    handle = f"refused-{uuid4().hex[:12]}@example.com"
    remove = refuse_commit("owner_channels", f"NEW.handle = '{handle}'")
    r = TestClient(app, raise_server_exceptions=False).put(
        "/api/v1/owner/channel", json={"handle": handle}, headers=h
    )
    assert r.status_code == 500 and r.json()["error_code"] == "INTERNAL_ERROR", r.text  # before the fix: 200
    assert _channels_with(handle) == 0
    assert client.get("/api/v1/owner/channel", headers=h).json()["channel"] is None

    remove()
    again = client.put("/api/v1/owner/channel", json={"handle": handle}, headers=h)
    assert again.status_code == 200 and again.json()["channel"]["handle"] == handle, again.text
    assert _channels_with(handle) == 1


# ---- the session still rolls back on a handler exception, and is always closed ---------------------------------


def _writing_app(user_id, error: Exception | None) -> FastAPI:
    """A one-route app on the real `db_session` and the app's error handlers: insert a user, then raise `error`."""
    from app.api.deps import db_session
    from app.api.errors import install_error_handlers

    t = FastAPI()
    install_error_handlers(t)

    @t.post("/write")
    def write(db: Session = Depends(db_session)):
        db.add(m.User(id=user_id, display_name="rollback probe"))
        db.flush()  # the row is written inside the transaction before the handler fails
        assert db.get(m.User, user_id) is not None
        if error is not None:
            raise error
        return {"ok": True}

    return t


@pytest.mark.parametrize(
    "error, status, code",
    [
        (None, 200, None),
        ("conflict", 409, "CONFLICT"),
        ("crash", 500, "INTERNAL_ERROR"),
    ],
    ids=["success", "app-error", "unexpected-exception"],
)
def test_the_session_commits_on_success_and_rolls_back_on_a_handler_exception(
    db, sessions, error, status, code
):
    from app.core.errors import ConflictError

    uid = uuid4()
    exc = {"conflict": ConflictError("refused after a write"), "crash": RuntimeError("boom")}.get(error)
    db.rollback()
    pool = get_engine().pool
    checked_out = pool.checkedout()
    r = TestClient(_writing_app(uid, exc), raise_server_exceptions=False).post("/write")
    assert r.status_code == status, r.text
    if code:
        assert r.json()["error_code"] == code
    assert sessions == [["commit", "close"] if error is None else ["rollback", "close"]]
    assert pool.checkedout() == checked_out  # the connection went back to the pool
    assert _count("SELECT count(*) FROM users WHERE id = :u", u=uid) == (1 if error is None else 0)


# ---- routes that stream or return bytes keep working -----------------------------------------------------------


def _read_until(lines, predicate, limit: int = 100) -> list[str]:
    """Lines of the event stream up to and including the first one that satisfies `predicate`. `lines` must be one
    iterator kept by the caller: a temporary `r.iter_lines()` is closed on return, which closes the stream."""
    out: list[str] = []
    for line in lines:
        out.append(line)
        if predicate(line):
            return out
        if len(out) >= limit:
            break
    raise AssertionError(f"not found in the stream: {out}")


def _new_project(client, h, body) -> str:
    r = client.post("/api/v1/projects", json=body, headers=h)
    assert r.status_code == 201, r.text
    return r.json()["project"]["project_id"]


def test_the_events_stream_still_streams_stored_and_new_events(client, auth, demo_project_body):
    h, _ = auth
    pid = _new_project(client, h, demo_project_body)  # emits PROJECT_PROFILE_CONFIRMED
    with client.stream("GET", f"/api/v1/projects/{pid}/events/stream", headers=h) as r:
        assert r.status_code == 200
        assert r.headers["content-type"].startswith("text/event-stream")
        lines = r.iter_lines()
        first = _read_until(lines, lambda line: line.startswith("data: "))
        assert first[-3:-1] == ["id: 1", "event: project.profile.confirmed"], first
        _read_until(lines, lambda line: line == ": keepalive")
        # A new event written while the stream is open still arrives (the stream polls with sessions of its own).
        put = client.put(
            f"/api/v1/projects/{pid}/business-profile", json=demo_project_body["business_profile"], headers=h
        )
        assert put.status_code == 200, put.text
        second = _read_until(lines, lambda line: line.startswith("data: "), limit=200)
        assert second[-3:-1] == ["id: 2", "event: project.profile.confirmed"], second


def test_the_events_stream_holds_no_database_connection_while_it_waits(client, auth, demo_project_body):
    h, _ = auth
    pid = _new_project(client, h, demo_project_body)
    with client.stream("GET", f"/api/v1/projects/{pid}/events/stream", headers=h) as r:
        assert r.status_code == 200
        lines = r.iter_lines()  # kept: dropping the iterator would close the stream and cancel the route
        _read_until(lines, lambda line: line == ": keepalive")
        # The route's own session (the ownership check) is already committed and closed, and the stream closes its
        # poll session before each keepalive. Before the fix the ownership-check session stayed checked out, idle in
        # a transaction, for as long as the stream was open (up to an hour): one pool connection per open stream.
        assert get_engine().pool.checkedout() == 0


def test_a_media_route_still_answers_with_its_bytes(client, container, auth, demo_project_body, monkeypatch):
    h, _ = auth
    pid = _new_project(client, h, demo_project_body)
    assert client.post(f"/api/v1/projects/{pid}/properties/search", json={}, headers=h).status_code == 202
    props = client.get(f"/api/v1/projects/{pid}/properties", headers=h).json()
    prop = next(p for p in props if p["property_identity"]["latitude"] is not None)
    jpeg = b"\xff\xd8\xff\xe0" + b"fixture-test-image" * 8
    monkeypatch.setattr(
        container.vexcel, "ortho_image", lambda _lat, _lon: jpeg
    )  # fixture mode has no imagery
    r = client.get(f"/api/v1/media/vexcel/ortho/{prop['property_id']}")
    assert r.status_code == 200 and r.content == jpeg
    assert r.headers["content-type"] == "image/jpeg"
    assert r.headers["cache-control"] == "private, max-age=86400"
    missing = client.get(f"/api/v1/media/vexcel/ortho/{uuid4()}")
    assert missing.status_code == 404 and missing.json()["error_code"] == "NOT_FOUND"
