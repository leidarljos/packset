#!/usr/bin/env python3
"""Answer accuracy on LongMemEval_S from the harness's retrieval dump.

Two measurements, one seam. `examples/longmemeval` ranks sessions and, with
`PACKSET_LME_DUMP` set, writes one JSON line a question with the session ids
each arm retrieved. This script hands the top sessions of one arm to a reader
model with the benchmark's own reading prompt, then asks a judge model the
benchmark's own type-specific question, and reports accuracy by type. The
reader and the judge are any OpenAI-compatible chat endpoint; the report
names them, because the number is theirs as much as the retriever's.

    export QA_BASE_URL=https://.../v1 QA_API_KEY=... QA_MODEL=...
    python3 scripts/longmemeval_qa.py longmemeval_s.json dump.jsonl --arm "sessions fused" --top 5

The prompts are LongMemEval's (doi:10.48550/arXiv.2410.10813), reproduced
from its evaluation code so a row here is read the way the paper's rows are.
An `oracle` arm hands the reader the labelled answer sessions: the ceiling
the retriever is measured against.
"""
import argparse
import concurrent.futures as cf
import json
import math
import os
import sys
import urllib.request

READ = (
    "I will give you several history chats between you and a user. Please answer "
    "the question based on the relevant chat history.\n\n\nHistory Chats:\n\n{}\n\n"
    "Current Date: {}\nQuestion: {}\nAnswer:"
)
SESSION = "\n### Session {}:\nSession Date: {}\nSession Content:\n{}\n"

# The seat's own reading of time, handed to the reader: every session dated
# as a distance from the question, and the rule the pack applies to facts
# that supersede each other. A memory that knows when things happened does
# the date arithmetic; a 7B reader given raw timestamps mostly cannot.
TIMELINE_NOTE = (
    "The sessions are in date order, each marked with how many days before the "
    "question it happened. A later session supersedes an earlier one on the same "
    "fact. Use the marked distances for any question about when or how long ago.\n\n"
)
SESSION_TIMED = (
    "\n### Session {}:\nSession Date: {} ({} days before the question)\nSession Content:\n{}\n"
)
# The arithmetic done for the reader: every pair of retrieved sessions and
# the days between them. A question that asks how long after one event
# another came is answered by one of these lines once the reader has
# placed the two events in their sessions.
GAPS_NOTE = "The days between each pair of sessions are listed after the sessions; use them for any question about how long between two events.\n\n"
GAPS_HEAD = "\n### Days between sessions\n"


# MemoryAgentBench: the retrieved chunks or facts of one record, each with
# its position, and the pack's rule that a later position supersedes an
# earlier one on the same fact. The answer is scored by substring match
# after normalisation, as the benchmark scores it; its LongMemEval rows are
# judged with LongMemEval's prompts.
MAB_READ = (
    "Below are memory entries retrieved from one long record for a question. Each "
    "entry is labelled with its position in the record; a later position supersedes "
    "an earlier one on the same fact.\n\n{}\n{}Question: {}\nAnswer with the answer "
    "only, no explanation.\nAnswer:"
)
MAB_ENTRY = "[position {}] {}\n"


def normal(text):
    return " ".join("".join(c if c.isalnum() else " " for c in text.lower()).split())


def substring_match(answers, response):
    r = normal(response)
    return any(normal(a) and normal(a) in r for a in answers)


def mab_fill(rows, data_dir):
    """A dump without the dataset's words (the package copy) takes its
    questions and answers back from the split files by (split, row, index)."""
    if not data_dir or all("question" in r for r in rows):
        return rows
    splits = {}
    for r in rows:
        if "question" in r:
            continue
        s = r["split"]
        if s not in splits:
            with open(os.path.join(data_dir, s + ".jsonl")) as f:
                splits[s] = [json.loads(l) for l in f if l.strip()]
        rec = splits[s][r["row"]]
        r["question"] = rec["questions"][r["question_index"]]
        a = rec["answers"][r["question_index"]]
        r["answers"] = a if isinstance(a, list) else [a]
    return rows


def mab_rows(rows, chunks_dir, arm, top):
    """One prompt per question from the harness's dump and chunk store."""
    stores = {}
    for r in rows:
        key = f"{r['split']}-{r['row']}"
        if key not in stores:
            with open(os.path.join(chunks_dir, key + ".json")) as f:
                stores[key] = json.load(f)
        chunks = stores[key]
        picked = r["retrieved"].get(arm)
        if picked is None:
            continue
        entries = "".join(MAB_ENTRY.format(i, chunks[i]) for i in picked[:top])
        date = f"Current Date: {r['question_date']}\n" if r.get("question_date") else ""
        yield {
            "question_id": f"{key}-{r['question_index']}",
            "question_type": r["source"],
            "lme_type": r.get("question_type", ""),
            "question": r["question"],
            "answers": r["answers"],
            "prompt": MAB_READ.format(entries, date, r["question"]),
        }


# The reader's context, in characters: about four per token, leaving room
# for the prompt and the answer under a 32k-token slot. LongMemEval trims
# its history the same way (max_retrieval_length); here the longest
# sessions give up their tails first, so every retrieved session stays.
HISTORY_CHARS = 100_000


def fit(parts):
    """Trim the longest parts from the end until the whole fits the budget."""
    parts = list(parts)
    total = sum(len(p) for p in parts)
    while total > HISTORY_CHARS and parts:
        i = max(range(len(parts)), key=lambda k: len(parts[k]))
        cut = min(len(parts[i]) - 500, total - HISTORY_CHARS)
        if cut <= 0:
            break
        parts[i] = parts[i][: len(parts[i]) - cut] + "\n[... trimmed to fit ...]\n"
        total = sum(len(p) for p in parts)
    return parts


def days_of(date):
    """Days since the epoch of a benchmark date, `2023/05/20 (Sat) 02:21`."""
    import datetime

    try:
        head = date.split(" (")[0]
        tail = date.split(") ")[-1] if ") " in date else "00:00"
        y, m, d = (int(x) for x in head.split("/"))
        hh, mm = (int(x) for x in tail.split(":")[:2])
        return (datetime.datetime(y, m, d, hh, mm) - datetime.datetime(1970, 1, 1)).total_seconds() / 86400
    except (ValueError, IndexError):
        return None


def days_before(question_date, session_date):
    a, b = days_of(question_date), days_of(session_date)
    if a is None or b is None:
        return None
    return max(0, int(round(a - b)))

JUDGE_BASE = (
    "I will give you a question, a correct answer, and a response from a model. "
    "Please answer yes if the response contains the correct answer. Otherwise, "
    "answer no. If the response is equivalent to the correct answer or contains "
    "all the intermediate steps to get the correct answer, you should also answer "
    "yes. If the response only contains a subset of the information required by "
    "the answer, answer no. \n\nQuestion: {}\n\nCorrect Answer: {}\n\nModel Response: {}\n\n"
    "Is the model response correct? Answer yes or no only."
)
JUDGE_TEMPORAL = (
    "I will give you a question, a correct answer, and a response from a model. "
    "Please answer yes if the response contains the correct answer. Otherwise, "
    "answer no. If the response is equivalent to the correct answer or contains "
    "all the intermediate steps to get the correct answer, you should also answer "
    "yes. If the response only contains a subset of the information required by "
    "the answer, answer no. In addition, do not penalize off-by-one errors for the "
    "number of days. If the question asks for the number of days/weeks/months, etc., "
    "and the model makes off-by-one errors (e.g., predicting 19 days when the answer "
    "is 18), the model's response is still correct. \n\nQuestion: {}\n\nCorrect "
    "Answer: {}\n\nModel Response: {}\n\nIs the model response correct? Answer yes or no only."
)
JUDGE_UPDATE = (
    "I will give you a question, a correct answer, and a response from a model. "
    "Please answer yes if the response contains the correct answer. Otherwise, "
    "answer no. If the response contains some previous information along with an "
    "updated answer, the response should be considered as correct as long as the "
    "updated answer is the required answer.\n\nQuestion: {}\n\nCorrect Answer: {}\n\n"
    "Model Response: {}\n\nIs the model response correct? Answer yes or no only."
)
JUDGE_PREFERENCE = (
    "I will give you a question, a rubric for desired personalized response, and "
    "a response from a model. Please answer yes if the response satisfies the "
    "desired response. Otherwise, answer no. The model does not need to reflect "
    "all the points in the rubric. The response is correct as long as it recalls "
    "and utilizes the user's personal information correctly.\n\nQuestion: {}\n\n"
    "Rubric: {}\n\nModel Response: {}\n\nIs the model response correct? Answer yes or no only."
)


def judge_prompt(kind, question, answer, response):
    if kind == "temporal-reasoning":
        t = JUDGE_TEMPORAL
    elif kind == "knowledge-update":
        t = JUDGE_UPDATE
    elif kind == "single-session-preference":
        t = JUDGE_PREFERENCE
    else:
        t = JUDGE_BASE
    return t.format(question, answer, response)


def chat(base, key, model, prompt, max_tokens):
    body = json.dumps({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0,
    }).encode()
    req = urllib.request.Request(
        f"{base.rstrip('/')}/chat/completions",
        data=body,
        headers={"content-type": "application/json", "authorization": f"Bearer {key}"},
    )
    with urllib.request.urlopen(req, timeout=600) as r:
        out = json.load(r)
    return out["choices"][0]["message"]["content"].strip()


LOCOMO_READ = (
    "Below are excerpts of a conversation between two people, each with the date "
    "of the session it comes from. Answer the question briefly, based only on the "
    "excerpts.\n\nExcerpts:\n\n{}\n\nQuestion: {}\nAnswer:"
)


# Test-time learning on LoCoMo: the questions of one conversation are
# answered in order, and every turn the reader was handed is graded by the
# judge's verdict on that answer (recalled when right, lapsed when wrong),
# moving the same review clock the pack keeps (stability and difficulty,
# the FSRS update in packset-core). The next question's fused hits are
# reweighed by retrievability, so a turn that served right answers stays
# retrievable and one that served wrong answers or was never used fades.
# One question is one step of the clock. `oracle` grades by the gold
# evidence instead: the ceiling the judge-graded loop is measured against.
RETRIEVABILITY_FLOOR = 0.25


def retrievability(elapsed, stability):
    stability = stability if stability > 0 else 1.0
    return (1.0 + 19.0 / 81.0 * max(elapsed, 0.0) / stability) ** -0.5


def review(state, tid, step, recalled):
    s = state.setdefault(tid, {"stability": 1.0, "difficulty": 5.0, "last": None})
    if recalled:
        elapsed = 0.0 if s["last"] is None else step - s["last"]
        retr = min(max(0.9 ** (elapsed / s["stability"]) if s["stability"] > 0 else 0.0, 0.01), 0.99)
        s["difficulty"] = min(max(s["difficulty"] - 0.15, 1.0), 10.0)
        s["stability"] *= 1.0 + math.exp(1.0 - s["difficulty"] / 10.0) * (1.0 - retr)
    else:
        s["difficulty"] = min(max(s["difficulty"] + 0.2, 1.0), 10.0)
        s["stability"] = max(s["stability"] * 0.5, 0.1)
    s["last"] = step


def reweigh(scored, state, step):
    """Fused (id, score) pairs scaled by retrievability since last review, or since the start."""
    out = []
    for tid, score in scored:
        s = state.get(tid)
        last = 0 if s is None else (s["last"] if s["last"] is not None else 0)
        stability = 1.0 if s is None else s["stability"]
        out.append((tid, score * max(retrievability(step - last, stability), RETRIEVABILITY_FLOOR)))
    out.sort(key=lambda x: -x[1])
    return [tid for tid, _ in out]


def locomo_where(sample):
    conv = sample["conversation"]
    where = {}
    for key, val in conv.items():
        if key.startswith("session_") and isinstance(val, list):
            date = conv.get(f"{key}_date_time", "")
            for n, t in enumerate(val):
                where[t["dia_id"]] = (key, n, date, f"{t.get('speaker', '')}: {t.get('text', '')}")
    return where


def locomo_prompt(where, chosen, question):
    picked = sorted((where[d] for d in chosen if d in where), key=lambda x: (int(x[0].split("_")[1]), x[1]))
    excerpts = "\n".join(f"[{date}] {text}" for _, _, date, text in picked)
    return LOCOMO_READ.format(excerpts, question)


def locomo_learn(data, rows, top, mode, base, key, reader, judge):
    """One conversation at a time, in question order, the clock moved by grades."""
    by_conv = {}
    for row in rows:
        if row["category"] == 5:
            continue
        by_conv.setdefault(row["conversation"], []).append(row)
    results = []
    for c, conv_rows in sorted(by_conv.items()):
        conv_rows.sort(key=lambda r: r["question_index"])
        where = locomo_where(data[c])
        state = {}
        for step, row in enumerate(conv_rows):
            scored = row["retrieved"].get("turns fused scored")
            if not scored:
                continue
            chosen = reweigh(scored, state, step)[:top] if mode != "none" else [t for t, _ in scored[:top]]
            prompt = locomo_prompt(where, chosen, row["question"])
            try:
                response = chat(base, key, reader, prompt[:HISTORY_CHARS], 256)
                verdict = chat(base, key, judge, JUDGE_BASE.format(row["question"], row["answer"], response), 8)
                correct = "yes" in verdict.lower()
            except Exception as e:
                response, correct = f"[error: {e}]", False
            gold = set(row["evidence"])
            for tid in chosen:
                if mode == "oracle":
                    review(state, tid, step, tid in gold)
                elif mode == "fsrs":
                    review(state, tid, step, correct)
            results.append({
                "question_id": f"c{c}-q{row['question_index']}",
                "question_type": f"category-{row['category']}",
                "correct": correct,
                "response": response,
                "chosen": chosen,
            })
        print(f"conversation {c}: {len(conv_rows)} questions", file=sys.stderr)
    return results


def locomo_rows(data, rows, arm, top):
    """LoCoMo: `data` is the list of samples, a row names a conversation and
    the turn ids an arm retrieved. Category 5 (adversarial) is left out, as
    the memory systems' published rows leave it out."""
    for row in rows:
        if row["category"] == 5:
            continue
        sample = data[row["conversation"]]
        conv = sample["conversation"]
        where = {}
        for key, val in conv.items():
            if key.startswith("session_") and isinstance(val, list):
                date = conv.get(f"{key}_date_time", "")
                for n, t in enumerate(val):
                    where[t["dia_id"]] = (key, n, date, f"{t.get('speaker', '')}: {t.get('text', '')}")
        chosen = row["evidence"] if arm == "oracle" else row["retrieved"][arm][:top]
        picked = sorted((where[d] for d in chosen if d in where), key=lambda x: (int(x[0].split("_")[1]), x[1]))
        excerpts = "\n".join(f"[{date}] {text}" for _, _, date, text in picked)
        yield {
            "question_id": f"c{row['conversation']}-q{row['question_index']}",
            "question_type": f"category-{row['category']}",
            "question": row["question"],
            "answer": row["answer"],
            "prompt": LOCOMO_READ.format(excerpts, row["question"]),
        }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dataset")
    ap.add_argument("dump")
    ap.add_argument("--bench", default="longmemeval", choices=["longmemeval", "locomo", "mab"])
    ap.add_argument("--arm", default="sessions fused")
    ap.add_argument("--top", type=int, default=5)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--types", default="", help="comma list of question types to keep")
    ap.add_argument("--no-timeline", action="store_true", help="raw dates only, the benchmark's own reading prompt")
    ap.add_argument("--gaps", action=argparse.BooleanOptionalAction, default=True,
                    help="list the days between every pair of retrieved sessions after them (on unless --no-gaps or --no-timeline)")
    ap.add_argument("--chunks", default="", help="MemoryAgentBench chunk store (PACKSET_MAB_CHUNKS)")
    ap.add_argument("--learn", default="", choices=["", "none", "fsrs", "oracle"],
                    help="LoCoMo test-time learning: answer in order, grade the turns used, reweigh by the review clock")
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    if a.no_timeline:
        a.gaps = False
    base = os.environ["QA_BASE_URL"]
    key = os.environ.get("QA_API_KEY", "")
    reader = os.environ.get("QA_MODEL", "")
    judge = os.environ.get("QA_JUDGE_MODEL", reader)
    raw = json.load(open(a.dataset)) if a.bench != "mab" else None
    rows = [json.loads(l) for l in open(a.dump) if l.strip()]
    if a.types:
        keep = {t.strip() for t in a.types.split(",") if t.strip()}
        rows = [r for r in rows if str(r.get("question_type", r.get("category", ""))) in keep]
    if a.limit:
        rows = rows[: a.limit]
    if a.bench == "mab":
        # The positional dataset is the directory of split files, or `-`.
        rows = mab_fill(rows, a.dataset if os.path.isdir(a.dataset) else "")
        items = list(mab_rows(rows, a.chunks, a.arm, a.top))

        def one_mab(item):
            try:
                response = chat(base, key, reader, item["prompt"][:HISTORY_CHARS], 256)
                if item["question_type"].startswith("longmemeval") and item["lme_type"]:
                    verdict = chat(
                        base, key, judge,
                        judge_prompt(item["lme_type"], item["question"], item["answers"][0], response), 8,
                    )
                    correct = "yes" in verdict.lower()
                else:
                    correct = substring_match(item["answers"], response)
            except Exception as e:
                return {"question_id": item["question_id"], "question_type": item["question_type"],
                        "correct": False, "response": f"[error: {e}]"}
            return {"question_id": item["question_id"], "question_type": item["question_type"],
                    "correct": correct, "response": response}

        results = []
        with cf.ThreadPoolExecutor(a.workers) as pool:
            for n, r in enumerate(pool.map(one_mab, items), 1):
                results.append(r)
                if n % 100 == 0:
                    print(f"{n} questions", file=sys.stderr)
        report(results, a, reader, judge, "MemoryAgentBench")
        return
    if a.bench == "locomo" and a.learn:
        results = locomo_learn(raw, rows, a.top, a.learn, base, key, reader, judge)
        a.arm = f"turns fused, learn {a.learn}"
        report(results, a, reader, judge, "LoCoMo")
        return
    if a.bench == "locomo":
        items = list(locomo_rows(raw, rows, a.arm, a.top))

        def one_locomo(item):
            try:
                response = chat(base, key, reader, item["prompt"][:HISTORY_CHARS], 256)
                verdict = chat(base, key, judge, JUDGE_BASE.format(item["question"], item["answer"], response), 8)
            except Exception as e:
                return {
                    "question_id": item["question_id"],
                    "question_type": item["question_type"],
                    "correct": False,
                    "response": f"[error: {e}]",
                }
            return {
                "question_id": item["question_id"],
                "question_type": item["question_type"],
                "correct": "yes" in verdict.lower(),
                "response": response,
            }

        results = []
        with cf.ThreadPoolExecutor(a.workers) as pool:
            for n, r in enumerate(pool.map(one_locomo, items), 1):
                results.append(r)
                if n % 100 == 0:
                    print(f"{n} questions", file=sys.stderr)
        report(results, a, reader, judge, "LoCoMo")
        return
    data = {q["question_id"]: q for q in raw}

    def one(row):
        q = data[row["question_id"]]
        ids = q["haystack_session_ids"]
        if a.arm == "oracle":
            chosen = row["answer_session_ids"]
        else:
            chosen = row["retrieved"][a.arm][: a.top]
        # Sessions in date order, as the benchmark's reader gets them.
        picked = sorted(
            (ids.index(s) for s in chosen if s in ids),
            key=lambda i: q["haystack_dates"][i],
        )
        parts = []
        for n, i in enumerate(picked):
            content = "\n".join(f"{t['role']}: {t['content']}" for t in q["haystack_sessions"][i])
            gap = None if a.no_timeline else days_before(q["question_date"], q["haystack_dates"][i])
            if gap is None:
                parts.append(SESSION.format(n + 1, q["haystack_dates"][i], content))
            else:
                parts.append(SESSION_TIMED.format(n + 1, q["haystack_dates"][i], gap, content))
        history = "".join(fit(parts))
        if a.gaps and len(picked) > 1:
            lines = []
            for x in range(len(picked)):
                for y in range(x + 1, len(picked)):
                    dx, dy = days_of(q["haystack_dates"][picked[x]]), days_of(q["haystack_dates"][picked[y]])
                    if dx is not None and dy is not None:
                        lines.append(f"Session {x + 1} to Session {y + 1}: {int(round(dy - dx))} days")
            if lines:
                history += GAPS_HEAD + "\n".join(lines) + "\n"
        prompt = READ.format(history, q["question_date"], q["question"])
        if not a.no_timeline:
            prompt = TIMELINE_NOTE + prompt
        if a.gaps:
            prompt = GAPS_NOTE + prompt
        try:
            response = chat(base, key, reader, prompt, 512)
            verdict = chat(base, key, judge, judge_prompt(q["question_type"], q["question"], q["answer"], response), 8)
        except Exception as e:  # one question's failure is one wrong answer, not a dead arm
            return {
                "question_id": q["question_id"],
                "question_type": q["question_type"],
                "correct": False,
                "response": f"[error: {e}]",
            }
        return {
            "question_id": q["question_id"],
            "question_type": q["question_type"],
            "correct": "yes" in verdict.lower(),
            "response": response,
        }

    results = []
    with cf.ThreadPoolExecutor(a.workers) as pool:
        for n, r in enumerate(pool.map(one, rows), 1):
            results.append(r)
            if n % 25 == 0:
                print(f"{n} questions", file=sys.stderr)
    report(results, a, reader, judge, "LongMemEval_S")


def report(results, a, reader, judge, bench):
    if a.out:
        with open(a.out, "w") as f:
            for r in results:
                f.write(json.dumps(r) + "\n")
    by = {}
    for r in results:
        t = by.setdefault(r["question_type"], [0, 0])
        t[0] += r["correct"]
        t[1] += 1
    total = sum(r["correct"] for r in results)
    timeline = "raw dates" if getattr(a, "no_timeline", False) else "timeline"
    if getattr(a, "gaps", False):
        timeline += " with gaps"
    print(f"\n{bench} answer accuracy, arm {a.arm!r} top {a.top}, {timeline}, reader {reader}, judge {judge}\n")
    print("| type | asked | accuracy |\n|---|---|---|")
    for kind, (c, n) in sorted(by.items()):
        print(f"| {kind} | {n} | {c / n:.3f} |")
    print(f"| all | {len(results)} | {total / max(len(results), 1):.3f} |")


if __name__ == "__main__":
    main()
