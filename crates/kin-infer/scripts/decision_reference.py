#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Write the reference fixture for the typed decision API.

For each example question this records the exact prompt text of every option order, the token ids
the Hugging Face tokenizer gives them, the letter-token logits float32 transformers computes at the
last position, and the option probabilities for one order, for two averaged orders, and for two
averaged orders at temperature 2. The Rust tests replay the prompt text through `ChoiceQuestion`
and the logits through `DecisionModel::decide`.

    python3 scripts/decision_reference.py <model_dir> tests/fixtures/decision_reference.json

The prompt layout below is the one `ChoiceQuestion` renders: the state, a blank line, the
question, one lettered line per option with "none of these" last, and `Answer:`. The model then
scores the next token over " a", " b", ... A second order lists the options in reverse with "none
of these" still last, and the two distributions are averaged per option.
"""
import json
import sys

import torch
from transformers import AutoModelForCausalLM, AutoTokenizer

LETTERS = "abcdefghijklmnopqrstuvwxyz"
CALL_TARGET_NONE = (
    "none of these: the call runs a definition not listed here "
    "(another type, a library, or a builtin)"
)

RUST_STATE = """Language: Rust
File: src/settings.rs
Imports:
use crate::format::parse_toml;
use std::fs;
use std::path::Path;
Caller: load (in Settings)
pub fn load(path: &Path) -> Result<Settings, SettingsError> {
        let text = fs::read_to_string(path)?;
        let table = parse_toml(&text)?;
        let name = table.get_str("name").unwrap_or("default");
        Ok(Settings {
            name: name.to_string(),
            retries: table.get_u32("retries").unwrap_or(3),
        })
    }

Call site: `let table = parse_toml(&text)?;`"""

TS_STATE = """Language: TypeScript
File: src/views/invoice.ts
Imports:
import { formatCurrency } from "../util/format";
import type { Invoice } from "../model";
Caller: renderInvoice (in InvoiceView)
renderInvoice(invoice: Invoice): string {
    const rows = invoice.lines.map((line) => {
      const price = line.quantity * line.unitPrice;
      return `<tr><td>${line.description}</td><td>${price}</td></tr>`;
    });
    const total = formatCurrency(invoice.total, this.locale);
    return `<table>${rows.join("")}</table><p>Total: ${total}</p>`;
  }

Call site: `const total = formatCurrency(invoice.total, this.locale);`"""


def call_question(callee):
    return f"Which definition does the call to `{callee}` at this call site execute?"


RUST_OPTIONS = [
    "src/format.rs: parse_toml :: pub fn parse_toml(text: &str) -> Result<Table, FormatError>",
    "tests/support/mod.rs: parse_toml :: pub fn parse_toml(path: &Path) -> Table",
    "src/legacy.rs: LegacyReader.parse_toml :: fn parse_toml(&self, text: &str) -> Option<Table>",
]
TS_OPTIONS = [
    "src/util/format.ts: formatCurrency :: export function formatCurrency(amount: number, locale: string): string",
    "src/i18n/currency.ts: CurrencyFormatter.formatCurrency :: formatCurrency(amount: number): string",
]

CASES = [
    ("rust-call-with-none", RUST_STATE, call_question("parse_toml"), RUST_OPTIONS, CALL_TARGET_NONE),
    ("rust-call-without-none", RUST_STATE, call_question("parse_toml"), RUST_OPTIONS, None),
    ("typescript-call-with-none", TS_STATE, call_question("formatCurrency"), TS_OPTIONS, CALL_TARGET_NONE),
    ("typescript-call-without-none", TS_STATE, call_question("formatCurrency"), TS_OPTIONS, None),
    (
        "label-with-default-none",
        "Commit message: Fix the page count when the last page is empty",
        "Which label fits this change?",
        ["bug fix", "new feature", "documentation"],
        "none of these",
    ),
]


def prompt(state, question, texts):
    lines = "\n".join(f"{LETTERS[i]}. {t}" for i, t in enumerate(texts))
    return f"{state}\n\nQuestion: {question}\n{lines}\nAnswer:"


def orders(n_options, has_none):
    """The shown order, then the reverse order with "none of these" kept last."""
    tail = [n_options] if has_none else []
    return [list(range(n_options)) + tail, list(reversed(range(n_options))) + tail]


def combine(order_list, logits, k, temperature):
    agg = [0.0] * k
    for order, row in zip(order_list, logits):
        p = torch.softmax(torch.tensor(row) / temperature, -1).tolist()
        for pos, i in enumerate(order):
            agg[i] += p[pos] / len(order_list)
    return agg


def model_name(model_dir):
    parts = model_dir.rstrip("/").split("/")
    for i, part in enumerate(parts):
        if part.startswith("models--"):
            return part[len("models--"):].replace("--", "/"), parts[-1]
    return parts[-1], None


def main():
    model_dir, out = sys.argv[1], sys.argv[2]
    tok = AutoTokenizer.from_pretrained(model_dir)
    model = AutoModelForCausalLM.from_pretrained(model_dir, dtype=torch.float32).eval()
    letter_ids = [tok.encode(" " + letter, add_special_tokens=False)[0] for letter in LETTERS]
    cases = []
    with torch.no_grad():
        for name, state, question, options, none_text in CASES:
            texts = options + ([none_text] if none_text is not None else [])
            k = len(texts)
            order_list = orders(len(options), none_text is not None)
            prompts, token_ids, logits = [], [], []
            for order in order_list:
                text = prompt(state, question, [texts[i] for i in order])
                ids = tok(text)["input_ids"]
                row = model(torch.tensor([ids]), logits_to_keep=1).logits[0, -1]
                prompts.append(text)
                token_ids.append(ids)
                logits.append([float(row[t]) for t in letter_ids[:k]])
            cases.append({
                "name": name,
                "state": state,
                "question": question,
                "options": options,
                "none_text": none_text,
                "prompts": prompts,
                "token_ids": token_ids,
                "letter_logits": logits,
                "probabilities": {
                    "single": combine(order_list[:1], logits[:1], k, 1.0),
                    "averaged": combine(order_list, logits, k, 1.0),
                    "averaged_temperature_2": combine(order_list, logits, k, 2.0),
                },
            })
    name, revision = model_name(model_dir)
    fixture = {"model": name, "revision": revision, "dtype": "float32", "letters": LETTERS,
               "letter_ids": letter_ids, "cases": cases}
    with open(out, "w") as f:
        json.dump(fixture, f, indent=1)
        f.write("\n")
    print(f"wrote {out}: {[[len(ids) for ids in c['token_ids']] for c in cases]} tokens")


if __name__ == "__main__":
    main()
