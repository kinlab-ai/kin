// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC
"use strict";
(() => {
  const root = (globalThis.kinReview = globalThis.kinReview || {});

  // One hand-written scanner over the whole text. Multi-line constructs
  // (block comments, templates, triple quotes, raw strings) are simply long
  // tokens, and `emit` splits any token at "\n" so the output is per line.
  // That keeps cross-line state implicit and the work linear.

  const words = (s) => new Set(s.split(" "));

  const JS_KW =
    "break case catch class const continue debugger default delete do else export extends finally for function if import in instanceof let new return static switch throw try typeof var void while with yield async await as";
  const TS_KW =
    "abstract declare enum implements interface namespace private protected public readonly type module keyof infer is satisfies override asserts unique require accessor";
  const TS_TYPES =
    "string number boolean void any unknown never object symbol bigint";
  const C_TYPES =
    "char double float int long short signed unsigned void bool size_t ssize_t ptrdiff_t wchar_t int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t intptr_t uintptr_t";
  const C_KW =
    "auto break case const continue default do else enum extern for goto if inline register restrict return sizeof static struct switch typedef union volatile while";

  // Per-language configuration.
  //  slash/hash/block: comment styles. nest: block comments nest.
  //  sq: single quotes open a string. rustQuote: single quote is a char
  //  literal only when it closes (otherwise it is a lifetime).
  //  bt: "tpl" (backslash escapes, JS) or "raw" (no escapes, Go) backticks.
  //  triple: quote characters that have a triple-quoted multi-line form.
  //  prefix: identifiers that glue onto a following quote (r"", b'', f"").
  //  rawHash: Rust r#"..."# strings. dollar: $ is an identifier character.
  //  regex: allow conservative regex literals. rubyBegin: =begin/=end.
  const LANGS = new Map();
  const def = (names, cfg) => {
    const full = Object.assign(
      {
        slash: false, hash: false, block: false, nest: false, sq: true,
        rustQuote: false, bt: "", triple: "", prefix: null, rawHash: false,
        dollar: false, regex: false, rubyBegin: false,
      },
      cfg
    );
    full.kw = words(cfg.kw || "");
    full.lit = words(cfg.lit || "");
    full.ty = words(cfg.ty || "");
    for (const n of names.split(" ")) LANGS.set(n, full);
  };

  // Unknown or missing language: a generic C-like scan with no keywords.
  const FALLBACK = {
    slash: true, hash: false, block: true, nest: false, sq: true,
    rustQuote: false, bt: "", triple: "", prefix: null, rawHash: false,
    dollar: false, regex: false, rubyBegin: false,
    kw: words(""), lit: words(""), ty: words(""),
  };

  def("javascript", {
    slash: true, block: true, bt: "tpl", dollar: true, regex: true,
    kw: JS_KW,
    lit: "true false null undefined NaN Infinity this super",
    ty: "",
  });
  def("typescript", {
    slash: true, block: true, bt: "tpl", dollar: true, regex: true,
    // `void` is a type here, not the JS operator.
    kw: (JS_KW + " " + TS_KW).replace(" void", ""),
    lit: "true false null undefined NaN Infinity this super",
    ty: TS_TYPES,
  });
  def("python", {
    hash: true, triple: "\"'", prefix: /^(?:[rRbBuUfF]|[rR][bBfF]|[bBfF][rR])$/,
    kw: "and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield match case",
    lit: "True False None self cls",
    ty: "int float str bool bytes list dict set tuple object complex frozenset bytearray",
  });
  def("go", {
    slash: true, block: true, bt: "raw",
    kw: "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var",
    lit: "true false nil iota",
    ty: "bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uint8 uint16 uint32 uint64 uintptr any",
  });
  def("java", {
    slash: true, block: true, triple: "\"",
    kw: "abstract assert break case catch class const continue default do else enum extends final finally for goto if implements import instanceof interface native new package private protected public return static strictfp switch synchronized throw throws transient try volatile while var record sealed permits yield",
    lit: "true false null this super",
    ty: "boolean byte char double float int long short void",
  });
  def("rust", {
    slash: true, block: true, nest: true, rustQuote: true, rawHash: true,
    prefix: /^(?:b|c)$/,
    kw: "as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move mut pub ref return static struct trait type unsafe use where while union macro_rules",
    lit: "true false self super None",
    ty: "i8 i16 i32 i64 i128 isize u8 u16 u32 u64 u128 usize f32 f64 bool char str String Vec Option Result Box",
  });
  def("c", {
    slash: true, block: true,
    kw: C_KW,
    lit: "true false NULL nullptr",
    ty: C_TYPES,
  });
  def("cpp", {
    slash: true, block: true,
    kw: C_KW + " class namespace template typename public private protected virtual override final new delete try catch throw using operator explicit friend constexpr consteval constinit noexcept static_cast dynamic_cast reinterpret_cast const_cast mutable decltype thread_local co_await co_return co_yield concept requires export import module",
    lit: "true false NULL nullptr this",
    ty: C_TYPES + " auto string",
  });
  def("csharp", {
    slash: true, block: true, triple: "\"",
    kw: "abstract as base break case catch checked class const continue default delegate do else enum event explicit extern finally fixed for foreach goto if implicit in interface internal is lock namespace new operator out override params private protected public readonly ref return sealed sizeof stackalloc static struct switch throw try typeof unchecked unsafe using virtual volatile while var async await yield record partial where",
    lit: "true false null this base",
    ty: "bool byte char decimal double float int long object sbyte short string uint ulong ushort void dynamic nint nuint",
  });
  def("ruby", {
    hash: true, rubyBegin: true,
    kw: "alias and begin break case class def do else elsif end ensure for if in module next not or redo rescue retry return then undef unless until when while yield require require_relative include extend attr_accessor attr_reader attr_writer private protected public raise loop lambda proc",
    lit: "true false nil self super",
    ty: "",
  });
  def("php", {
    slash: true, hash: true, block: true, dollar: true,
    kw: "abstract and as break callable case catch class clone const continue declare default do echo else elseif empty enddeclare endfor endforeach endif endswitch endwhile extends final finally fn for foreach function global goto if implements include include_once instanceof insteadof interface isset list match namespace new or print private protected public readonly require require_once return static switch throw trait try unset use var while xor yield enum",
    lit: "true false null $this self parent TRUE FALSE NULL",
    ty: "int float string bool array void mixed iterable object never",
  });
  def("swift", {
    slash: true, block: true, nest: true, triple: "\"",
    kw: "associatedtype break case catch class continue default defer deinit do else enum extension fallthrough fileprivate for func guard if import in init inout internal let open operator private protocol public repeat rethrows return static struct subscript switch throw throws try typealias var where while async await actor some any override final lazy mutating nonmutating weak unowned convenience required indirect",
    lit: "true false nil self super",
    ty: "",
  });
  def("kotlin", {
    slash: true, block: true, nest: true, triple: "\"",
    kw: "as break class continue do else for fun if in interface is object package return throw try typealias typeof val var when while catch constructor finally import init abstract annotation companion const crossinline data enum expect external final infix inline inner internal lateinit noinline open operator out override private protected public reified sealed suspend tailrec vararg actual",
    lit: "true false null this super",
    ty: "",
  });
  def("hcl", {
    slash: true, hash: true, block: true, sq: false,
    kw: "resource data variable output locals module provider terraform for in if else dynamic provisioner",
    lit: "true false null",
    ty: "string number bool list map set object tuple any",
  });

  const isLetter = (c, dollar) =>
    (c >= 97 && c <= 122) || (c >= 65 && c <= 90) || c === 95 || c >= 128 ||
    (dollar && c === 36);
  const isDigit = (c) => c >= 48 && c <= 57;

  // Sticky so it anchors at lastIndex. Every alternative is a flat run with
  // no nested quantifiers, so it cannot backtrack catastrophically. The
  // trailing word covers suffixes such as n, f, u32, i64, L and ul.
  const NUM_RE =
    /0[xX][0-9a-fA-F_]+(?:[a-zA-Z]\w*)?|0[bB][01_]+(?:[a-zA-Z]\w*)?|0[oO][0-7_]+(?:[a-zA-Z]\w*)?|(?:\d[\d_]*(?:\.\d[\d_]*)?|\.\d[\d_]*)(?:[eE][+-]?\d[\d_]*)?(?:[a-zA-Z]\w*)?/y;

  // A `/` starts a regex only after these characters (or at line start).
  // Anything else, notably an identifier, number, `)` or `]`, means division.
  const REGEX_PREV = "(,=:[!&|?{};}";
  const REGEX_WORDS = new Set(["return", "typeof", "case"]);

  function tokenize(text, language) {
    const L = (typeof language === "string" && LANGS.get(language)) || FALLBACK;
    const n = text.length;
    const lines = [[]];
    let line = lines[0];

    // Append a token, splitting at newlines. Adjacent plain text is merged.
    function emit(type, s) {
      let from = 0;
      for (;;) {
        const nl = s.indexOf("\n", from);
        const part = nl < 0 ? s.slice(from) : s.slice(from, nl);
        if (part) {
          const last = line[line.length - 1];
          if (type === "" && last && last[0] === "") last[1] += part;
          else line.push([type, part]);
        }
        if (nl < 0) break;
        line = [];
        lines.push(line);
        from = nl + 1;
      }
    }

    // A quoted string on one line. Unterminated runs to the end of the line.
    function readQuoted(i, q) {
      let j = i + 1;
      while (j < n) {
        const ch = text.charCodeAt(j);
        if (ch === q) return j + 1;
        if (ch === 10) return j;
        // A backslash escapes the next char, but never swallows the newline.
        if (ch === 92 && j + 1 < n && text.charCodeAt(j + 1) !== 10) j += 2;
        else j++;
      }
      return n;
    }

    // End index of a string literal opening at `i`, or -1 if none does.
    function stringAt(i) {
      const q = text.charCodeAt(i);
      if (q === 96) {
        if (!L.bt) return -1;
        let j = i + 1;
        while (j < n) {
          const ch = text.charCodeAt(j);
          if (ch === 96) return j + 1;
          if (ch === 92 && L.bt === "tpl") j += 2;
          else j++;
        }
        return n;
      }
      if (q === 39 && !L.sq) return -1;
      if (q === 39 && L.rustQuote) {
        // 'x' or an escape such as '\n' or '\u{1F600}' closes within a few
        // chars; 'a (a lifetime) never does, so it stays plain text.
        if (text.charCodeAt(i + 1) === 92) {
          for (let j = i + 3; j < n && j < i + 14; j++) {
            const ch = text.charCodeAt(j);
            if (ch === 39) return j + 1;
            if (ch === 10) return -1;
          }
          return -1;
        }
        const first = text.charCodeAt(i + 1);
        const width = first >= 0xd800 && first <= 0xdbff ? 2 : 1;
        return text.charCodeAt(i + 1 + width) === 39 && first !== 39 && first !== 10
          ? i + 2 + width
          : -1;
      }
      const ch = text[i];
      if (L.triple.indexOf(ch) >= 0 && text.charCodeAt(i + 1) === q && text.charCodeAt(i + 2) === q) {
        const close = ch + ch + ch;
        let j = i + 3;
        while (j < n) {
          if (text.charCodeAt(j) === 92) j += 2;
          else if (text.charCodeAt(j) === q && text.startsWith(close, j)) return j + 3;
          else j++;
        }
        return n;
      }
      return readQuoted(i, q);
    }

    function blockCommentEnd(i) {
      if (!L.nest) {
        const k = text.indexOf("*/", i + 2);
        return k < 0 ? n : k + 2;
      }
      let depth = 1;
      let j = i + 2;
      while (j < n) {
        if (text.charCodeAt(j) === 47 && text.charCodeAt(j + 1) === 42) { depth++; j += 2; }
        else if (text.charCodeAt(j) === 42 && text.charCodeAt(j + 1) === 47) {
          depth--;
          j += 2;
          if (depth === 0) return j;
        } else j++;
      }
      return n;
    }

    // Regex literal end, or -1. Stays on one line; a failed attempt records
    // where the line ends so later `/` on that line do not rescan it
    // (otherwise a long line of `(/` would be quadratic).
    let noRegexBefore = -1;
    function regexEnd(i) {
      if (i < noRegexBefore) return -1;
      let j = i + 1;
      let inClass = false;
      while (j < n) {
        const ch = text.charCodeAt(j);
        if (ch === 10) { noRegexBefore = j; return -1; }
        if (ch === 92) j += 2;
        else if (ch === 91) { inClass = true; j++; }
        else if (ch === 93) { inClass = false; j++; }
        else if (ch === 47 && !inClass) {
          j++;
          while (j < n && ((text.charCodeAt(j) | 32) >= 97 && (text.charCodeAt(j) | 32) <= 122)) j++;
          return j;
        } else j++;
      }
      noRegexBefore = n;
      return -1;
    }

    // Last significant char and word on this line, for regex and `.kw` rules.
    let prevCh = "";
    let prevWord = "";
    let i = 0;

    while (i < n) {
      const c = text.charCodeAt(i);

      if (c === 10) { emit("", "\n"); prevCh = ""; prevWord = ""; i++; continue; }
      if (c === 32 || c === 9 || c === 13) { emit("", text[i]); i++; continue; }

      // Comments.
      if (c === 47 && (L.slash || L.block || L.regex)) {
        const next = text.charCodeAt(i + 1);
        if (L.slash && next === 47) {
          const k = text.indexOf("\n", i);
          const end = k < 0 ? n : k;
          emit("com", text.slice(i, end));
          i = end;
          continue;
        }
        if (L.block && next === 42) {
          const end = blockCommentEnd(i);
          emit("com", text.slice(i, end));
          i = end;
          continue;
        }
        if (L.regex && next !== 47 && next !== 42 &&
            (prevCh === "" || REGEX_PREV.indexOf(prevCh) >= 0 || (prevCh === "a" && REGEX_WORDS.has(prevWord)))) {
          const end = regexEnd(i);
          if (end > 0) {
            emit("str", text.slice(i, end));
            i = end;
            prevCh = '"'; prevWord = "";
            continue;
          }
        }
      }
      if (c === 35 && L.hash) {
        const k = text.indexOf("\n", i);
        const end = k < 0 ? n : k;
        emit("com", text.slice(i, end));
        i = end;
        continue;
      }
      if (c === 61 && L.rubyBegin && (i === 0 || text.charCodeAt(i - 1) === 10) && text.startsWith("=begin", i)) {
        const k = text.indexOf("\n=end", i);
        let end = n;
        if (k >= 0) {
          const nl = text.indexOf("\n", k + 1);
          end = nl < 0 ? n : nl;
        }
        emit("com", text.slice(i, end));
        i = end;
        continue;
      }

      // Strings.
      if (c === 34 || c === 39 || c === 96) {
        const end = stringAt(i);
        if (end > i) {
          emit("str", text.slice(i, end));
          i = end;
          prevCh = '"'; prevWord = "";
          continue;
        }
      }

      // Numbers. A leading dot only counts when it cannot be member access.
      if (isDigit(c) || (c === 46 && isDigit(text.charCodeAt(i + 1)) &&
          prevCh !== "a" && prevCh !== ")" && prevCh !== "]")) {
        NUM_RE.lastIndex = i;
        const m = NUM_RE.exec(text);
        if (m && m[0].length > 0) {
          emit("num", m[0]);
          i += m[0].length;
          prevCh = "0"; prevWord = "";
          continue;
        }
      }

      // Identifiers and keywords. Non-ASCII is treated as identifier text so
      // surrogate pairs are never split.
      if (isLetter(c, L.dollar)) {
        let j = i + 1;
        while (j < n) {
          const ch = text.charCodeAt(j);
          if (isLetter(ch, L.dollar) || isDigit(ch)) j++;
          else break;
        }
        const word = text.slice(i, j);
        const nc = text.charCodeAt(j);

        // Prefixed strings (python r/b/f, rust b/c) and rust raw strings.
        if (L.prefix && (nc === 34 || nc === 39) && L.prefix.test(word)) {
          const end = stringAt(j);
          if (end > j) {
            emit("str", text.slice(i, end));
            i = end;
            prevCh = '"'; prevWord = "";
            continue;
          }
        }
        if (L.rawHash && (word === "r" || word === "br" || word === "cr") && (nc === 34 || nc === 35)) {
          let k = j;
          while (text.charCodeAt(k) === 35) k++;
          if (text.charCodeAt(k) === 34) {
            const close = '"' + "#".repeat(k - j);
            const at = text.indexOf(close, k + 1);
            const end = at < 0 ? n : at + close.length;
            emit("str", text.slice(i, end));
            i = end;
            prevCh = '"'; prevWord = "";
            continue;
          }
        }

        // After a dot the word is a member name, never a keyword.
        const member = prevCh === ".";
        let type = "";
        if (!member && L.kw.has(word)) type = "kw";
        else if (!member && L.lit.has(word)) type = "lit";
        else if (nc === 40) type = "fn";
        else if (L.ty.has(word)) type = "type";
        else {
          const f = text.charCodeAt(i);
          if (f >= 65 && f <= 90 && /[a-z]/.test(word)) type = "type";
        }
        if (type) emit(type, word);
        else emit("", word);
        i = j;
        prevCh = "a"; prevWord = word;
        continue;
      }

      // Anything else is one plain character.
      emit("", text[i]);
      prevCh = text[i]; prevWord = "";
      i++;
    }
    return lines;
  }

  /**
   * Tokenize `text` for `language`. One array per line of `text.split("\n")`,
   * each a list of [type, string] pairs that concatenate back to the line.
   */
  root.highlightLines = function highlightLines(text, language) {
    const src = typeof text === "string" ? text : String(text == null ? "" : text);
    try {
      return tokenize(src, language);
    } catch (_err) {
      // The page must keep working: fall back to unhighlighted lines.
      return src.split("\n").map((l) => (l ? [["", l]] : []));
    }
  };
})();
