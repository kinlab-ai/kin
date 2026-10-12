#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Write reference next-token logits for the Qwen3 decision parity test.

Runs the model in float32 with Hugging Face transformers and records, for a few fixed prompts,
the prompt's token ids and the last position's logits for a handful of candidate tokens. The
Rust test replays the same token ids through kin-infer and compares.

    python3 scripts/qwen3_reference.py <model_dir> tests/fixtures/qwen3_reference.json
"""
import json
import sys

import torch
from transformers import AutoModelForCausalLM, AutoTokenizer

CODE = """fn checksum(bytes: &[u8]) -> u32 {
    let mut acc: u32 = 0;
    for b in bytes {
        acc = acc.rotate_left(5) ^ u32::from(*b);
    }
    acc
}
"""

PROMPTS = [
    ("The capital of France is", [" Paris", " London", " Berlin", " Rome"]),
    (
        "Question: Which function computes a checksum?\n"
        "a. parse_config\nb. crc32\nc. render_page\nd. none of these\nAnswer:",
        [" a", " b", " c", " d"],
    ),
    (
        "\n".join(f"// file {i}\n{CODE}" for i in range(12))
        + "\nQuestion: How many times is checksum defined above?\n"
        "a. once\nb. twelve times\nc. none of these\nAnswer:",
        [" a", " b", " c"],
    ),
]


def main():
    model_dir, out = sys.argv[1], sys.argv[2]
    tok = AutoTokenizer.from_pretrained(model_dir)
    model = AutoModelForCausalLM.from_pretrained(model_dir, dtype=torch.float32).eval()
    cases = []
    with torch.no_grad():
        for text, candidates in PROMPTS:
            ids = tok.encode(text, add_special_tokens=False)
            cand = [tok.encode(c, add_special_tokens=False)[0] for c in candidates]
            logits = model(torch.tensor([ids])).logits[0, -1]
            cases.append({"text": text, "token_ids": ids, "candidate_ids": cand,
                          "logits": [float(logits[c]) for c in cand]})
    json.dump({"model": model_dir.rstrip("/").split("/")[-1], "dtype": "float32", "cases": cases}, open(out, "w"), indent=1)
    print(f"wrote {out}: {[len(c['token_ids']) for c in cases]} tokens")


if __name__ == "__main__":
    main()
