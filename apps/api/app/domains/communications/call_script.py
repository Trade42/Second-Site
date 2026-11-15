"""What the assistant asks a broker, and how answers become structured findings.

Questions are grounded in the property's open feasibility assessments (UNKNOWN / MAY_REQUIRE_WORK), so every call
exists to resolve a specific unknown. Extraction is deterministic and conservative: an answer that does not clearly
state a value yields a finding with value=None ("unclear"), never a guessed number.
"""

from __future__ import annotations

import bisect
import re
from typing import Any

from app.contracts import AssessmentStatus, CallQuestion, InfrastructureAssessment, StructuredFinding

QUESTION_BANK: list[tuple[str, CallQuestion]] = [
    (
        "electrical.tenant_service",
        CallQuestion(
            question="How many amps is the electrical service to the suite's own panel, and roughly how much is in use?",
            claim_type="tenant_subpanel_amps",
        ),
    ),
    (
        "plumbing.treatment_sinks",
        CallQuestion(
            question="Is there existing plumbing inside the suite, like a sink or a floor drain?",
            claim_type="in_unit_plumbing",
        ),
    ),
    (
        "hvac.ventilation",
        CallQuestion(
            question="Could the HVAC be zoned so that separate small rooms each get supply and return air?",
            claim_type="hvac_zonable",
        ),
    ),
    (
        "space.min_area",
        CallQuestion(
            question="What is the usable square footage of the suite?", claim_type="usable_area_sqft"
        ),
    ),
]
AVAILABILITY = CallQuestion(
    question="When would the space be available to start a lease?", claim_type="available_from"
)
MAX_QUESTIONS = 4

# The same alternatives in the same order as before, grouped: 1 interjections, 2 predicates, 3 adverbs.
YES = re.compile(
    r"\b(?:(yes|yeah|yep)|(there is|there's|it has|it does|we have)|(definitely|absolutely|correct|sure))\b",
    re.I,
)
NO = re.compile(
    r"\b(no|nope|none|not really|there isn'?t|doesn'?t|don'?t have|it's all open|no plumbing)\b", re.I
)
UNSURE = re.compile(
    r"\b(not sure|don'?t know|no idea|would have to check|have to check|unsure|i'?ll find out)\b", re.I
)
AMPS = re.compile(r"(\d{2,4})\s*(?:-|\s)?(?:amps?|amperes?|a\b)", re.I)
SQFT = re.compile(r"(\d[\d,]{2,6})\s*(?:usable\s*)?(?:sq(?:uare)?\.?\s*(?:ft|feet)|sf\b)", re.I)

# Truth rules for extract. The Rust port (apps/backend/crates/domain/src/communications/call_script.rs)
# writes the same rules the same way; gen_domain4.py vectors compare the two answer by answer.
# Characters: a word character is str.isalnum() or "_" (re's \w), a space is str.isspace(), a digit
# that counts as part of a reported number is ASCII 0-9 only (any other Unicode decimal next to a
# number makes it unclear), and the apostrophes below are read as "'" before any pattern runs.
# Read as "'": right and left single quotation marks, modifier letter apostrophe, and the look-alikes
# grave accent, acute accent, prime and fullwidth apostrophe ("isn´t", "doesn`t").
APOSTROPHES = {0x2019: "'", 0x2018: "'", 0x2BC: "'", 0x60: "'", 0xB4: "'", 0x2032: "'", 0xFF07: "'"}
WORD = re.compile(r"\w+(?:'\w+)*")
SPACES = re.compile(r"\s+")  # every run of spaces is read as one " ", so "have  to check" is a hedge
NO_IDIOM = re.compile(
    r"\b(?:no|not\s+an?)\s+(?:\w+\s+)?(?:problems?|issues?|worries|doubts?|questions?|reason)\b", re.I
)
# An area stated as rentable or gross is not the usable area the question asks for.
NOT_USABLE = re.compile(r"\b(?:rentable|gross|rsf|gsf)\b", re.I)
NEGATIONS = frozenset(
    "not no nope none never nor neither without cannot hardly barely scarcely nothing nobody nowhere "
    "zero 0 nah nay nil nada zilch negative impossible incorrect lack lacks lacking absent missing".split()
)
# Contractions written without an apostrophe; any word ending in "n't" is one too.
CONTRACTIONS = frozenset(
    "aint arent cant couldnt didnt doesnt dont hadnt hasnt havent isnt mightnt mustnt neednt shant shouldnt "
    "wasnt werent wont wouldnt".split()
)
# Words that negate the affirmation right after them (so does every contraction).
NEGATE_BEFORE = frozenset({"not", "never", "cannot", "hardly", "barely", "scarcely"})
NEGATE_AFTER_ADVERB = frozenset({"not", "never"})  # after definitely, absolutely, correct, sure
# A predicate followed by these is an auxiliary: "it has to be added", "it has been removed", "we have yet to"
PREDICATE_AUX = frozenset({"to", "been", "yet"})
# A bound, not the value: "less than 100 amps", "at least 1,000 sf", "under 200 amps", "200 amps or more"
BOUND_BEFORE = frozenset("under over below above than least most max maximum min minimum".split())
BOUND_AFTER = frozenset("max maximum min minimum plus".split())
BOUND_AFTER_OR = frozenset("more less fewer greater higher lower above below over under".split())
NUMBER_OPENERS = '([{"\u201c~:'  # allowed directly before a number, besides a space
# Join a number to an earlier one: hyphens, dashes, minus sign, ~ / , . : + &
NUMBER_JOINERS = "-\u2010\u2011\u2012\u2013\u2014\u2015\u2212~/,.:+&"
JOIN_WORDS_BEFORE = frozenset({"to", "and", "or", "through", "thru", "till", "until", "vs", "versus", "of"})
JOIN_WORDS_AFTER = frozenset({"to", "or", "through", "thru", "till", "until"})


def build_script(assessments: list[InfrastructureAssessment]) -> list[CallQuestion]:
    open_ids = {
        a.requirement_id.split(".", 1)[1]: a.requirement_id
        for a in assessments
        if a.status in (AssessmentStatus.UNKNOWN, AssessmentStatus.MAY_REQUIRE_WORK)
        and "." in a.requirement_id
    }
    script = [
        q.model_copy(update={"requirement_id": open_ids[suffix]})
        for suffix, q in QUESTION_BANK
        if suffix in open_ids
    ][: MAX_QUESTIONS - 1]
    return [*script, AVAILABILITY]


def _is_word(c: str) -> bool:
    return c.isalnum() or c == "_"


def _folded(w: str) -> str:
    """ASCII lower case; a word with any non-ASCII character is never one of the listed words."""
    return w.lower() if w.isascii() else ""


def _is_contraction(w: str) -> bool:
    return (len(w) >= 3 and w[-3] in "nN" and w[-2] == "'" and w[-1] in "tT") or _folded(w) in CONTRACTIONS


def _is_negation(w: str) -> bool:
    """A listed negation, alone or before a clitic ("nothing's", "nobody'll"), or a contraction."""
    return _folded(w.split("'")[0]) in NEGATIONS or _is_contraction(w)


def _skip_space_back(text: str, i: int) -> int:
    while i > 0 and text[i - 1].isspace():
        i -= 1
    return i


def _skip_space_fwd(text: str, i: int) -> int:
    while i < len(text) and text[i].isspace():
        i += 1
    return i


def _affirmed(text: str, m: re.Match[str], ending: dict[int, str], starting: dict[int, str]) -> bool:
    """A YES match is an affirmation unless a negation is next to it, separated by spaces only: before it
    "not", "never", "cannot", "hardly", "barely", "scarcely" or a contraction ("isn't correct"); after a
    predicate (there is, there's, it has, it does, we have) any negation word ("it does not", "there's no");
    after an adverb (definitely, absolutely, correct, sure) "not" or "never" ("absolutely not").
    `ending` / `starting` map each word's end / start offset to the word."""
    i = _skip_space_back(text, m.start())
    before = ending.get(i) if i < m.start() else None
    j = _skip_space_fwd(text, m.end())
    after = starting.get(j) if j > m.end() else None
    if before is not None and (_folded(before) in NEGATE_BEFORE or _is_contraction(before)):
        return False
    if m.group(2) and after is not None and (_is_negation(after) or _folded(after) in PREDICATE_AUX):
        return False
    if m.group(3) and after is not None and _folded(after) in NEGATE_AFTER_ADVERB:
        return False
    return True


def _negation_before_no(text: str, negations: list[tuple[int, int]], noes: list[tuple[int, int]]) -> bool:
    """A negation word outside every NO match that comes before a NO match in the same clause (no punctuation
    between them): a double negative such as "it's not that there isn't a sink". Punctuation is any character
    that is not a word character, a space or "'". NO matches are sorted and do not overlap, so only the last NO
    match starting at or before the word can contain it, and only the first one after it can be in its clause."""
    no_starts = [ns for ns, _ in noes]
    breaks = [i for i, c in enumerate(text) if not (_is_word(c) or c.isspace() or c == "'")]
    for s, e in negations:
        k = bisect.bisect_right(no_starts, s) - 1
        if k >= 0 and e <= noes[k][1]:
            continue
        k = bisect.bisect_left(no_starts, e)
        if k < len(noes):
            b = bisect.bisect_left(breaks, e)
            if b == len(breaks) or breaks[b] >= no_starts[k]:
                return True
    return False


def _yes_no(text: str) -> bool | None:
    words = [(m.start(), m.end(), m.group()) for m in WORD.finditer(text)]
    ending = {e: w for _, e, w in words}
    starting = {s: w for s, _, w in words}
    noes = [m.span() for m in NO.finditer(text)]
    affirmed = any(_affirmed(text, m, ending, starting) for m in YES.finditer(text))
    negations = [(s, e) for s, e, w in words if _is_negation(w)]
    if affirmed and not noes and not negations:
        return True
    if noes and not affirmed and not NO_IDIOM.search(text) and not _negation_before_no(text, negations, noes):
        return False
    return None


def _plain_number(digits: str) -> bool:
    """ASCII digits, with commas only as thousands separators ("1,128"; not "11,28", "1,1280" or "1,,128")."""
    groups = digits.split(",")
    if not all(g and all(c in "0123456789" for c in g) for g in groups):
        return False
    return len(groups) == 1 or (len(groups[0]) <= 3 and all(len(g) == 3 for g in groups[1:]))


def _stands_alone(text: str, start: int, end: int, starts: set[int]) -> bool:
    """The number at text[start:] (its match ends at `end`) is a whole token, not part of a longer number or a
    range: the character before it is a space, the start, or one of NUMBER_OPENERS; and no digit is joined to
    it on the left by spaces alone ("1 128"), by one of NUMBER_JOINERS ("200-400", "200, 400") or by one of
    JOIN_WORDS_BEFORE ("200 to 400 amps"), nor on the right by one of JOIN_WORDS_AFTER, unless that digit
    starts another unit match (in `starts`), which the caller compares ("200 amps or 400")."""
    if start > 0 and not (text[start - 1].isspace() or text[start - 1] in NUMBER_OPENERS):
        return False
    k = _skip_space_back(text, start)
    if k > 0 and text[k - 1] in NUMBER_JOINERS:
        k = _skip_space_back(text, k - 1)
    else:
        j = k
        while j > 0 and _is_word(text[j - 1]):
            j -= 1
        if _folded(text[j:k]) in JOIN_WORDS_BEFORE:
            k = _skip_space_back(text, j)
    if k > 0 and text[k - 1].isdecimal():
        return False
    i = _skip_space_fwd(text, end)
    j = i
    while j < len(text) and _is_word(text[j]):
        j += 1
    if i > end and _folded(text[i:j]) in JOIN_WORDS_AFTER:
        i = _skip_space_fwd(text, j)
        if i < len(text) and text[i].isdecimal() and i not in starts:
            return False
    return True


def _word_at(text: str, i: int, step: int) -> tuple[str, int]:
    """The listed form of the word right before (step -1) or after (step 1) offset i, spaces only between,
    and the offset past it."""
    i = _skip_space_back(text, i) if step < 0 else _skip_space_fwd(text, i)
    j = i
    while (j > 0 and _is_word(text[j - 1])) if step < 0 else (j < len(text) and _is_word(text[j])):
        j += step
    return _folded(text[min(i, j) : max(i, j)]), j


def _qualified(text: str, m: re.Match[str], negation_ends: list[int], breaks: list[int]) -> bool:
    """The unit match is negated, a bound or an article: a negation word earlier in its clause ("it's not
    1,500 sf", "not even 100 amps"); a bound word next to it ("less than", "at least", "under", "200 amps or
    more"); or the unit as a lower-case "a" after a space and before a word ("in 2015 a while back").
    `negation_ends` (the end offsets of the negation words) and `breaks` (the clause breaks) are sorted."""
    k = bisect.bisect_right(negation_ends, m.start()) - 1
    if k >= 0:
        b = bisect.bisect_left(breaks, negation_ends[k])
        if b == len(breaks) or breaks[b] >= m.start():
            return True
    after, j = _word_at(text, m.end(), 1)
    if _word_at(text, m.start(), -1)[0] in BOUND_BEFORE or after in BOUND_AFTER:
        return True
    if after == "or" and _word_at(text, j, 1)[0] in BOUND_AFTER_OR:
        return True
    g, i = m.group(), _skip_space_fwd(text, m.end())
    return g[-1] == "a" and g[-2] == " " and i < len(text) and _is_word(text[i])


def _quantity(pattern: re.Pattern[str], text: str) -> int | None:
    """The one value every unit match states; None if any match is not a whole plain number or is qualified
    (_qualified), if the matches disagree, or if the value is zero. Group 1 (the digits) starts each match."""
    matches = list(pattern.finditer(text))
    starts = {m.start() for m in matches}
    negation_ends = [m.end() for m in WORD.finditer(text) if _is_negation(m.group())]
    breaks = [i for i, c in enumerate(text) if not (_is_word(c) or c.isspace() or c == "'")]
    values: set[int] = set()
    for m in matches:
        digits = m.group(1)
        if not (_plain_number(digits) and _stands_alone(text, m.start(), m.end(), starts)):
            return None
        if _qualified(text, m, negation_ends, breaks):
            return None
        values.add(int(digits.replace(",", "")))
        if len(values) > 1:
            return None
    return values.pop() if len(values) == 1 and 0 not in values else None


def extract(question: CallQuestion, answer: str) -> StructuredFinding:
    """One finding; value None ("unclear") unless the answer clearly states one.

    - Typographic apostrophes are read as "'" and every run of spaces as one " "; any hedge (UNSURE) leaves
      every claim unclear.
    - Amps and area: every unit match must be a whole plain number (_plain_number, _stands_alone), all of
      them the same non-zero value; otherwise unclear. No other bounds than the patterns' own.
    - Yes/no: True only for an affirmation that is not negated (_affirmed), no NO match and no negation word
      anywhere; False only for a NO match with no un-negated affirmation, no "no problem"-style idiom and no
      double negative (_negation_before_no); otherwise unclear.
    - Any other claim: the stripped answer's first 200 characters (as given), or None when empty.
    """
    value: Any = None
    unit: str | None = None
    text = SPACES.sub(" ", answer.translate(APOSTROPHES))
    if not UNSURE.search(text):
        if question.claim_type == "tenant_subpanel_amps":
            n = _quantity(AMPS, text)
            if n is not None:
                value, unit = n, "A"
        elif question.claim_type == "usable_area_sqft":
            n = None if NOT_USABLE.search(text) else _quantity(SQFT, text)
            if n is not None:
                value, unit = n, "sqft"
        elif question.claim_type in ("in_unit_plumbing", "hvac_zonable"):
            value = _yes_no(text)
        else:
            value = answer.strip()[:200] or None
    return StructuredFinding(
        question=question.question,
        answer=answer,
        claim_type=question.claim_type,
        value=value,
        unit=unit,
        speaker="counterparty",
        classification="OWNER_REPORTED",
    )


def describe(f: StructuredFinding) -> str:
    if f.value is None:
        return f"{f.claim_type}: unclear"
    if isinstance(f.value, bool):
        return f"{f.claim_type}: {'yes' if f.value else 'no'}"
    return f"{f.claim_type}: {f.value}{(' ' + f.unit) if f.unit else ''}"
