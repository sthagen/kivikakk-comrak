use crate::arena_tree::Node;
use crate::ctype::{isdigit, ispunct, isspace};
use crate::entity;
use crate::nodes::{
    Ast, AstNode, NodeCode, NodeFootnoteReference, NodeLink, NodeMath, NodeValue, NodeWikiLink,
    Sourcepos,
};
use crate::parser::autolink;
#[cfg(feature = "shortcodes")]
use crate::parser::shortcodes::NodeShortCode;
use crate::parser::{
    unwrap_into_2, unwrap_into_copy, AutolinkType, BrokenLinkReference, Options, ResolvedReference,
};
use crate::scanners;
use crate::strings::{self, is_blank, Case};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::ptr;
use std::str;
use typed_arena::Arena;
use unicode_categories::UnicodeCategories;

use super::WikiLinksMode;

const MAXBACKTICKS: usize = 80;
const MAX_LINK_LABEL_LENGTH: usize = 1000;
const MAX_MATH_DOLLARS: usize = 2;

trait FlankingCheckHelper
where
    Self: Sized + Copy,
{
    fn is_cjk_ambiguous_punctuation_candidate(&self) -> bool;
    fn is_ideographic_vs(&self) -> bool;
    fn is_cjk(&self) -> bool;
    #[inline]
    fn is_cjk_or_ideographic_vs(&self) -> bool {
        self.is_cjk() || self.is_ideographic_vs()
    }
    fn is_non_emoji_general_purpose_vs(&self) -> bool;
    fn is_cmark_punctuation(&self) -> bool;
}

impl FlankingCheckHelper for char {
    /// https://github.com/tats-u/markdown-cjk-friendly/blob/main/ranges.md#cjk-characters
    #[inline]
    fn is_cjk(&self) -> bool {
        // Snapshot as of Unicode 16
        matches!(
            u32::from(*self),
            0x1100..=0x11ff
              | 0x20a9
              | 0x2329..=0x232a
              | 0x2630..=0x2637
              | 0x268a..=0x268f
              | 0x2e80..=0x2e99
              | 0x2e9b..=0x2ef3
              | 0x2f00..=0x2fd5
              | 0x2ff0..=0x303e
              | 0x3041..=0x3096
              | 0x3099..=0x30ff
              | 0x3105..=0x312f
              | 0x3131..=0x318e
              | 0x3190..=0x31e5
              | 0x31ef..=0x321e
              | 0x3220..=0x3247
              | 0x3250..=0xa48c
              | 0xa490..=0xa4c6
              | 0xa960..=0xa97c
              | 0xac00..=0xd7a3
              | 0xd7b0..=0xd7c6
              | 0xd7cb..=0xd7fb
              | 0xf900..=0xfaff
              | 0xfe10..=0xfe19
              | 0xfe30..=0xfe52
              | 0xfe54..=0xfe66
              | 0xfe68..=0xfe6b
              | 0xff01..=0xffbe
              | 0xffc2..=0xffc7
              | 0xffca..=0xffcf
              | 0xffd2..=0xffd7
              | 0xffda..=0xffdc
              | 0xffe0..=0xffe6
              | 0xffe8..=0xffee
              | 0x16fe0..=0x16fe4
              | 0x16ff0..=0x16ff1
              | 0x17000..=0x187f7
              | 0x18800..=0x18cd5
              | 0x18cff..=0x18d08
              | 0x1aff0..=0x1aff3
              | 0x1aff5..=0x1affb
              | 0x1affd..=0x1affe
              | 0x1b000..=0x1b122
              | 0x1b132
              | 0x1b150..=0x1b152
              | 0x1b155
              | 0x1b164..=0x1b167
              | 0x1b170..=0x1b2fb
              | 0x1d300..=0x1d356
              | 0x1d360..=0x1d376
              | 0x1f200
              | 0x1f202
              | 0x1f210..=0x1f219
              | 0x1f21b..=0x1f22e
              | 0x1f230..=0x1f231
              | 0x1f237
              | 0x1f23b
              | 0x1f240..=0x1f248
              | 0x1f260..=0x1f265
              | 0x20000..=0x3fffd
        )
    }

    #[inline]
    fn is_non_emoji_general_purpose_vs(&self) -> bool {
        matches!(u32::from(*self), 0xFE00..=0xFE0F)
    }

    #[inline]
    fn is_ideographic_vs(&self) -> bool {
        matches!(u32::from(*self), 0xE0100..=0xE01EF)
    }
    #[inline]
    fn is_cmark_punctuation(&self) -> bool {
        self.is_punctuation() || self.is_symbol()
    }
    #[inline]
    fn is_cjk_ambiguous_punctuation_candidate(&self) -> bool {
        matches!(u32::from(*self), 0x2018 | 0x2019 | 0x201c | 0x201d)
    }
}

pub struct Subject<'a: 'd, 'r, 'o, 'd, 'i, 'c> {
    pub arena: &'a Arena<AstNode<'a>>,
    options: &'o Options<'c>,
    pub input: &'i [u8],
    line: usize,
    pub pos: usize,
    column_offset: isize,
    line_offset: usize,
    flags: Flags,
    pub refmap: &'r mut RefMap,
    delimiter_arena: &'d Arena<Delimiter<'a, 'd>>,
    last_delimiter: Option<&'d Delimiter<'a, 'd>>,
    brackets: Vec<Bracket<'a>>,
    within_brackets: bool,
    pub backticks: [usize; MAXBACKTICKS + 1],
    pub scanned_for_backticks: bool,
    no_link_openers: bool,
    special_chars: [bool; 256],
    skip_chars: [bool; 256],
    smart_chars: [bool; 256],
}

#[derive(Default)]
struct Flags {
    skip_html_cdata: bool,
    skip_html_declaration: bool,
    skip_html_pi: bool,
    skip_html_comment: bool,
}

pub struct RefMap {
    pub map: HashMap<String, ResolvedReference>,
    pub(crate) max_ref_size: usize,
    ref_size: usize,
}

impl RefMap {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            max_ref_size: usize::MAX,
            ref_size: 0,
        }
    }

    fn lookup(&mut self, lab: &str) -> Option<ResolvedReference> {
        match self.map.get(lab) {
            Some(entry) => {
                let size = entry.url.len() + entry.title.len();
                if size > self.max_ref_size - self.ref_size {
                    None
                } else {
                    self.ref_size += size;
                    Some(entry.clone())
                }
            }
            None => None,
        }
    }
}

pub struct Delimiter<'a: 'd, 'd> {
    inl: &'a AstNode<'a>,
    position: usize,
    length: usize,
    delim_char: u8,
    can_open: bool,
    can_close: bool,
    prev: Cell<Option<&'d Delimiter<'a, 'd>>>,
    next: Cell<Option<&'d Delimiter<'a, 'd>>>,
}

impl<'a: 'd, 'd> std::fmt::Debug for Delimiter<'a, 'd> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[pos {}, len {}, delim_char {:?}, open? {} close? {} -- {}]",
            self.position,
            self.length,
            self.delim_char,
            self.can_open,
            self.can_close,
            self.inl.data.borrow().sourcepos
        )
    }
}

struct Bracket<'a> {
    inl_text: &'a AstNode<'a>,
    position: usize,
    image: bool,
    bracket_after: bool,
}

#[derive(Clone, Copy)]
struct WikilinkComponents<'i> {
    url: &'i [u8],
    link_label: Option<(&'i [u8], usize, usize)>,
}

impl<'a, 'r, 'o, 'd, 'i, 'c> Subject<'a, 'r, 'o, 'd, 'i, 'c> {
    pub fn new(
        arena: &'a Arena<AstNode<'a>>,
        options: &'o Options<'c>,
        input: &'i [u8],
        line: usize,
        refmap: &'r mut RefMap,
        delimiter_arena: &'d Arena<Delimiter<'a, 'd>>,
    ) -> Self {
        let mut s = Subject {
            arena,
            options,
            input,
            line,
            pos: 0,
            column_offset: 0,
            line_offset: 0,
            flags: Flags::default(),
            refmap,
            delimiter_arena,
            last_delimiter: None,
            brackets: vec![],
            within_brackets: false,
            backticks: [0; MAXBACKTICKS + 1],
            scanned_for_backticks: false,
            no_link_openers: true,
            special_chars: [false; 256],
            skip_chars: [false; 256],
            smart_chars: [false; 256],
        };
        for &c in &[
            b'\n', b'\r', b'_', b'*', b'"', b'`', b'\\', b'&', b'<', b'[', b']', b'!', b'$',
        ] {
            s.special_chars[c as usize] = true;
        }
        if options.extension.autolink {
            s.special_chars[b':' as usize] = true;
            s.special_chars[b'w' as usize] = true;
        }
        if options.extension.strikethrough || options.extension.subscript {
            s.special_chars[b'~' as usize] = true;
            s.skip_chars[b'~' as usize] = true;
        }
        if options.extension.superscript {
            s.special_chars[b'^' as usize] = true;
        }
        #[cfg(feature = "shortcodes")]
        if options.extension.shortcodes {
            s.special_chars[b':' as usize] = true;
        }
        if options.extension.underline {
            s.special_chars[b'_' as usize] = true;
        }
        if options.extension.spoiler {
            s.special_chars[b'|' as usize] = true;
        }
        for &c in b"\"'.-" {
            s.smart_chars[c as usize] = true;
        }
        s
    }

    pub fn pop_bracket(&mut self) -> bool {
        self.brackets.pop().is_some()
    }

    pub fn parse_inline(&mut self, node: &'a AstNode<'a>) -> bool {
        let c = match self.peek_char() {
            None => return false,
            Some(ch) => *ch as char,
        };

        let node_ast = node.data.borrow();
        let adjusted_line = self.line - node_ast.sourcepos.start.line;
        self.line_offset = node_ast.line_offsets[adjusted_line];

        let new_inl: Option<&'a AstNode<'a>> = match c {
            '\0' => return false,
            '\r' | '\n' => Some(self.handle_newline()),
            '`' => Some(self.handle_backticks(&node_ast.line_offsets)),
            '\\' => Some(self.handle_backslash()),
            '&' => Some(self.handle_entity()),
            '<' => Some(self.handle_pointy_brace(&node_ast.line_offsets)),
            ':' => {
                let mut res = None;

                if self.options.extension.autolink {
                    res = self.handle_autolink_colon(node);
                }

                #[cfg(feature = "shortcodes")]
                if res.is_none() && self.options.extension.shortcodes {
                    res = self.handle_shortcodes_colon();
                }

                if res.is_none() {
                    self.pos += 1;
                    res = Some(self.make_inline(
                        NodeValue::Text(":".to_string()),
                        self.pos - 1,
                        self.pos - 1,
                    ));
                }

                res
            }
            'w' if self.options.extension.autolink => match self.handle_autolink_w(node) {
                Some(inl) => Some(inl),
                None => {
                    self.pos += 1;
                    Some(self.make_inline(
                        NodeValue::Text("w".to_string()),
                        self.pos - 1,
                        self.pos - 1,
                    ))
                }
            },
            '*' | '_' | '\'' | '"' => Some(self.handle_delim(c as u8)),
            '-' => Some(self.handle_hyphen()),
            '.' => Some(self.handle_period()),
            '[' => {
                self.pos += 1;

                let mut wikilink_inl = None;

                if self.options.extension.wikilinks().is_some()
                    && !self.within_brackets
                    && self.peek_char() == Some(&(b'['))
                {
                    wikilink_inl = self.handle_wikilink();
                }

                if wikilink_inl.is_none() {
                    let inl = self.make_inline(
                        NodeValue::Text("[".to_string()),
                        self.pos - 1,
                        self.pos - 1,
                    );
                    self.push_bracket(false, inl);
                    self.within_brackets = true;

                    Some(inl)
                } else {
                    wikilink_inl
                }
            }
            ']' => {
                self.within_brackets = false;
                self.handle_close_bracket()
            }
            '!' => {
                self.pos += 1;
                if self.peek_char() == Some(&(b'[')) && self.peek_char_n(1) != Some(&(b'^')) {
                    self.pos += 1;
                    let inl = self.make_inline(
                        NodeValue::Text("![".to_string()),
                        self.pos - 2,
                        self.pos - 1,
                    );
                    self.push_bracket(true, inl);
                    self.within_brackets = true;
                    Some(inl)
                } else {
                    Some(self.make_inline(
                        NodeValue::Text("!".to_string()),
                        self.pos - 1,
                        self.pos - 1,
                    ))
                }
            }
            '~' if self.options.extension.strikethrough || self.options.extension.subscript => {
                Some(self.handle_delim(b'~'))
            }
            '^' if self.options.extension.superscript && !self.within_brackets => {
                Some(self.handle_delim(b'^'))
            }
            '$' => Some(self.handle_dollars(&node_ast.line_offsets)),
            '|' if self.options.extension.spoiler => Some(self.handle_delim(b'|')),
            _ => {
                let mut endpos = self.find_special_char();
                let mut contents = self.input[self.pos..endpos].to_vec();
                let mut startpos = self.pos;
                self.pos = endpos;

                if self
                    .peek_char()
                    .map_or(false, |&c| strings::is_line_end_char(c))
                {
                    endpos -= strings::rtrim(&mut contents);
                }

                // if we've just produced a LineBreak, then we should consume any leading
                // space on this line
                if node.last_child().map_or(false, |n| {
                    matches!(n.data.borrow().value, NodeValue::LineBreak)
                }) {
                    // TODO: test this more explicitly.
                    let n = strings::ltrim(&mut contents);
                    startpos += n;
                }

                Some(self.make_inline(
                    NodeValue::Text(String::from_utf8(contents).unwrap()),
                    startpos,
                    endpos - 1,
                ))
            }
        };

        if let Some(inl) = new_inl {
            node.append(inl);
        }

        true
    }

    fn del_ref_eq(lhs: Option<&'d Delimiter<'a, 'd>>, rhs: Option<&'d Delimiter<'a, 'd>>) -> bool {
        match (lhs, rhs) {
            (None, None) => true,
            (Some(l), Some(r)) => ptr::eq(l, r),
            _ => false,
        }
    }

    // After parsing a block (and sometimes during), this function traverses the
    // stack of `Delimiters`, tokens ("*", "_", etc.) that may delimit regions
    // of text for special rendering: emphasis, strong, superscript, subscript,
    // spoilertext; looking for pairs of opening and closing delimiters,
    // with the goal of placing the intervening nodes into new emphasis,
    // etc AST nodes.
    //
    // The term stack here is a bit of a misnomer, as the `Delimiters` actually
    // form a doubly-linked list. Items are pushed onto the stack during parsing,
    // but during post-processing are removed from arbitrary locations.
    //
    // The `Delimiter` contains references AST `Text` nodes, which are also
    // linked into the AST as siblings in the order they are parsed. This
    // function doesn't know a-priori which ones are markdown syntax and which
    // are just text: candidate delimiters that match have their nodes removed
    // from the AST, as they are markdown, and their intervening siblings
    // lowered into a new AST parent node via the `insert_emph` function;
    // candidate delimiters that don't match are left in the tree.
    //
    // The basic algorithm is to start at the bottom of the stack, walk upwards
    // looking for closing delimiters, and from each closing delimiter walk back
    // down the stack looking for its matching opening delimiter. This traversal
    // favors the smallest matching leftmost pairs, e.g.
    //
    //   _a *b c_ d* e_
    //    ~~~~~~
    //
    // (The emphasis region is wavy-underlined)
    //
    // All of the `_` and `*` tokens are scanned as candidates, but only the
    // region "a *b c" is lowered into an `Emph` node; the other candidate
    // delimiters are all actually text.
    //
    // And in
    //
    //   _a _b c_
    //       ~~~
    //
    // "b c" is the emphasis region, not "a _b c".
    //
    // Note that Delimiters are matched by comparing their `delim_char`, which
    // is simply a value used to compare opening and closing delimiters - the
    // actual text value of the scanned token can theoretically be different.
    //
    // There's some additional trickiness in the logic because "_", "__", and
    // "___" (and etc. etc.) all share the same delim_char, but represent
    // different emphasis. Note also that "_"- and "*"-delimited regions have
    // complex rules for which can be opening and/or closing delimiters,
    // determined in `scan_delims`.
    pub fn process_emphasis(&mut self, stack_bottom: usize) {
        // This array is an important optimization that prevents searching down
        // the stack for openers we've previously searched for and know don't
        // exist, preventing exponential blowup on pathological cases.
        let mut openers_bottom: [usize; 12] = [stack_bottom; 12];

        // This is traversing the stack from the top to the bottom, setting `closer` to
        // the delimiter directly above `stack_bottom`. In the case where we are processing
        // emphasis on an entire block, `stack_bottom` is `None`, so `closer` references
        // the very bottom of the stack.
        let mut candidate = self.last_delimiter;
        let mut closer: Option<&Delimiter> = None;
        while candidate.map_or(false, |c| c.position >= stack_bottom) {
            closer = candidate;
            candidate = candidate.unwrap().prev.get();
        }

        while let Some(c) = closer {
            if c.can_close {
                // Each time through the outer `closer` loop we reset the opener
                // to the element below the closer, and search down the stack
                // for a matching opener.

                let mut opener = c.prev.get();
                let mut opener_found = false;
                let mut mod_three_rule_invoked = false;

                let ix = match c.delim_char {
                    b'|' => 0,
                    b'~' => 1,
                    b'^' => 2,
                    b'"' => 3,
                    b'\'' => 4,
                    b'_' => 5,
                    b'*' => 6 + (if c.can_open { 3 } else { 0 }) + (c.length % 3),
                    _ => unreachable!(),
                };

                // Here's where we find the opener by searching down the stack,
                // looking for matching delims with the `can_open` flag.
                // On any invocation, on the first time through the outer
                // `closer` loop, this inner `opener` search doesn't succeed:
                // when processing a full block, `opener` starts out `None`;
                // when processing emphasis otherwise, opener will be equal to
                // `stack_bottom`.
                //
                // This search short-circuits for openers we've previously
                // failed to find, avoiding repeatedly rescanning the bottom of
                // the stack, using the openers_bottom array.
                while opener.map_or(false, |o| o.position >= openers_bottom[ix]) {
                    let o = opener.unwrap();
                    if o.can_open && o.delim_char == c.delim_char {
                        // This is a bit convoluted; see points 9 and 10 here:
                        // http://spec.commonmark.org/0.28/#can-open-emphasis.
                        // This is to aid processing of runs like this:
                        // “***hello*there**” or “***hello**there*”. In this
                        // case, the middle delimiter can both open and close
                        // emphasis; when trying to find an opening delimiter
                        // that matches the last ** or *, we need to skip it,
                        // and this algorithm ensures we do. (The sum of the
                        // lengths are a multiple of 3.)
                        let odd_match = (c.can_open || o.can_close)
                            && ((o.length + c.length) % 3 == 0)
                            && !(o.length % 3 == 0 && c.length % 3 == 0);
                        if !odd_match {
                            opener_found = true;
                            break;
                        } else {
                            mod_three_rule_invoked = true;
                        }
                    }
                    opener = o.prev.get();
                }

                let old_c = c;

                // There's a case here for every possible delimiter. If we found
                // a matching opening delimiter for our closing delimiter, they
                // both get passed.
                if c.delim_char == b'*'
                    || c.delim_char == b'_'
                    || ((self.options.extension.strikethrough || self.options.extension.subscript)
                        && c.delim_char == b'~')
                    || (self.options.extension.superscript && c.delim_char == b'^')
                    || (self.options.extension.spoiler && c.delim_char == b'|')
                {
                    if opener_found {
                        // Finally, here's the happy case where the delimiters
                        // match and they are inserted. We get a new closer
                        // delimiter and go around the loop again.
                        //
                        // Note that for "***" and "___" delimiters of length
                        // greater than 2, insert_emph will create a `Strong`
                        // node (i.e. "**"), then _truncate_ the delimiters in
                        // place, turning them into e.g. "*" delimiters, and
                        // hand us back the same mutated closer to be matched
                        // again.
                        //
                        // In general though the closer will be the next
                        // delimiter up the stack.
                        closer = self.insert_emph(opener.unwrap(), c);
                    } else {
                        // When no matching opener is found we move the closer
                        // up the stack, do some bookkeeping with old_closer
                        // (below), try again.
                        closer = c.next.get();
                    }
                } else if c.delim_char == b'\'' || c.delim_char == b'"' {
                    *c.inl.data.borrow_mut().value.text_mut().unwrap() =
                        if c.delim_char == b'\'' { "’" } else { "”" }.to_string();
                    closer = c.next.get();

                    if opener_found {
                        *opener
                            .unwrap()
                            .inl
                            .data
                            .borrow_mut()
                            .value
                            .text_mut()
                            .unwrap() = if old_c.delim_char == b'\'' {
                            "‘"
                        } else {
                            "“"
                        }
                        .to_string();
                        self.remove_delimiter(opener.unwrap());
                        self.remove_delimiter(old_c);
                    }
                }

                // If the search for an opener was unsuccessful, then record
                // the position the search started at in the `openers_bottom`
                // so that the `opener` search can avoid looking for this
                // same opener at the bottom of the stack later.
                if !opener_found {
                    if !mod_three_rule_invoked {
                        openers_bottom[ix] = old_c.position;
                    }

                    // Now that we've failed the `opener` search starting from
                    // `old_closer`, future opener searches will be searching it
                    // for openers - if `old_closer` can't be used as an opener
                    // then we know it's just text - remove it from the
                    // delimiter stack, leaving it in the AST as text
                    if !old_c.can_open {
                        self.remove_delimiter(old_c);
                    }
                }
            } else {
                // Closer is !can_close. Move up the stack
                closer = c.next.get();
            }
        }

        // At this point the entire delimiter stack from `stack_bottom` up has
        // been scanned for matches, everything left is just text. Pop it all
        // off.
        self.remove_delimiters(stack_bottom);
    }

    fn remove_delimiter(&mut self, delimiter: &'d Delimiter<'a, 'd>) {
        if delimiter.next.get().is_none() {
            assert!(ptr::eq(delimiter, self.last_delimiter.unwrap()));
            self.last_delimiter = delimiter.prev.get();
        } else {
            delimiter.next.get().unwrap().prev.set(delimiter.prev.get());
        }
        if delimiter.prev.get().is_some() {
            delimiter.prev.get().unwrap().next.set(delimiter.next.get());
        }
    }

    fn remove_delimiters(&mut self, stack_bottom: usize) {
        while self
            .last_delimiter
            .map_or(false, |d| d.position >= stack_bottom)
        {
            self.remove_delimiter(self.last_delimiter.unwrap());
        }
    }

    #[inline]
    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    #[inline]
    pub fn peek_char(&self) -> Option<&u8> {
        self.peek_char_n(0)
    }

    #[inline]
    fn peek_char_n(&self, n: usize) -> Option<&u8> {
        if self.pos + n >= self.input.len() {
            None
        } else {
            let c = &self.input[self.pos + n];
            assert!(*c > 0);
            Some(c)
        }
    }

    fn find_special_char(&self) -> usize {
        for n in self.pos..self.input.len() {
            if self.special_chars[self.input[n] as usize] {
                if self.input[n] == b'^' && self.within_brackets {
                    // NO OP
                } else {
                    return n;
                }
            }
            if self.options.parse.smart && self.smart_chars[self.input[n] as usize] {
                return n;
            }
        }

        self.input.len()
    }

    fn adjust_node_newlines(
        &mut self,
        node: &'a AstNode<'a>,
        matchlen: usize,
        extra: usize,
        parent_line_offsets: &[usize],
    ) {
        let (newlines, since_newline) =
            count_newlines(&self.input[self.pos - matchlen - extra..self.pos - extra]);

        if newlines > 0 {
            self.line += newlines;
            let node_ast = &mut node.data.borrow_mut();
            node_ast.sourcepos.end.line += newlines;
            let adjusted_line = self.line - node_ast.sourcepos.start.line;
            node_ast.sourcepos.end.column =
                parent_line_offsets[adjusted_line] + since_newline + extra;
            self.column_offset = -(self.pos as isize) + since_newline as isize + extra as isize;
        }
    }

    fn handle_newline(&mut self) -> &'a AstNode<'a> {
        let nlpos = self.pos;
        if self.input[self.pos] == b'\r' {
            self.pos += 1;
        }
        if self.input[self.pos] == b'\n' {
            self.pos += 1;
        }
        let inl = if nlpos > 1 && self.input[nlpos - 1] == b' ' && self.input[nlpos - 2] == b' ' {
            self.make_inline(NodeValue::LineBreak, nlpos - 2, self.pos - 1)
        } else {
            self.make_inline(NodeValue::SoftBreak, nlpos, self.pos - 1)
        };
        self.line += 1;
        self.column_offset = -(self.pos as isize);
        self.skip_spaces();
        inl
    }

    fn take_while(&mut self, c: u8) -> usize {
        let start_pos = self.pos;
        while self.peek_char() == Some(&c) {
            self.pos += 1;
        }
        self.pos - start_pos
    }

    fn take_while_with_limit(&mut self, c: u8, limit: usize) -> usize {
        let start_pos = self.pos;
        let mut count = 0;
        while count < limit && self.peek_char() == Some(&c) {
            self.pos += 1;
            count += 1;
        }
        self.pos - start_pos
    }

    fn scan_to_closing_backtick(&mut self, openticklength: usize) -> Option<usize> {
        if openticklength > MAXBACKTICKS {
            return None;
        }

        if self.scanned_for_backticks && self.backticks[openticklength] <= self.pos {
            return None;
        }

        loop {
            while self.peek_char().map_or(false, |&c| c != b'`') {
                self.pos += 1;
            }
            if self.pos >= self.input.len() {
                self.scanned_for_backticks = true;
                return None;
            }
            let numticks = self.take_while(b'`');
            if numticks <= MAXBACKTICKS {
                self.backticks[numticks] = self.pos - numticks;
            }
            if numticks == openticklength {
                return Some(self.pos);
            }
        }
    }

    fn handle_backticks(&mut self, parent_line_offsets: &[usize]) -> &'a AstNode<'a> {
        let startpos = self.pos;
        let openticks = self.take_while(b'`');
        let endpos = self.scan_to_closing_backtick(openticks);

        match endpos {
            None => {
                self.pos = startpos + openticks;
                self.make_inline(
                    NodeValue::Text("`".repeat(openticks)),
                    startpos,
                    self.pos - 1,
                )
            }
            Some(endpos) => {
                let buf = &self.input[startpos + openticks..endpos - openticks];
                let buf = strings::normalize_code(buf);
                let code = NodeCode {
                    num_backticks: openticks,
                    literal: String::from_utf8(buf).unwrap(),
                };
                let node = self.make_inline(NodeValue::Code(code), startpos, endpos - 1);
                self.adjust_node_newlines(
                    node,
                    endpos - startpos - openticks,
                    openticks,
                    parent_line_offsets,
                );
                node
            }
        }
    }

    fn scan_to_closing_dollar(&mut self, opendollarlength: usize) -> Option<usize> {
        if !self.options.extension.math_dollars || opendollarlength > MAX_MATH_DOLLARS {
            return None;
        }

        // space not allowed after initial $
        if opendollarlength == 1 && self.peek_char().map_or(false, |&c| isspace(c)) {
            return None;
        }

        loop {
            while self.peek_char().map_or(false, |&c| c != b'$') {
                self.pos += 1;
            }

            if self.pos >= self.input.len() {
                return None;
            }

            let c = self.input[self.pos - 1];

            // space not allowed before ending $
            if opendollarlength == 1 && isspace(c) {
                return None;
            }

            // dollar signs must also be backslash-escaped if they occur within math
            if opendollarlength == 1 && c == b'\\' {
                self.pos += 1;
                continue;
            }

            let numdollars = self.take_while_with_limit(b'$', opendollarlength);

            // ending $ can't be followed by a digit
            if opendollarlength == 1 && self.peek_char().map_or(false, |&c| isdigit(c)) {
                return None;
            }

            if numdollars == opendollarlength {
                return Some(self.pos);
            }
        }
    }

    fn scan_to_closing_code_dollar(&mut self) -> Option<usize> {
        assert!(self.options.extension.math_code);

        loop {
            while self.peek_char().map_or(false, |&c| c != b'$') {
                self.pos += 1;
            }

            if self.pos >= self.input.len() {
                return None;
            }

            let c = self.input[self.pos - 1];
            self.pos += 1;
            if c == b'`' {
                return Some(self.pos);
            }
        }
    }

    // Heuristics used from https://pandoc.org/MANUAL.html#extension-tex_math_dollars
    fn handle_dollars(&mut self, parent_line_offsets: &[usize]) -> &'a AstNode<'a> {
        if !(self.options.extension.math_dollars || self.options.extension.math_code) {
            self.pos += 1;
            return self.make_inline(NodeValue::Text("$".to_string()), self.pos - 1, self.pos - 1);
        }
        let startpos = self.pos;
        let opendollars = self.take_while(b'$');
        let mut code_math = false;

        // check for code math
        if opendollars == 1
            && self.options.extension.math_code
            && self.peek_char().map_or(false, |&c| c == b'`')
        {
            code_math = true;
            self.pos += 1;
        }
        let fence_length = if code_math { 2 } else { opendollars };

        let endpos: Option<usize> = if code_math {
            self.scan_to_closing_code_dollar()
        } else {
            self.scan_to_closing_dollar(opendollars)
        }
        .filter(|endpos| endpos - startpos >= fence_length * 2 + 1);

        if let Some(endpos) = endpos {
            let buf = &self.input[startpos + fence_length..endpos - fence_length];
            let buf: Vec<u8> = if code_math || opendollars == 1 {
                strings::normalize_code(buf)
            } else {
                buf.to_vec()
            };
            let math = NodeMath {
                dollar_math: !code_math,
                display_math: opendollars == 2,
                literal: String::from_utf8(buf).unwrap(),
            };
            let node = self.make_inline(NodeValue::Math(math), startpos, endpos - 1);
            self.adjust_node_newlines(
                node,
                endpos - startpos - fence_length,
                fence_length,
                parent_line_offsets,
            );
            node
        } else if code_math {
            self.pos = startpos + 1;
            self.make_inline(NodeValue::Text("$".to_string()), self.pos - 1, self.pos - 1)
        } else {
            self.pos = startpos + fence_length;
            self.make_inline(
                NodeValue::Text("$".repeat(opendollars)),
                self.pos - fence_length,
                self.pos - 1,
            )
        }
    }

    pub fn skip_spaces(&mut self) -> bool {
        let mut skipped = false;
        while self.peek_char().map_or(false, |&c| c == b' ' || c == b'\t') {
            self.pos += 1;
            skipped = true;
        }
        skipped
    }

    fn handle_delim(&mut self, c: u8) -> &'a AstNode<'a> {
        let (numdelims, can_open, can_close) = self.scan_delims(c);

        let contents = if c == b'\'' && self.options.parse.smart {
            "’".to_string()
        } else if c == b'"' && self.options.parse.smart {
            if can_close {
                "”".to_string()
            } else {
                "“".to_string()
            }
        } else {
            str::from_utf8(&self.input[self.pos - numdelims..self.pos])
                .unwrap()
                .to_string()
        };
        let inl = self.make_inline(
            NodeValue::Text(contents),
            self.pos - numdelims,
            self.pos - 1,
        );

        if (can_open || can_close) && (!(c == b'\'' || c == b'"') || self.options.parse.smart) {
            self.push_delimiter(c, can_open, can_close, inl);
        }

        inl
    }

    fn handle_hyphen(&mut self) -> &'a AstNode<'a> {
        let start = self.pos;
        self.pos += 1;

        if !self.options.parse.smart || self.peek_char().map_or(true, |&c| c != b'-') {
            return self.make_inline(NodeValue::Text("-".to_string()), self.pos - 1, self.pos - 1);
        }

        while self.options.parse.smart && self.peek_char().map_or(false, |&c| c == b'-') {
            self.pos += 1;
        }

        let numhyphens = (self.pos - start) as i32;

        let (ens, ems) = if numhyphens % 3 == 0 {
            (0, numhyphens / 3)
        } else if numhyphens % 2 == 0 {
            (numhyphens / 2, 0)
        } else if numhyphens % 3 == 2 {
            (1, (numhyphens - 2) / 3)
        } else {
            (2, (numhyphens - 4) / 3)
        };

        let ens = if ens > 0 { ens as usize } else { 0 };
        let ems = if ems > 0 { ems as usize } else { 0 };

        let mut buf = String::with_capacity(3 * (ems + ens));
        buf.push_str(&"—".repeat(ems));
        buf.push_str(&"–".repeat(ens));
        self.make_inline(NodeValue::Text(buf), start, self.pos - 1)
    }

    fn handle_period(&mut self) -> &'a AstNode<'a> {
        self.pos += 1;
        if self.options.parse.smart && self.peek_char().map_or(false, |&c| c == b'.') {
            self.pos += 1;
            if self.peek_char().map_or(false, |&c| c == b'.') {
                self.pos += 1;
                self.make_inline(NodeValue::Text("…".to_string()), self.pos - 3, self.pos - 1)
            } else {
                self.make_inline(
                    NodeValue::Text("..".to_string()),
                    self.pos - 2,
                    self.pos - 1,
                )
            }
        } else {
            self.make_inline(NodeValue::Text(".".to_string()), self.pos - 1, self.pos - 1)
        }
    }

    #[inline]
    fn get_before_char(&self, pos: usize) -> (char, Option<usize>) {
        if pos == 0 {
            return ('\n', None);
        }
        let mut before_char_pos = pos - 1;
        while before_char_pos > 0
            && (self.input[before_char_pos] >> 6 == 2
                || self.skip_chars[self.input[before_char_pos] as usize])
        {
            before_char_pos -= 1;
        }
        match unsafe { str::from_utf8_unchecked(&self.input[before_char_pos..pos]) }
            .chars()
            .next()
        {
            Some(x) => {
                if (x as usize) < 256 && self.skip_chars[x as usize] {
                    ('\n', None)
                } else {
                    (x, Some(before_char_pos))
                }
            }
            None => ('\n', None),
        }
    }

    fn scan_delims(&mut self, c: u8) -> (usize, bool, bool) {
        let (before_char, before_char_pos) = self.get_before_char(self.pos);

        let mut numdelims = 0;
        if c == b'\'' || c == b'"' {
            numdelims += 1;
            self.pos += 1;
        } else {
            while self.peek_char() == Some(&c) {
                numdelims += 1;
                self.pos += 1;
            }
        }

        let after_char = if self.eof() {
            '\n'
        } else {
            let mut after_char_pos = self.pos;
            while after_char_pos < self.input.len() - 1
                && self.skip_chars[self.input[after_char_pos] as usize]
            {
                after_char_pos += 1;
            }
            match unsafe { str::from_utf8_unchecked(&self.input[after_char_pos..]) }
                .chars()
                .next()
            {
                Some(x) => {
                    if (x as usize) < 256 && self.skip_chars[x as usize] {
                        '\n'
                    } else {
                        x
                    }
                }
                None => '\n',
            }
        };

        let cjk_friendly = self.options.extension.cjk_friendly_emphasis;
        let mut two_before_char: Option<char> = None;

        let left_flanking = numdelims > 0
            && !after_char.is_whitespace()
            && (!after_char.is_cmark_punctuation()
                || before_char.is_whitespace()
                || if !cjk_friendly {
                    before_char.is_cmark_punctuation()
                } else {
                    after_char.is_cjk()
                        || if before_char.is_non_emoji_general_purpose_vs() {
                            if let Some(before_char_pos) = before_char_pos {
                                let (two_before_char_, _) = self.get_before_char(before_char_pos);
                                two_before_char = Some(two_before_char_);
                                two_before_char_.is_cjk()
                                    || two_before_char_.is_cmark_punctuation()
                                    || two_before_char_.is_cjk_ambiguous_punctuation_candidate()
                                        && before_char == '\u{fe01}'
                            } else {
                                false
                            }
                        } else {
                            before_char.is_cjk_or_ideographic_vs()
                                || before_char.is_cmark_punctuation()
                        }
                });
        let right_flanking = numdelims > 0
            && !before_char.is_whitespace()
            && (!if !cjk_friendly {
                before_char.is_cmark_punctuation()
            } else {
                !after_char.is_cjk()
                    && if before_char.is_non_emoji_general_purpose_vs() {
                        let two_before_char = if let Some(two_before_char_) = two_before_char {
                            two_before_char_
                        } else if let Some(before_char_pos) = before_char_pos {
                            let (two_before_char_, _) = self.get_before_char(before_char_pos);
                            two_before_char = Some(two_before_char_);
                            two_before_char_
                        } else {
                            '\n'
                        };
                        !two_before_char.is_cjk()
                            && two_before_char.is_cmark_punctuation()
                            && !(two_before_char.is_cjk_ambiguous_punctuation_candidate()
                                && before_char == '\u{fe01}')
                    } else {
                        !before_char.is_cjk() && before_char.is_cmark_punctuation()
                    }
            } || after_char.is_whitespace()
                || after_char.is_cmark_punctuation());

        if c == b'_' {
            (
                numdelims,
                left_flanking
                    && (!right_flanking
                        || if !(cjk_friendly && before_char.is_non_emoji_general_purpose_vs()) {
                            before_char.is_cmark_punctuation()
                        } else {
                            let two_before_char = if let Some(two_before_char_) = two_before_char {
                                two_before_char_
                            } else if let Some(before_char_pos) = before_char_pos {
                                self.get_before_char(before_char_pos).0
                            } else {
                                '\n'
                            };
                            two_before_char.is_cmark_punctuation()
                        }),
                right_flanking && (!left_flanking || after_char.is_cmark_punctuation()),
            )
        } else if c == b'\'' || c == b'"' {
            (
                numdelims,
                left_flanking
                    && (!right_flanking || before_char == '(' || before_char == '[')
                    && before_char != ']'
                    && before_char != ')',
                right_flanking,
            )
        } else {
            (numdelims, left_flanking, right_flanking)
        }
    }

    fn push_delimiter(&mut self, c: u8, can_open: bool, can_close: bool, inl: &'a AstNode<'a>) {
        let d = self.delimiter_arena.alloc(Delimiter {
            prev: Cell::new(self.last_delimiter),
            next: Cell::new(None),
            inl,
            position: self.pos,
            length: inl.data.borrow().value.text().unwrap().len(),
            delim_char: c,
            can_open,
            can_close,
        });
        if d.prev.get().is_some() {
            d.prev.get().unwrap().next.set(Some(d));
        }
        self.last_delimiter = Some(d);
    }

    // Create a new emphasis node, move all the nodes between `opener`
    // and `closer` into it, and insert it into the AST.
    //
    // As a side-effect, handle long "***" and "___" nodes by truncating them in
    // place to be re-matched by `process_emphasis`.
    fn insert_emph(
        &mut self,
        opener: &'d Delimiter<'a, 'd>,
        closer: &'d Delimiter<'a, 'd>,
    ) -> Option<&'d Delimiter<'a, 'd>> {
        let opener_char = opener.inl.data.borrow().value.text().unwrap().as_bytes()[0];
        let mut opener_num_chars = opener.inl.data.borrow().value.text().unwrap().len();
        let mut closer_num_chars = closer.inl.data.borrow().value.text().unwrap().len();
        let use_delims = if closer_num_chars >= 2 && opener_num_chars >= 2 {
            2
        } else {
            1
        };

        opener_num_chars -= use_delims;
        closer_num_chars -= use_delims;

        if (self.options.extension.strikethrough || self.options.extension.subscript)
            && opener_char == b'~'
            && (opener_num_chars != closer_num_chars || opener_num_chars > 0)
        {
            return None;
        }

        opener
            .inl
            .data
            .borrow_mut()
            .value
            .text_mut()
            .unwrap()
            .truncate(opener_num_chars);
        closer
            .inl
            .data
            .borrow_mut()
            .value
            .text_mut()
            .unwrap()
            .truncate(closer_num_chars);

        // Remove all the candidate delimiters from between the opener and the
        // closer. None of them are matched pairs. They've been scanned already.
        let mut delim = closer.prev.get();
        while delim.is_some() && !Self::del_ref_eq(delim, Some(opener)) {
            self.remove_delimiter(delim.unwrap());
            delim = delim.unwrap().prev.get();
        }

        let emph = self.make_inline(
            if self.options.extension.subscript && opener_char == b'~' && use_delims == 1 {
                NodeValue::Subscript
            } else if opener_char == b'~' {
                // Not emphasis
                // Unlike for |, these cases have to be handled because they will match
                // in the event subscript but not strikethrough is enabled
                if self.options.extension.strikethrough {
                    NodeValue::Strikethrough
                } else if use_delims == 1 {
                    NodeValue::EscapedTag("~".to_owned())
                } else {
                    NodeValue::EscapedTag("~~".to_owned())
                }
            } else if self.options.extension.superscript && opener_char == b'^' {
                NodeValue::Superscript
            } else if self.options.extension.spoiler && opener_char == b'|' {
                if use_delims == 2 {
                    NodeValue::SpoileredText
                } else {
                    NodeValue::EscapedTag("|".to_owned())
                }
            } else if self.options.extension.underline && opener_char == b'_' && use_delims == 2 {
                NodeValue::Underline
            } else if use_delims == 1 {
                NodeValue::Emph
            } else {
                NodeValue::Strong
            },
            // These are overriden immediately below.
            self.pos,
            self.pos,
        );

        emph.data.borrow_mut().sourcepos = (
            opener.inl.data.borrow().sourcepos.start.line,
            opener.inl.data.borrow().sourcepos.start.column + opener_num_chars,
            closer.inl.data.borrow().sourcepos.end.line,
            closer.inl.data.borrow().sourcepos.end.column - closer_num_chars,
        )
            .into();

        // Drop all the interior AST nodes into the emphasis node
        // and then insert the emphasis node
        let mut tmp = opener.inl.next_sibling().unwrap();
        while !tmp.same_node(closer.inl) {
            let next = tmp.next_sibling();
            emph.append(tmp);
            if let Some(n) = next {
                tmp = n;
            } else {
                break;
            }
        }
        opener.inl.insert_after(emph);

        // Drop completely "used up" delimiters, adjust sourcepos of those not,
        // and return the next closest one for processing.
        if opener_num_chars == 0 {
            opener.inl.detach();
            self.remove_delimiter(opener);
        } else {
            opener.inl.data.borrow_mut().sourcepos.end.column -= use_delims;
        }

        if closer_num_chars == 0 {
            closer.inl.detach();
            self.remove_delimiter(closer);
            closer.next.get()
        } else {
            closer.inl.data.borrow_mut().sourcepos.start.column += use_delims;
            Some(closer)
        }
    }

    fn handle_backslash(&mut self) -> &'a AstNode<'a> {
        let startpos = self.pos;
        self.pos += 1;

        if self.peek_char().map_or(false, |&c| ispunct(c)) {
            let inl;
            self.pos += 1;

            let inline_text = self.make_inline(
                NodeValue::Text(String::from_utf8(vec![self.input[self.pos - 1]]).unwrap()),
                self.pos - 2,
                self.pos - 1,
            );

            if self.options.render.escaped_char_spans {
                inl = self.make_inline(NodeValue::Escaped, self.pos - 2, self.pos - 1);
                inl.append(inline_text);
                inl
            } else {
                inline_text
            }
        } else if !self.eof() && self.skip_line_end() {
            let inl = self.make_inline(NodeValue::LineBreak, startpos, self.pos - 1);
            self.line += 1;
            self.column_offset = -(self.pos as isize);
            self.skip_spaces();
            inl
        } else {
            self.make_inline(
                NodeValue::Text("\\".to_string()),
                self.pos - 1,
                self.pos - 1,
            )
        }
    }

    pub fn skip_line_end(&mut self) -> bool {
        let old_pos = self.pos;
        if self.peek_char() == Some(&(b'\r')) {
            self.pos += 1;
        }
        if self.peek_char() == Some(&(b'\n')) {
            self.pos += 1;
        }
        self.pos > old_pos || self.eof()
    }

    fn handle_entity(&mut self) -> &'a AstNode<'a> {
        self.pos += 1;

        match entity::unescape(&self.input[self.pos..]) {
            None => self.make_inline(NodeValue::Text("&".to_string()), self.pos - 1, self.pos - 1),
            Some((entity, len)) => {
                self.pos += len;
                self.make_inline(
                    NodeValue::Text(String::from_utf8(entity).unwrap()),
                    self.pos - 1 - len,
                    self.pos - 1,
                )
            }
        }
    }

    #[cfg(feature = "shortcodes")]
    fn handle_shortcodes_colon(&mut self) -> Option<&'a AstNode<'a>> {
        let matchlen = scanners::shortcode(&self.input[self.pos + 1..])?;

        let shortcode = unsafe {
            str::from_utf8_unchecked(&self.input[self.pos + 1..self.pos + 1 + matchlen - 1])
        };

        let nsc = NodeShortCode::resolve(shortcode)?;
        self.pos += 1 + matchlen;

        Some(self.make_inline(
            NodeValue::ShortCode(nsc),
            self.pos - 1 - matchlen,
            self.pos - 1,
        ))
    }

    fn handle_autolink_with<F>(&mut self, node: &'a AstNode<'a>, f: F) -> Option<&'a AstNode<'a>>
    where
        F: Fn(
            &'a Arena<AstNode<'a>>,
            &[u8],
            usize,
            bool,
        ) -> Option<(&'a AstNode<'a>, usize, usize)>,
    {
        if !self.options.parse.relaxed_autolinks && self.within_brackets {
            return None;
        }
        let startpos = self.pos;
        let (post, need_reverse, skip) = f(
            self.arena,
            self.input,
            self.pos,
            self.options.parse.relaxed_autolinks,
        )?;

        self.pos += skip - need_reverse;

        // We need to "rewind" by `need_reverse` chars, which should be in one
        // or more Text nodes beforehand. Typically the chars will *all* be in
        // a single Text node, containing whatever text came before the ":" that
        // triggered this method, eg. "See our website at http" ("://blah.com").
        //
        // relaxed_autolinks allows some slightly pathological cases. First,
        // "://…" is a possible parse, meaning `reverse == 0`. There may also be
        // a scheme including the letter "w", which will split Text inlines due
        // to them being their own trigger (for handle_autolink_w), meaning
        // "wa://…" will need to traverse two Texts to complete the rewind.
        let mut reverse = need_reverse;
        while reverse > 0 {
            let mut last_child = node.last_child().unwrap().data.borrow_mut();
            match last_child.value {
                NodeValue::Text(ref mut prev) => {
                    if reverse < prev.len() {
                        prev.truncate(prev.len() - reverse);
                        last_child.sourcepos.end.column -= reverse;
                        reverse = 0;
                    } else {
                        reverse -= prev.len();
                        node.last_child().unwrap().detach();
                    }
                }
                _ => panic!("expected text node before autolink colon"),
            }
        }

        {
            let sp = &mut post.data.borrow_mut().sourcepos;
            // See [`make_inline`].
            sp.start = (
                self.line,
                (startpos as isize - need_reverse as isize
                    + 1
                    + self.column_offset
                    + self.line_offset as isize) as usize,
            )
                .into();
            sp.end = (
                self.line,
                (self.pos as isize + self.column_offset + self.line_offset as isize) as usize,
            )
                .into();

            // Inner text node gets the same sp, since there are no surrounding
            // characters for autolinks of these kind.
            post.first_child().unwrap().data.borrow_mut().sourcepos = *sp;
        }

        Some(post)
    }

    fn handle_autolink_colon(&mut self, node: &'a AstNode<'a>) -> Option<&'a AstNode<'a>> {
        self.handle_autolink_with(node, autolink::url_match)
    }

    fn handle_autolink_w(&mut self, node: &'a AstNode<'a>) -> Option<&'a AstNode<'a>> {
        self.handle_autolink_with(node, autolink::www_match)
    }

    fn handle_pointy_brace(&mut self, parent_line_offsets: &[usize]) -> &'a AstNode<'a> {
        self.pos += 1;

        if let Some(matchlen) = scanners::autolink_uri(&self.input[self.pos..]) {
            self.pos += matchlen;
            let inl = self.make_autolink(
                &self.input[self.pos - matchlen..self.pos - 1],
                AutolinkType::Uri,
                self.pos - 1 - matchlen,
                self.pos - 1,
            );
            return inl;
        }

        if let Some(matchlen) = scanners::autolink_email(&self.input[self.pos..]) {
            self.pos += matchlen;
            let inl = self.make_autolink(
                &self.input[self.pos - matchlen..self.pos - 1],
                AutolinkType::Email,
                self.pos - 1 - matchlen,
                self.pos - 1,
            );
            return inl;
        }

        // Most comments below are verbatim from cmark upstream.
        let mut matchlen: Option<usize> = None;

        if self.pos + 2 <= self.input.len() {
            let c = self.input[self.pos];
            if c == b'!' && !self.flags.skip_html_comment {
                let c = self.input[self.pos + 1];
                if c == b'-' && self.peek_char_n(2) == Some(&b'-') {
                    if self.peek_char_n(3) == Some(&b'>') {
                        matchlen = Some(4);
                    } else if self.peek_char_n(3) == Some(&b'-')
                        && self.peek_char_n(4) == Some(&b'>')
                    {
                        matchlen = Some(5);
                    } else if let Some(m) = scanners::html_comment(&self.input[self.pos + 1..]) {
                        matchlen = Some(m + 1);
                    } else {
                        self.flags.skip_html_comment = true;
                    }
                } else if c == b'[' {
                    if !self.flags.skip_html_cdata && self.pos + 3 <= self.input.len() {
                        if let Some(m) = scanners::html_cdata(&self.input[self.pos + 2..]) {
                            // The regex doesn't require the final "]]>". But if we're not at
                            // the end of input, it must come after the match. Otherwise,
                            // disable subsequent scans to avoid quadratic behavior.

                            // Adding 5 to matchlen for prefix "![", suffix "]]>"
                            if self.pos + m + 5 > self.input.len() {
                                self.flags.skip_html_cdata = true;
                            } else {
                                matchlen = Some(m + 5);
                            }
                        }
                    }
                } else if !self.flags.skip_html_declaration {
                    if let Some(m) = scanners::html_declaration(&self.input[self.pos + 1..]) {
                        // Adding 2 to matchlen for prefix "!", suffix ">"
                        if self.pos + m + 2 > self.input.len() {
                            self.flags.skip_html_declaration = true;
                        } else {
                            matchlen = Some(m + 2);
                        }
                    }
                }
            } else if c == b'?' {
                if !self.flags.skip_html_pi {
                    // Note that we allow an empty match.
                    let m = scanners::html_processing_instruction(&self.input[self.pos + 1..])
                        .unwrap_or(0);
                    // Adding 3 to matchlen fro prefix "?", suffix "?>"
                    if self.pos + m + 3 > self.input.len() {
                        self.flags.skip_html_pi = true;
                    } else {
                        matchlen = Some(m + 3);
                    }
                }
            } else {
                matchlen = scanners::html_tag(&self.input[self.pos..]);
            }
        }

        if let Some(matchlen) = matchlen {
            let contents = &self.input[self.pos - 1..self.pos + matchlen];
            self.pos += matchlen;
            let inl = self.make_inline(
                NodeValue::HtmlInline(str::from_utf8(contents).unwrap().to_string()),
                self.pos - matchlen - 1,
                self.pos - 1,
            );
            self.adjust_node_newlines(inl, matchlen, 1, parent_line_offsets);
            return inl;
        }

        self.make_inline(NodeValue::Text("<".to_string()), self.pos - 1, self.pos - 1)
    }

    fn push_bracket(&mut self, image: bool, inl_text: &'a AstNode<'a>) {
        let len = self.brackets.len();
        if len > 0 {
            self.brackets[len - 1].bracket_after = true;
        }
        self.brackets.push(Bracket {
            inl_text,
            position: self.pos,
            image,
            bracket_after: false,
        });
        if !image {
            self.no_link_openers = false;
        }
    }

    fn handle_close_bracket(&mut self) -> Option<&'a AstNode<'a>> {
        self.pos += 1;
        let initial_pos = self.pos;

        let brackets_len = self.brackets.len();
        if brackets_len == 0 {
            return Some(self.make_inline(
                NodeValue::Text("]".to_string()),
                self.pos - 1,
                self.pos - 1,
            ));
        }

        let is_image = self.brackets[brackets_len - 1].image;

        if !is_image && self.no_link_openers {
            self.brackets.pop();
            return Some(self.make_inline(
                NodeValue::Text("]".to_string()),
                self.pos - 1,
                self.pos - 1,
            ));
        }

        // Ensure there was text if this was a link and not an image link
        if self.options.render.ignore_empty_links && !is_image {
            let mut non_blank_found = false;
            let mut tmpch = self.brackets[brackets_len - 1].inl_text.next_sibling();
            while let Some(tmp) = tmpch {
                match tmp.data.borrow().value {
                    NodeValue::Text(ref s) if is_blank(s.as_bytes()) => (),
                    _ => {
                        non_blank_found = true;
                        break;
                    }
                }

                tmpch = tmp.next_sibling();
            }

            if !non_blank_found {
                self.brackets.pop();
                return Some(self.make_inline(
                    NodeValue::Text("]".to_string()),
                    self.pos - 1,
                    self.pos - 1,
                ));
            }
        }

        let after_link_text_pos = self.pos;

        // Try to find a link destination within parenthesis

        let mut sps = 0;
        let mut url: &[u8] = &[];
        let mut n: usize = 0;
        if self.peek_char() == Some(&(b'(')) && {
            sps = scanners::spacechars(&self.input[self.pos + 1..]).unwrap_or(0);
            let offset = self.pos + 1 + sps;
            offset < self.input.len()
                && unwrap_into_2(
                    manual_scan_link_url(&self.input[offset..]),
                    &mut url,
                    &mut n,
                )
        } {
            let starturl = self.pos + 1 + sps;
            let endurl = starturl + n;
            let starttitle = endurl + scanners::spacechars(&self.input[endurl..]).unwrap_or(0);
            let endtitle = if starttitle == endurl {
                starttitle
            } else {
                starttitle + scanners::link_title(&self.input[starttitle..]).unwrap_or(0)
            };
            let endall = endtitle + scanners::spacechars(&self.input[endtitle..]).unwrap_or(0);

            if endall < self.input.len() && self.input[endall] == b')' {
                self.pos = endall + 1;
                let url = strings::clean_url(url);
                let title = strings::clean_title(&self.input[starttitle..endtitle]);
                self.close_bracket_match(
                    is_image,
                    String::from_utf8(url).unwrap(),
                    String::from_utf8(title).unwrap(),
                );
                return None;
            } else {
                self.pos = after_link_text_pos;
            }
        }

        // Try to see if this is a reference link

        let (mut lab, mut found_label) = match self.link_label() {
            Some(lab) => (lab.to_string(), true),
            None => ("".to_string(), false),
        };

        if !found_label {
            self.pos = initial_pos;
        }

        if (!found_label || lab.is_empty()) && !self.brackets[brackets_len - 1].bracket_after {
            lab = str::from_utf8(
                &self.input[self.brackets[brackets_len - 1].position..initial_pos - 1],
            )
            .unwrap()
            .to_string();
            found_label = true;
        }

        // Need to normalize both to lookup in refmap and to call callback
        let unfolded_lab = lab.to_owned();
        let lab = strings::normalize_label(&lab, Case::Fold);
        let mut reff = if found_label {
            self.refmap.lookup(&lab)
        } else {
            None
        };

        // Attempt to use the provided broken link callback if a reference cannot be resolved
        if reff.is_none() {
            if let Some(callback) = &self.options.parse.broken_link_callback {
                reff = callback.resolve(BrokenLinkReference {
                    normalized: &lab,
                    original: &unfolded_lab,
                });
            }
        }

        if let Some(reff) = reff {
            self.close_bracket_match(is_image, reff.url.clone(), reff.title);
            return None;
        }

        let bracket_inl_text = self.brackets[brackets_len - 1].inl_text;

        if self.options.extension.footnotes
            && match bracket_inl_text.next_sibling() {
                Some(n) => {
                    if n.data.borrow().value.text().is_some() {
                        n.data
                            .borrow()
                            .value
                            .text()
                            .unwrap()
                            .as_bytes()
                            .starts_with(b"^")
                    } else {
                        false
                    }
                }
                _ => false,
            }
        {
            let mut text = String::new();
            let mut sibling_iterator = bracket_inl_text.following_siblings();

            self.pos = initial_pos;

            // Skip the initial node, which holds the `[`
            sibling_iterator.next().unwrap();

            // The footnote name could have been parsed into multiple text/htmlinline nodes.
            // For example `[^_foo]` gives `^`, `_`, and `foo`. So pull them together.
            // Since we're handling the closing bracket, the only siblings at this point are
            // related to the footnote name.
            for sibling in sibling_iterator {
                match sibling.data.borrow().value {
                    NodeValue::Text(ref literal) | NodeValue::HtmlInline(ref literal) => {
                        text.push_str(literal);
                    }
                    _ => {}
                };
            }

            if text.len() > 1 {
                let inl = self.make_inline(
                    NodeValue::FootnoteReference(NodeFootnoteReference {
                        name: text[1..].to_string(),
                        ref_num: 0,
                        ix: 0,
                    }),
                    // Overridden immediately below.
                    self.pos,
                    self.pos,
                );
                inl.data.borrow_mut().sourcepos.start.column =
                    bracket_inl_text.data.borrow().sourcepos.start.column;
                inl.data.borrow_mut().sourcepos.end.column = usize::try_from(
                    self.pos as isize + self.column_offset + self.line_offset as isize,
                )
                .unwrap();
                bracket_inl_text.insert_before(inl);

                // detach all the nodes, including bracket_inl_text
                sibling_iterator = bracket_inl_text.following_siblings();
                for sibling in sibling_iterator {
                    match sibling.data.borrow().value {
                        NodeValue::Text(_) | NodeValue::HtmlInline(_) => {
                            sibling.detach();
                        }
                        _ => {}
                    };
                }

                // We don't need to process emphasis for footnote names, so cleanup
                // any outstanding delimiters
                self.remove_delimiters(self.brackets[brackets_len - 1].position);

                self.brackets.pop();
                return None;
            }
        }

        self.brackets.pop();
        self.pos = initial_pos;
        Some(self.make_inline(NodeValue::Text("]".to_string()), self.pos - 1, self.pos - 1))
    }

    fn close_bracket_match(&mut self, is_image: bool, url: String, title: String) {
        let brackets_len = self.brackets.len();

        let nl = NodeLink { url, title };
        let inl = self.make_inline(
            if is_image {
                NodeValue::Image(nl)
            } else {
                NodeValue::Link(nl)
            },
            // Manually set below.
            self.pos,
            self.pos,
        );
        inl.data.borrow_mut().sourcepos.start = self.brackets[brackets_len - 1]
            .inl_text
            .data
            .borrow()
            .sourcepos
            .start;
        inl.data.borrow_mut().sourcepos.end.column =
            usize::try_from(self.pos as isize + self.column_offset + self.line_offset as isize)
                .unwrap();

        self.brackets[brackets_len - 1].inl_text.insert_before(inl);
        let mut tmpch = self.brackets[brackets_len - 1].inl_text.next_sibling();
        while let Some(tmp) = tmpch {
            tmpch = tmp.next_sibling();
            inl.append(tmp);
        }
        self.brackets[brackets_len - 1].inl_text.detach();
        self.process_emphasis(self.brackets[brackets_len - 1].position);
        self.brackets.pop();

        if !is_image {
            self.no_link_openers = true;
        }
    }

    pub fn link_label(&mut self) -> Option<&str> {
        let startpos = self.pos;

        if self.peek_char() != Some(&(b'[')) {
            return None;
        }

        self.pos += 1;

        let mut length = 0;
        let mut c = 0;
        while unwrap_into_copy(self.peek_char(), &mut c) && c != b'[' && c != b']' {
            if c == b'\\' {
                self.pos += 1;
                length += 1;
                if self.peek_char().map_or(false, |&c| ispunct(c)) {
                    self.pos += 1;
                    length += 1;
                }
            } else {
                self.pos += 1;
                length += 1;
            }
            if length > MAX_LINK_LABEL_LENGTH {
                self.pos = startpos;
                return None;
            }
        }

        if c == b']' {
            let raw_label = strings::trim_slice(&self.input[startpos + 1..self.pos]);
            self.pos += 1;
            Some(str::from_utf8(raw_label).unwrap())
        } else {
            self.pos = startpos;
            None
        }
    }

    // Handles wikilink syntax
    //   [[link text|url]]
    //   [[url|link text]]
    fn handle_wikilink(&mut self) -> Option<&'a AstNode<'a>> {
        let startpos = self.pos;
        let component = self.wikilink_url_link_label()?;
        let url_clean = strings::clean_url(component.url);
        let (link_label, link_label_start_column, _link_label_end_column) =
            match component.link_label {
                Some((label, sc, ec)) => (entity::unescape_html(label), sc, ec),
                None => (
                    entity::unescape_html(component.url),
                    startpos + 1,
                    self.pos - 3,
                ),
            };

        let nl = NodeWikiLink {
            url: String::from_utf8(url_clean).unwrap(),
        };
        let inl = self.make_inline(NodeValue::WikiLink(nl), startpos - 1, self.pos - 1);

        self.label_backslash_escapes(inl, link_label, link_label_start_column);

        Some(inl)
    }

    fn wikilink_url_link_label(&mut self) -> Option<WikilinkComponents<'i>> {
        let left_startpos = self.pos;

        if self.peek_char() != Some(&(b'[')) {
            return None;
        }

        let found_left = self.wikilink_component();

        if !found_left {
            self.pos = left_startpos;
            return None;
        }

        let left = strings::trim_slice(&self.input[left_startpos + 1..self.pos]);

        if self.peek_char() == Some(&(b']')) && self.peek_char_n(1) == Some(&(b']')) {
            self.pos += 2;
            return Some(WikilinkComponents {
                url: left,
                link_label: None,
            });
        } else if self.peek_char() != Some(&(b'|')) {
            self.pos = left_startpos;
            return None;
        }

        let right_startpos = self.pos;
        let found_right = self.wikilink_component();

        if !found_right {
            self.pos = left_startpos;
            return None;
        }

        let right = strings::trim_slice(&self.input[right_startpos + 1..self.pos]);

        if self.peek_char() == Some(&(b']')) && self.peek_char_n(1) == Some(&(b']')) {
            self.pos += 2;

            match self.options.extension.wikilinks() {
                Some(WikiLinksMode::UrlFirst) => Some(WikilinkComponents {
                    url: left,
                    link_label: Some((right, right_startpos + 1, self.pos - 3)),
                }),
                Some(WikiLinksMode::TitleFirst) => Some(WikilinkComponents {
                    url: right,
                    link_label: Some((left, left_startpos + 1, right_startpos - 1)),
                }),
                None => unreachable!(),
            }
        } else {
            self.pos = left_startpos;
            None
        }
    }

    // Locates the edge of a wikilink component (link label or url), and sets the
    // self.pos to it's end if it's found.
    fn wikilink_component(&mut self) -> bool {
        let startpos = self.pos;

        if self.peek_char() != Some(&(b'[')) && self.peek_char() != Some(&(b'|')) {
            return false;
        }

        self.pos += 1;

        let mut length = 0;
        let mut c = 0;
        while unwrap_into_copy(self.peek_char(), &mut c) && c != b'[' && c != b']' && c != b'|' {
            if c == b'\\' {
                self.pos += 1;
                length += 1;
                if self.peek_char().map_or(false, |&c| ispunct(c)) {
                    self.pos += 1;
                    length += 1;
                }
            } else {
                self.pos += 1;
                length += 1;
            }
            if length > MAX_LINK_LABEL_LENGTH {
                self.pos = startpos;
                return false;
            }
        }

        true
    }

    // Given a label, handles backslash escaped characters. Appends the resulting
    // nodes to the container
    fn label_backslash_escapes(
        &mut self,
        container: &'a AstNode<'a>,
        label: Vec<u8>,
        start_column: usize,
    ) {
        let mut startpos = 0;
        let mut offset = 0;
        let len = label.len();

        while offset < len {
            let c = label[offset];

            if c == b'\\' && (offset + 1) < len && ispunct(label[offset + 1]) {
                let preceding_text = self.make_inline(
                    NodeValue::Text(String::from_utf8(label[startpos..offset].to_owned()).unwrap()),
                    start_column + startpos,
                    start_column + offset - 1,
                );

                container.append(preceding_text);

                let inline_text = self.make_inline(
                    NodeValue::Text(String::from_utf8(vec![label[offset + 1]]).unwrap()),
                    start_column + offset,
                    start_column + offset + 1,
                );

                if self.options.render.escaped_char_spans {
                    let span = self.make_inline(
                        NodeValue::Escaped,
                        start_column + offset,
                        start_column + offset + 1,
                    );

                    span.append(inline_text);
                    container.append(span);
                } else {
                    container.append(inline_text);
                }

                offset += 2;
                startpos = offset;
            } else {
                offset += 1;
            }
        }

        if startpos != offset {
            container.append(self.make_inline(
                NodeValue::Text(String::from_utf8(label[startpos..offset].to_owned()).unwrap()),
                start_column + startpos,
                start_column + offset - 1,
            ));
        }
    }

    pub fn spnl(&mut self) {
        self.skip_spaces();
        if self.skip_line_end() {
            self.skip_spaces();
        }
    }

    fn make_inline(
        &self,
        value: NodeValue,
        start_column: usize,
        end_column: usize,
    ) -> &'a AstNode<'a> {
        let start_column =
            start_column as isize + 1 + self.column_offset + self.line_offset as isize;
        let end_column = end_column as isize + 1 + self.column_offset + self.line_offset as isize;

        let ast = Ast {
            value,
            content: String::new(),
            sourcepos: (
                self.line,
                usize::try_from(start_column).unwrap(),
                self.line,
                usize::try_from(end_column).unwrap(),
            )
                .into(),
            internal_offset: 0,
            open: false,
            last_line_blank: false,
            table_visited: false,
            line_offsets: Vec::with_capacity(0),
        };
        self.arena.alloc(Node::new(RefCell::new(ast)))
    }

    fn make_autolink(
        &self,
        url: &[u8],
        kind: AutolinkType,
        start_column: usize,
        end_column: usize,
    ) -> &'a AstNode<'a> {
        let inl = self.make_inline(
            NodeValue::Link(NodeLink {
                url: String::from_utf8(strings::clean_autolink(url, kind)).unwrap(),
                title: String::new(),
            }),
            start_column,
            end_column,
        );
        inl.append(self.make_inline(
            NodeValue::Text(String::from_utf8(entity::unescape_html(url)).unwrap()),
            start_column + 1,
            end_column - 1,
        ));
        inl
    }
}

pub fn manual_scan_link_url(input: &[u8]) -> Option<(&[u8], usize)> {
    let len = input.len();
    let mut i = 0;

    if i < len && input[i] == b'<' {
        i += 1;
        while i < len {
            let b = input[i];
            if b == b'>' {
                i += 1;
                break;
            } else if b == b'\\' {
                i += 2;
            } else if b == b'\n' || b == b'<' {
                return None;
            } else {
                i += 1;
            }
        }
    } else {
        return manual_scan_link_url_2(input);
    }

    if i >= len {
        None
    } else {
        Some((&input[1..i - 1], i))
    }
}

pub fn manual_scan_link_url_2(input: &[u8]) -> Option<(&[u8], usize)> {
    let len = input.len();
    let mut i = 0;
    let mut nb_p = 0;

    while i < len {
        if input[i] == b'\\' && i + 1 < len && ispunct(input[i + 1]) {
            i += 2;
        } else if input[i] == b'(' {
            nb_p += 1;
            i += 1;
            if nb_p > 32 {
                return None;
            }
        } else if input[i] == b')' {
            if nb_p == 0 {
                break;
            }
            nb_p -= 1;
            i += 1;
        } else if isspace(input[i]) || input[i].is_ascii_control() {
            if i == 0 {
                return None;
            }
            break;
        } else {
            i += 1;
        }
    }

    if i >= len || nb_p != 0 {
        None
    } else {
        Some((&input[..i], i))
    }
}

pub fn make_inline<'a>(
    arena: &'a Arena<AstNode<'a>>,
    value: NodeValue,
    sourcepos: Sourcepos,
) -> &'a AstNode<'a> {
    let ast = Ast {
        value,
        content: String::new(),
        sourcepos,
        internal_offset: 0,
        open: false,
        last_line_blank: false,
        table_visited: false,
        line_offsets: Vec::with_capacity(0),
    };
    arena.alloc(Node::new(RefCell::new(ast)))
}

pub fn count_newlines(input: &[u8]) -> (usize, usize) {
    let mut nls = 0;
    let mut since_nl = 0;

    for &c in input {
        if c == b'\n' {
            nls += 1;
            since_nl = 0;
        } else {
            since_nl += 1;
        }
    }

    (nls, since_nl)
}
