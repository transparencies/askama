#![deny(unreachable_pub)]
#![deny(elided_lifetimes_in_paths)]

use std::cell::Cell;
use std::{fmt, str};

use nom::branch::alt;
use nom::bytes::complete::{escaped, is_not, tag, take_till};
use nom::character::complete::char;
use nom::character::complete::{anychar, digit1};
use nom::combinator::{eof, map, not, opt, recognize, value};
use nom::error::ErrorKind;
use nom::multi::separated_list1;
use nom::sequence::{delimited, pair, tuple};
use nom::{error_position, AsChar, IResult, InputTakeAtPosition};

pub use self::expr::Expr;
pub use self::node::{Cond, CondTest, Loop, Macro, Node, Target, When, Whitespace, Ws};

mod expr;
mod node;
#[cfg(test)]
mod tests;

mod _parsed {
    use std::mem;

    use super::{Ast, Node, ParseError, Syntax};

    pub struct Parsed {
        #[allow(dead_code)]
        source: String,
        ast: Ast<'static>,
    }

    impl Parsed {
        pub fn new(source: String, syntax: &Syntax<'_>) -> Result<Self, ParseError> {
            // Self-referential borrowing: `self` will keep the source alive as `String`,
            // internally we will transmute it to `&'static str` to satisfy the compiler.
            // However, we only expose the nodes with a lifetime limited to `self`.
            let src = unsafe { mem::transmute::<&str, &'static str>(source.as_str()) };
            let ast = match Ast::from_str(src, syntax) {
                Ok(ast) => ast,
                Err(e) => return Err(e),
            };

            Ok(Self { source, ast })
        }

        // The return value's lifetime must be limited to `self` to uphold the unsafe invariant.
        pub fn nodes(&self) -> &[Node<'_>] {
            &self.ast.nodes
        }
    }
}

pub use _parsed::Parsed;

#[derive(Debug)]
pub struct Ast<'a> {
    nodes: Vec<Node<'a>>,
}

impl<'a> Ast<'a> {
    pub fn from_str(src: &'a str, syntax: &Syntax<'_>) -> Result<Self, ParseError> {
        match Node::parse(src, &State::new(syntax)) {
            Ok((left, nodes)) => {
                if !left.is_empty() {
                    Err(ParseError(format!("unable to parse template:\n\n{left:?}")))
                } else {
                    Ok(Self { nodes })
                }
            }

            Err(nom::Err::Error(err)) | Err(nom::Err::Failure(err)) => {
                let nom::error::Error { input, .. } = err;
                let offset = src.len() - input.len();
                let (source_before, source_after) = src.split_at(offset);

                let source_after = match source_after.char_indices().enumerate().take(41).last() {
                    Some((40, (i, _))) => format!("{:?}...", &source_after[..i]),
                    _ => format!("{source_after:?}"),
                };

                let (row, last_line) = source_before.lines().enumerate().last().unwrap();
                let column = last_line.chars().count();

                let msg = format!(
                    "problems parsing template source at row {}, column {} near:\n{}",
                    row + 1,
                    column,
                    source_after,
                );

                Err(ParseError(msg))
            }

            Err(nom::Err::Incomplete(_)) => Err(ParseError("parsing incomplete".into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl std::error::Error for ParseError {}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

fn not_ws(c: char) -> bool {
    !is_ws(c)
}

fn ws<'a, O>(
    inner: impl FnMut(&'a str) -> IResult<&'a str, O>,
) -> impl FnMut(&'a str) -> IResult<&'a str, O> {
    delimited(take_till(not_ws), inner, take_till(not_ws))
}

fn split_ws_parts(s: &str) -> Node<'_> {
    let trimmed_start = s.trim_start_matches(is_ws);
    let len_start = s.len() - trimmed_start.len();
    let trimmed = trimmed_start.trim_end_matches(is_ws);
    Node::Lit(&s[..len_start], trimmed, &trimmed_start[trimmed.len()..])
}

/// Skips input until `end` was found, but does not consume it.
/// Returns tuple that would be returned when parsing `end`.
fn skip_till<'a, O>(
    end: impl FnMut(&'a str) -> IResult<&'a str, O>,
) -> impl FnMut(&'a str) -> IResult<&'a str, (&'a str, O)> {
    enum Next<O> {
        IsEnd(O),
        NotEnd(char),
    }
    let mut next = alt((map(end, Next::IsEnd), map(anychar, Next::NotEnd)));
    move |start: &'a str| {
        let mut i = start;
        loop {
            let (j, is_end) = next(i)?;
            match is_end {
                Next::IsEnd(lookahead) => return Ok((i, (j, lookahead))),
                Next::NotEnd(_) => i = j,
            }
        }
    }
}

fn keyword<'a>(k: &'a str) -> impl FnMut(&'a str) -> IResult<&'a str, &'a str> {
    move |i: &'a str| -> IResult<&'a str, &'a str> {
        let (j, v) = identifier(i)?;
        if k == v {
            Ok((j, v))
        } else {
            Err(nom::Err::Error(error_position!(i, ErrorKind::Tag)))
        }
    }
}

fn identifier(input: &str) -> IResult<&str, &str> {
    fn start(s: &str) -> IResult<&str, &str> {
        s.split_at_position1_complete(
            |c| !(c.is_alpha() || c == '_' || c >= '\u{0080}'),
            nom::error::ErrorKind::Alpha,
        )
    }

    fn tail(s: &str) -> IResult<&str, &str> {
        s.split_at_position1_complete(
            |c| !(c.is_alphanum() || c == '_' || c >= '\u{0080}'),
            nom::error::ErrorKind::Alpha,
        )
    }

    recognize(pair(start, opt(tail)))(input)
}

fn bool_lit(i: &str) -> IResult<&str, &str> {
    alt((keyword("false"), keyword("true")))(i)
}

fn num_lit(i: &str) -> IResult<&str, &str> {
    recognize(pair(digit1, opt(pair(char('.'), digit1))))(i)
}

fn str_lit(i: &str) -> IResult<&str, &str> {
    let (i, s) = delimited(
        char('"'),
        opt(escaped(is_not("\\\""), '\\', anychar)),
        char('"'),
    )(i)?;
    Ok((i, s.unwrap_or_default()))
}

fn char_lit(i: &str) -> IResult<&str, &str> {
    let (i, s) = delimited(
        char('\''),
        opt(escaped(is_not("\\\'"), '\\', anychar)),
        char('\''),
    )(i)?;
    Ok((i, s.unwrap_or_default()))
}

fn path(i: &str) -> IResult<&str, Vec<&str>> {
    let root = opt(value("", ws(tag("::"))));
    let tail = separated_list1(ws(tag("::")), identifier);

    match tuple((root, identifier, ws(tag("::")), tail))(i) {
        Ok((i, (root, start, _, rest))) => {
            let mut path = Vec::new();
            path.extend(root);
            path.push(start);
            path.extend(rest);
            Ok((i, path))
        }
        Err(err) => {
            if let Ok((i, name)) = identifier(i) {
                // The returned identifier can be assumed to be path if:
                // - Contains both a lowercase and uppercase character, i.e. a type name like `None`
                // - Doesn't contain any lowercase characters, i.e. it's a constant
                // In short, if it contains any uppercase characters it's a path.
                if name.contains(char::is_uppercase) {
                    return Ok((i, vec![name]));
                }
            }

            // If `identifier()` fails then just return the original error
            Err(err)
        }
    }
}

struct State<'a> {
    syntax: &'a Syntax<'a>,
    loop_depth: Cell<usize>,
}

impl<'a> State<'a> {
    fn new(syntax: &'a Syntax<'a>) -> State<'a> {
        State {
            syntax,
            loop_depth: Cell::new(0),
        }
    }

    fn take_content<'i>(&self, i: &'i str) -> IResult<&'i str, Node<'i>> {
        let p_start = alt((
            tag(self.syntax.block_start),
            tag(self.syntax.comment_start),
            tag(self.syntax.expr_start),
        ));

        let (i, _) = not(eof)(i)?;
        let (i, content) = opt(recognize(skip_till(p_start)))(i)?;
        let (i, content) = match content {
            Some("") => {
                // {block,comment,expr}_start follows immediately.
                return Err(nom::Err::Error(error_position!(i, ErrorKind::TakeUntil)));
            }
            Some(content) => (i, content),
            None => ("", i), // there is no {block,comment,expr}_start: take everything
        };
        Ok((i, split_ws_parts(content)))
    }

    fn tag_block_start<'i>(&self, i: &'i str) -> IResult<&'i str, &'i str> {
        tag(self.syntax.block_start)(i)
    }

    fn tag_block_end<'i>(&self, i: &'i str) -> IResult<&'i str, &'i str> {
        tag(self.syntax.block_end)(i)
    }

    fn tag_comment_start<'i>(&self, i: &'i str) -> IResult<&'i str, &'i str> {
        tag(self.syntax.comment_start)(i)
    }

    fn tag_comment_end<'i>(&self, i: &'i str) -> IResult<&'i str, &'i str> {
        tag(self.syntax.comment_end)(i)
    }

    fn tag_expr_start<'i>(&self, i: &'i str) -> IResult<&'i str, &'i str> {
        tag(self.syntax.expr_start)(i)
    }

    fn tag_expr_end<'i>(&self, i: &'i str) -> IResult<&'i str, &'i str> {
        tag(self.syntax.expr_end)(i)
    }

    fn enter_loop(&self) {
        self.loop_depth.set(self.loop_depth.get() + 1);
    }

    fn leave_loop(&self) {
        self.loop_depth.set(self.loop_depth.get() - 1);
    }

    fn is_in_loop(&self) -> bool {
        self.loop_depth.get() > 0
    }
}

#[derive(Debug)]
pub struct Syntax<'a> {
    pub block_start: &'a str,
    pub block_end: &'a str,
    pub expr_start: &'a str,
    pub expr_end: &'a str,
    pub comment_start: &'a str,
    pub comment_end: &'a str,
}

impl Default for Syntax<'static> {
    fn default() -> Self {
        Self {
            block_start: "{%",
            block_end: "%}",
            expr_start: "{{",
            expr_end: "}}",
            comment_start: "{#",
            comment_end: "#}",
        }
    }
}