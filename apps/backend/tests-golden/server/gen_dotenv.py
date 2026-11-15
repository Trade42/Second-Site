"""Golden vectors for the `.env` reader of the Rust settings (`crates/server/src/config.rs`,
`dotenv_values` and `read_dotenv`): what python-dotenv, which pydantic-settings uses for
`env_file`, reads.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_dotenv.py)
Output: dotenv_cases.json, {"dotenv_version", "cases", "environ", "files"}:
- `cases`: [{"line", "values"}], `values` is `dotenv_values(stream=StringIO(line + "\n"))` of
  that line alone ({} when the line binds nothing; a key without `=` maps to null). The cases are
  the hand-written LINES, then every binding line of the repository's `.env.example` as it was
  when this ran. None of them contains `${`, so the environment does not matter for them.
- `files`: [{"name", "text", "values"}], `values` is `dotenv_values(path, encoding="utf-8")` (the
  call pydantic-settings makes, through a real file, so `open()` translates `\r\n` and a lone
  `\r` to `\n`) as a list of [key, value-or-null] pairs in the dict's order, with the process
  environment replaced by `environ` while it runs (interpolation reads `os.environ`). The files
  are the hand-written FILES, then the whole `.env.example`.
"""

from __future__ import annotations

import io
import json
import os
import tempfile
from pathlib import Path

from importlib.metadata import version

from dotenv import dotenv_values

HERE = Path(__file__).resolve().parent

LINES = [
    # Values and inline comments (the .env.example shape first).
    "PHOTON_API_KEY=      # comment",
    "KEY=\t# comment after a tab",
    "KEY= #",
    "KEY=",
    "KEY= ",
    "KEY=#novalue",
    "KEY=#novalue # comment",
    "KEY=value",
    "KEY=value # comment",
    "KEY=value#notcomment",
    "KEY=value\t#tab comment",
    "KEY=a b c # x",
    "KEY=a #b #c",
    "KEY=a  #b",
    "KEY=trailing spaces   ",
    "KEY=http://localhost:8001/#frag",
    "KEY=a=b",
    "KEY=value  ",
    # Quoted values.
    'KEY="quoted value"',
    'KEY="quoted # not a comment"',
    'KEY="quoted" # comment',
    'KEY="quoted"# comment',
    "KEY='single'",
    "KEY='single # x' # y",
    'KEY="esc \\n \\t \\\\ \\" \\\' end"',
    "KEY='esc \\n \\' \\\\ end'",
    'KEY=""',
    'KEY="" # c',
    "KEY=''",
    'KEY= "spaced quoted"',
    'KEY="a" junk',
    'KEY="unterminated',
    "KEY='unterminated",
    'KEY=un"quoted"',
    # Keys, export, whitespace, comments.
    "export KEY=exported",
    "export   KEY=exported2",
    "exportKEY=x",
    "'QKEY'=v",
    "KEY = spaced",
    "  KEY=indented",
    "KEY",
    "# full comment",
    "   # indented comment",
    "=novalue",
    "KEY#x=1",
]


# The process environment while the files are read (`${NAME}` falls back to it).
ENVIRON = {
    "DB_USER": "env_user",
    "HOME_DIR": "/home/env",
    "EMPTY_NAME": "",
    "FROM_ENV": "from the environment",
    "SELF": "e",
    "lower_env": "lower",
}

# (name, text): shapes that need the whole file (interpolation, line breaks, multi-line values).
FILES = [
    # Interpolation: earlier values of the file, then the environment.
    ("value from the file", "DB_USER=alice\nDATABASE_URL=postgresql://${DB_USER}@localhost:5432/db\n"),
    ("value from the environment", "DATABASE_URL=postgresql://${DB_USER}@localhost:5432/db\n"),
    ("double quoted", 'URL="postgresql://${DB_USER}@h/db"\n'),
    ("single quoted", "Q='${DB_USER} and ${UNSET_NAME}  '\n"),
    ("unset is empty", "A=${UNSET_NAME}\n"),
    ("default", "A=${UNSET_NAME:-fallback}\nB=\"${UNSET_NAME:-two words}\"\nC='${UNSET_NAME:-x}'\n"),
    ("set but empty keeps empty", "A=${EMPTY_NAME:-fallback}\n"),
    ("default when set", "A=${HOME_DIR:-fallback}\n"),
    ("in the middle", "A=${HOME_DIR}/sub/${DB_USER}.txt\n"),
    ("no braces", "A=$HOME_DIR\nB=$\nC=$$\n"),
    ("unclosed", "A=${HOME_DIR\nB=x${\n"),
    ("colon without dash", "A=${HOME_DIR:x}\nB=${HOME_DIR:}\n"),
    ("empty name", "A=${}\nB=${:-d}\n"),
    ("colon in the default", "A=${UNSET_NAME:-a:b}\n"),
    ("nested is not nested", "A=${UNSET_NAME:-${HOME_DIR}}\n"),
    ("escaped dollar", "A=\\${HOME_DIR}\nB=\"\\${HOME_DIR}\"\n"),
    ("the file wins over the environment", "FROM_ENV=from the file\nB=${FROM_ENV}\n"),
    ("only earlier lines", "B=${FROM_ENV}\nFROM_ENV=from the file\nC=${FROM_ENV}\n"),
    ("a key without a value", "A\nB=${A:-default}\nC=${A}\n"),
    ("the last value", "A=1\nA=2\nB=${A}\n"),
    ("names are case-sensitive", "a=lower\nB=${A}\nC=${a}\nD=${lower_env}\nE=${LOWER_ENV}\n"),
    ("self reference", "SELF=${SELF}x\nSELF=${SELF}y\n"),
    ("twice", "A=${HOME_DIR}${HOME_DIR}\n"),
    ("before a comment", "A=pre ${HOME_DIR} # comment\n"),
    ("unicode name", "A=${\u00dcNAME:-\u00fc}\n"),
    ("export", "export A=${UNSET_NAME:-x}\n"),
    ("a value made by interpolation is not expanded again", "A=${B}\nB=$${HOME_DIR}\nC=${B}\n"),
    ("a value containing a reference", "A=\"${\"\nB=${A}{HOME_DIR}\n"),
    # Line breaks.
    ("lone CR", "A=1\rB=2\rC=three words\r"),
    ("CRLF", "A=1\r\nB=2\r\n"),
    ("CR inside quotes", 'A="x\ry"\nB=\'p\rq\'\n'),
    ("mixed", 'A=1\r\nB="multi\r\nline"\rC=3'),
    ("CR then comment", "A=1 # c\rB=2\r"),
    # Quoted values that span lines.
    ("double quoted over two lines", 'ML="first\nOTHER=second"\nNEXT=1\n'),
    ("single quoted over two lines", "ML='first\nsecond'\nNEXT=1\n"),
    ("then a comment", 'ML="first\nsecond" # c\nNEXT=1\n'),
    ("never closed", 'ML="unterminated\nOTHER=x\n'),
    ("never closed, single", "ML='unterminated\nOTHER=x\n"),
    ("escaped quote then a line break", 'ML="a\\"b\nc"\nNEXT=1\n'),
    ("escapes over lines", 'ML="a\\n\nb\\t"\n'),
    ("quoted key over two lines", "'MULTI\nKEY'=v\nNEXT=1\n"),
    ("junk after a value", 'A="x" junk\nB=2\n'),
    ("junk after a multi-line value", 'A="x\ny" junk\nB=2\n'),
    ("interpolated multi-line value", 'A="${HOME_DIR}\n${DB_USER}"\n'),
    # Duplicates, BOM, whitespace, ends of file.
    ("duplicate keeps the first place", "A=1\nB=2\nA=3\n"),
    ("a later key without a value", "A=1\nA\nB=2\n"),
    ("case-differing keys", "A=1\na=2\nA=3\n"),
    ("BOM", "\ufeffA=1\nB=2\n"),
    ("information separators are whitespace", "\x1cA=1\nB=a\x1d#c\nC=b\x1e\nD=c\x1f d\n"),
    ("other unicode whitespace", "A=a\u00a0#c\nB=b\u2003\n\u3000C=3\n"),
    ("no final line break", "A=1\nB=2"),
    ("empty", ""),
    ("only whitespace", "\n\n  \n\t\n"),
    ("comments", "\n# comment\nA=1 # c\n  # indented\nB=2\n"),
    ("export without a key", "export \nA=1\n"),
    ("equals first", "=x\nA=1\n"),
]


def example_lines() -> list[str]:
    # Every binding line of the repository's .env.example, copied into the vectors so the test
    # does not depend on that file staying as it is.
    path = HERE.parents[3] / ".env.example"
    if not path.exists():
        return []
    return [line for line in path.read_text().splitlines() if line.strip() and not line.lstrip().startswith("#")]


def read_files(files: list[tuple[str, str]]) -> list[dict]:
    """`dotenv_values(path, encoding="utf-8")` of each text, written as UTF-8 bytes to a real file,
    with the process environment set to ENVIRON."""
    out = []
    saved = dict(os.environ)
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / ".env"
        try:
            os.environ.clear()
            os.environ.update(ENVIRON)
            for name, text in files:
                path.write_bytes(text.encode("utf-8"))
                values = dotenv_values(path, encoding="utf-8")
                out.append({"name": name, "text": text, "values": [[k, v] for k, v in values.items()]})
        finally:
            os.environ.clear()
            os.environ.update(saved)
    return out


def main() -> None:
    cases = []
    for line in LINES + example_lines():
        values = dotenv_values(stream=io.StringIO(line + "\n"))
        cases.append({"line": line, "values": dict(values)})
    example = HERE.parents[3] / ".env.example"
    files = FILES + ([(".env.example", example.read_text())] if example.exists() else [])
    out = HERE / "dotenv_cases.json"
    doc = {
        "dotenv_version": version("python-dotenv"),
        "cases": cases,
        "environ": ENVIRON,
        "files": read_files(files),
    }
    out.write_text(json.dumps(doc, indent=1, ensure_ascii=False) + "\n")
    print(
        f"wrote {out.name}: {len(cases)} line cases, {len(doc['files'])} files "
        f"(python-dotenv {version('python-dotenv')})"
    )


if __name__ == "__main__":
    main()
