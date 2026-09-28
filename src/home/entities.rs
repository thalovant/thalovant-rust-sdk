//! Character references for [`plain_speech`](super::plain_speech), decoded
//! the way Python's `html.unescape` decodes them.
//!
//! Numeric references are complete, including the replacements HTML makes
//! for the Windows-1252 range and for code points that are not characters.
//! Named references are HTML 4's, `&apos;`, and the upper-case spellings HTML
//! keeps for a few of them, with or without the semicolon where HTML allows
//! it; any other HTML5 name is left as written.

use std::borrow::Cow;

/// `text` with its character references decoded.
pub(crate) fn unescape(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        match reference(tail, &mut out) {
            Some(consumed) => rest = &tail[consumed..],
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Decode the reference `tail` starts with (the text after an `&`) into
/// `out`, returning how many bytes of `tail` it used; `None` when it is not
/// one, and nothing was written.
fn reference(tail: &str, out: &mut String) -> Option<usize> {
    if let Some(number) = tail.strip_prefix('#') {
        let (hex, digits) = match number.strip_prefix(['x', 'X']) {
            Some(hex) => (true, hex),
            None => (false, number),
        };
        let radix = if hex { 16 } else { 10 };
        let length = digits
            .chars()
            .take_while(|digit| digit.is_digit(radix))
            .count();
        if length == 0 {
            return None;
        }
        // Held at one past the last code point: anything beyond it reads the
        // same, as U+FFFD, however many digits it has.
        let value = digits[..length].chars().fold(0_u32, |value, digit| {
            let digit = digit.to_digit(radix).unwrap_or(0);
            value
                .saturating_mul(radix)
                .saturating_add(digit)
                .min(0x11_0000)
        });
        push_numeric(value, out);
        let used = 1 + usize::from(hex) + length;
        return Some(used + usize::from(tail[used..].starts_with(';')));
    }
    // Up to 32 characters that can be part of a name, and the `;` after them
    // when there is one: the run `html.unescape` looks up.
    let name_length: usize = tail
        .chars()
        .take_while(|character| {
            !matches!(
                character,
                '\t' | '\n' | '\x0c' | ' ' | '<' | '&' | '#' | ';'
            )
        })
        .take(32)
        .map(char::len_utf8)
        .sum();
    if name_length == 0 {
        return None;
    }
    let name = &tail[..name_length];
    let semicolon = tail[name_length..].starts_with(';');
    if semicolon {
        if let Some(character) = named(name) {
            out.push(character);
            return Some(name_length + 1);
        }
    } else if LEGACY_REFERENCES.binary_search(&name).is_ok() {
        out.push(named(name)?);
        return Some(name_length);
    }
    // Otherwise the longest name that HTML accepts without a semicolon and
    // that begins the run, two characters at least; the rest of the run stays
    // as written.
    let run = &tail[..name_length + usize::from(semicolon)];
    let longest = (2..run.len())
        .rev()
        .filter(|end| run.is_char_boundary(*end))
        .find(|end| LEGACY_REFERENCES.binary_search(&&run[..*end]).is_ok())?;
    out.push(named(&run[..longest])?);
    Some(longest)
}

fn named(name: &str) -> Option<char> {
    NAMED_REFERENCES
        .binary_search_by(|(candidate, _)| candidate.cmp(&name))
        .ok()
        .map(|index| NAMED_REFERENCES[index].1)
}

/// Write what a numeric reference to `value` reads as.
fn push_numeric(value: u32, out: &mut String) {
    let replaced = match value {
        0x00 => Some('\u{fffd}'),
        0x0d => Some('\r'),
        0x80 => Some('\u{20ac}'),
        0x82 => Some('\u{201a}'),
        0x83 => Some('\u{192}'),
        0x84 => Some('\u{201e}'),
        0x85 => Some('\u{2026}'),
        0x86 => Some('\u{2020}'),
        0x87 => Some('\u{2021}'),
        0x88 => Some('\u{2c6}'),
        0x89 => Some('\u{2030}'),
        0x8a => Some('\u{160}'),
        0x8b => Some('\u{2039}'),
        0x8c => Some('\u{152}'),
        0x8e => Some('\u{17d}'),
        0x91 => Some('\u{2018}'),
        0x92 => Some('\u{2019}'),
        0x93 => Some('\u{201c}'),
        0x94 => Some('\u{201d}'),
        0x95 => Some('\u{2022}'),
        0x96 => Some('\u{2013}'),
        0x97 => Some('\u{2014}'),
        0x98 => Some('\u{2dc}'),
        0x99 => Some('\u{2122}'),
        0x9a => Some('\u{161}'),
        0x9b => Some('\u{203a}'),
        0x9c => Some('\u{153}'),
        0x9e => Some('\u{17e}'),
        0x9f => Some('\u{178}'),
        // The four control characters Windows-1252 leaves undefined keep
        // their own code point.
        0x81 | 0x8d | 0x8f | 0x90 | 0x9d => char::from_u32(value),
        0xd800..=0xdfff | 0x11_0000.. => Some('\u{fffd}'),
        // Code points that are not characters read as nothing.
        0x01..=0x08 | 0x0b | 0x0e..=0x1f | 0x7f | 0xfdd0..=0xfdef => None,
        _ if value & 0xfffe == 0xfffe => None,
        _ => char::from_u32(value),
    };
    if let Some(character) = replaced {
        out.push(character);
    }
}

/// Named character references this decoder knows, sorted by name: HTML 4's,
/// `&apos;`, and the upper-case spellings HTML keeps for a few of them.
/// Generated from Python's `html.entities.html5`.
const NAMED_REFERENCES: &[(&str, char)] = &[
    ("AElig", '\u{c6}'),
    ("AMP", '\u{26}'),
    ("Aacute", '\u{c1}'),
    ("Acirc", '\u{c2}'),
    ("Agrave", '\u{c0}'),
    ("Alpha", '\u{391}'),
    ("Aring", '\u{c5}'),
    ("Atilde", '\u{c3}'),
    ("Auml", '\u{c4}'),
    ("Beta", '\u{392}'),
    ("COPY", '\u{a9}'),
    ("Ccedil", '\u{c7}'),
    ("Chi", '\u{3a7}'),
    ("Dagger", '\u{2021}'),
    ("Delta", '\u{394}'),
    ("ETH", '\u{d0}'),
    ("Eacute", '\u{c9}'),
    ("Ecirc", '\u{ca}'),
    ("Egrave", '\u{c8}'),
    ("Epsilon", '\u{395}'),
    ("Eta", '\u{397}'),
    ("Euml", '\u{cb}'),
    ("GT", '\u{3e}'),
    ("Gamma", '\u{393}'),
    ("Iacute", '\u{cd}'),
    ("Icirc", '\u{ce}'),
    ("Igrave", '\u{cc}'),
    ("Iota", '\u{399}'),
    ("Iuml", '\u{cf}'),
    ("Kappa", '\u{39a}'),
    ("LT", '\u{3c}'),
    ("Lambda", '\u{39b}'),
    ("Mu", '\u{39c}'),
    ("Ntilde", '\u{d1}'),
    ("Nu", '\u{39d}'),
    ("OElig", '\u{152}'),
    ("Oacute", '\u{d3}'),
    ("Ocirc", '\u{d4}'),
    ("Ograve", '\u{d2}'),
    ("Omega", '\u{3a9}'),
    ("Omicron", '\u{39f}'),
    ("Oslash", '\u{d8}'),
    ("Otilde", '\u{d5}'),
    ("Ouml", '\u{d6}'),
    ("Phi", '\u{3a6}'),
    ("Pi", '\u{3a0}'),
    ("Prime", '\u{2033}'),
    ("Psi", '\u{3a8}'),
    ("QUOT", '\u{22}'),
    ("REG", '\u{ae}'),
    ("Rho", '\u{3a1}'),
    ("Scaron", '\u{160}'),
    ("Sigma", '\u{3a3}'),
    ("THORN", '\u{de}'),
    ("Tau", '\u{3a4}'),
    ("Theta", '\u{398}'),
    ("Uacute", '\u{da}'),
    ("Ucirc", '\u{db}'),
    ("Ugrave", '\u{d9}'),
    ("Upsilon", '\u{3a5}'),
    ("Uuml", '\u{dc}'),
    ("Xi", '\u{39e}'),
    ("Yacute", '\u{dd}'),
    ("Yuml", '\u{178}'),
    ("Zeta", '\u{396}'),
    ("aacute", '\u{e1}'),
    ("acirc", '\u{e2}'),
    ("acute", '\u{b4}'),
    ("aelig", '\u{e6}'),
    ("agrave", '\u{e0}'),
    ("alefsym", '\u{2135}'),
    ("alpha", '\u{3b1}'),
    ("amp", '\u{26}'),
    ("and", '\u{2227}'),
    ("ang", '\u{2220}'),
    ("apos", '\u{27}'),
    ("aring", '\u{e5}'),
    ("asymp", '\u{2248}'),
    ("atilde", '\u{e3}'),
    ("auml", '\u{e4}'),
    ("bdquo", '\u{201e}'),
    ("beta", '\u{3b2}'),
    ("brvbar", '\u{a6}'),
    ("bull", '\u{2022}'),
    ("cap", '\u{2229}'),
    ("ccedil", '\u{e7}'),
    ("cedil", '\u{b8}'),
    ("cent", '\u{a2}'),
    ("chi", '\u{3c7}'),
    ("circ", '\u{2c6}'),
    ("clubs", '\u{2663}'),
    ("cong", '\u{2245}'),
    ("copy", '\u{a9}'),
    ("crarr", '\u{21b5}'),
    ("cup", '\u{222a}'),
    ("curren", '\u{a4}'),
    ("dArr", '\u{21d3}'),
    ("dagger", '\u{2020}'),
    ("darr", '\u{2193}'),
    ("deg", '\u{b0}'),
    ("delta", '\u{3b4}'),
    ("diams", '\u{2666}'),
    ("divide", '\u{f7}'),
    ("eacute", '\u{e9}'),
    ("ecirc", '\u{ea}'),
    ("egrave", '\u{e8}'),
    ("empty", '\u{2205}'),
    ("emsp", '\u{2003}'),
    ("ensp", '\u{2002}'),
    ("epsilon", '\u{3b5}'),
    ("equiv", '\u{2261}'),
    ("eta", '\u{3b7}'),
    ("eth", '\u{f0}'),
    ("euml", '\u{eb}'),
    ("euro", '\u{20ac}'),
    ("exist", '\u{2203}'),
    ("fnof", '\u{192}'),
    ("forall", '\u{2200}'),
    ("frac12", '\u{bd}'),
    ("frac14", '\u{bc}'),
    ("frac34", '\u{be}'),
    ("frasl", '\u{2044}'),
    ("gamma", '\u{3b3}'),
    ("ge", '\u{2265}'),
    ("gt", '\u{3e}'),
    ("hArr", '\u{21d4}'),
    ("harr", '\u{2194}'),
    ("hearts", '\u{2665}'),
    ("hellip", '\u{2026}'),
    ("iacute", '\u{ed}'),
    ("icirc", '\u{ee}'),
    ("iexcl", '\u{a1}'),
    ("igrave", '\u{ec}'),
    ("image", '\u{2111}'),
    ("infin", '\u{221e}'),
    ("int", '\u{222b}'),
    ("iota", '\u{3b9}'),
    ("iquest", '\u{bf}'),
    ("isin", '\u{2208}'),
    ("iuml", '\u{ef}'),
    ("kappa", '\u{3ba}'),
    ("lArr", '\u{21d0}'),
    ("lambda", '\u{3bb}'),
    ("lang", '\u{27e8}'),
    ("laquo", '\u{ab}'),
    ("larr", '\u{2190}'),
    ("lceil", '\u{2308}'),
    ("ldquo", '\u{201c}'),
    ("le", '\u{2264}'),
    ("lfloor", '\u{230a}'),
    ("lowast", '\u{2217}'),
    ("loz", '\u{25ca}'),
    ("lrm", '\u{200e}'),
    ("lsaquo", '\u{2039}'),
    ("lsquo", '\u{2018}'),
    ("lt", '\u{3c}'),
    ("macr", '\u{af}'),
    ("mdash", '\u{2014}'),
    ("micro", '\u{b5}'),
    ("middot", '\u{b7}'),
    ("minus", '\u{2212}'),
    ("mu", '\u{3bc}'),
    ("nabla", '\u{2207}'),
    ("nbsp", '\u{a0}'),
    ("ndash", '\u{2013}'),
    ("ne", '\u{2260}'),
    ("ni", '\u{220b}'),
    ("not", '\u{ac}'),
    ("notin", '\u{2209}'),
    ("nsub", '\u{2284}'),
    ("ntilde", '\u{f1}'),
    ("nu", '\u{3bd}'),
    ("oacute", '\u{f3}'),
    ("ocirc", '\u{f4}'),
    ("oelig", '\u{153}'),
    ("ograve", '\u{f2}'),
    ("oline", '\u{203e}'),
    ("omega", '\u{3c9}'),
    ("omicron", '\u{3bf}'),
    ("oplus", '\u{2295}'),
    ("or", '\u{2228}'),
    ("ordf", '\u{aa}'),
    ("ordm", '\u{ba}'),
    ("oslash", '\u{f8}'),
    ("otilde", '\u{f5}'),
    ("otimes", '\u{2297}'),
    ("ouml", '\u{f6}'),
    ("para", '\u{b6}'),
    ("part", '\u{2202}'),
    ("permil", '\u{2030}'),
    ("perp", '\u{22a5}'),
    ("phi", '\u{3c6}'),
    ("pi", '\u{3c0}'),
    ("piv", '\u{3d6}'),
    ("plusmn", '\u{b1}'),
    ("pound", '\u{a3}'),
    ("prime", '\u{2032}'),
    ("prod", '\u{220f}'),
    ("prop", '\u{221d}'),
    ("psi", '\u{3c8}'),
    ("quot", '\u{22}'),
    ("rArr", '\u{21d2}'),
    ("radic", '\u{221a}'),
    ("rang", '\u{27e9}'),
    ("raquo", '\u{bb}'),
    ("rarr", '\u{2192}'),
    ("rceil", '\u{2309}'),
    ("rdquo", '\u{201d}'),
    ("real", '\u{211c}'),
    ("reg", '\u{ae}'),
    ("rfloor", '\u{230b}'),
    ("rho", '\u{3c1}'),
    ("rlm", '\u{200f}'),
    ("rsaquo", '\u{203a}'),
    ("rsquo", '\u{2019}'),
    ("sbquo", '\u{201a}'),
    ("scaron", '\u{161}'),
    ("sdot", '\u{22c5}'),
    ("sect", '\u{a7}'),
    ("shy", '\u{ad}'),
    ("sigma", '\u{3c3}'),
    ("sigmaf", '\u{3c2}'),
    ("sim", '\u{223c}'),
    ("spades", '\u{2660}'),
    ("sub", '\u{2282}'),
    ("sube", '\u{2286}'),
    ("sum", '\u{2211}'),
    ("sup", '\u{2283}'),
    ("sup1", '\u{b9}'),
    ("sup2", '\u{b2}'),
    ("sup3", '\u{b3}'),
    ("supe", '\u{2287}'),
    ("szlig", '\u{df}'),
    ("tau", '\u{3c4}'),
    ("there4", '\u{2234}'),
    ("theta", '\u{3b8}'),
    ("thetasym", '\u{3d1}'),
    ("thinsp", '\u{2009}'),
    ("thorn", '\u{fe}'),
    ("tilde", '\u{2dc}'),
    ("times", '\u{d7}'),
    ("trade", '\u{2122}'),
    ("uArr", '\u{21d1}'),
    ("uacute", '\u{fa}'),
    ("uarr", '\u{2191}'),
    ("ucirc", '\u{fb}'),
    ("ugrave", '\u{f9}'),
    ("uml", '\u{a8}'),
    ("upsih", '\u{3d2}'),
    ("upsilon", '\u{3c5}'),
    ("uuml", '\u{fc}'),
    ("weierp", '\u{2118}'),
    ("xi", '\u{3be}'),
    ("yacute", '\u{fd}'),
    ("yen", '\u{a5}'),
    ("yuml", '\u{ff}'),
    ("zeta", '\u{3b6}'),
    ("zwj", '\u{200d}'),
    ("zwnj", '\u{200c}'),
];

/// The names HTML also accepts without a semicolon, sorted: `&amp` reads as
/// `&amp;`. Generated from Python's `html.entities.html5`.
const LEGACY_REFERENCES: &[&str] = &[
    "AElig", "AMP", "Aacute", "Acirc", "Agrave", "Aring", "Atilde", "Auml", "COPY", "Ccedil",
    "ETH", "Eacute", "Ecirc", "Egrave", "Euml", "GT", "Iacute", "Icirc", "Igrave", "Iuml", "LT",
    "Ntilde", "Oacute", "Ocirc", "Ograve", "Oslash", "Otilde", "Ouml", "QUOT", "REG", "THORN",
    "Uacute", "Ucirc", "Ugrave", "Uuml", "Yacute", "aacute", "acirc", "acute", "aelig", "agrave",
    "amp", "aring", "atilde", "auml", "brvbar", "ccedil", "cedil", "cent", "copy", "curren", "deg",
    "divide", "eacute", "ecirc", "egrave", "eth", "euml", "frac12", "frac14", "frac34", "gt",
    "iacute", "icirc", "iexcl", "igrave", "iquest", "iuml", "laquo", "lt", "macr", "micro",
    "middot", "nbsp", "not", "ntilde", "oacute", "ocirc", "ograve", "ordf", "ordm", "oslash",
    "otilde", "ouml", "para", "plusmn", "pound", "quot", "raquo", "reg", "sect", "shy", "sup1",
    "sup2", "sup3", "szlig", "thorn", "times", "uacute", "ucirc", "ugrave", "uml", "uuml",
    "yacute", "yen", "yuml",
];
