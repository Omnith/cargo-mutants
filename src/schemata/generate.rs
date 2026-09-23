// Copyright 2026 Martin Pool

//! Pure generation of a mutant schema for one source file.
//!
//! A schema is the original source text with many mutants embedded at once. Each
//! mutant is guarded by a runtime check of the active mutant id, returned by
//! [`SITE_FN`], so that one build can test every embedded mutant.
//!
//! Mutation *sites* are byte ranges of the original text; they nest (for example a
//! binary operator inside a function body that also has `FnValue` mutants). A site
//! renders as a `match` on the id: one arm per mutant, whose text is the site with
//! just that mutant applied (and nested sites left as original text), and a final
//! `_` arm holding the original site text with nested sites rewritten recursively.
//!
//! All inserted text is on the line where the site starts, and alternative arms are
//! single-line, so line numbers of the original code are preserved.

#![warn(clippy::pedantic)]

use std::fmt::Write;

use itertools::Itertools;

/// Numeric id of a mutant within a schema; 0 means no mutant is active.
pub(crate) type MutantId = u32;

/// Function that returns the active mutant id at runtime, given the ids of the
/// mutants at the site that calls it, which it records as having run when no
/// mutant is active.
pub(crate) const SITE_FN: &str = "crate::__cargo_mutants_schemata::site";

/// Name of the environment variable that selects the active mutant at runtime.
pub(crate) const ID_ENV_VAR: &str = "CARGO_MUTANTS_SCHEMATA_ID";

/// Inner attribute inserted into each rewritten file so that the extra parentheses
/// and braces of the schema don't trip `#![deny(warnings)]`.
pub(crate) const LINT_ALLOW: &str =
    "#![allow(unused_parens, unused_braces, unreachable_code, unreachable_patterns)] ";

/// Match guard added to each mutant's arm, which records that the mutant was reached.
pub(crate) const REACHED_EXPR: &str = "crate::__cargo_mutants_schemata::reached()";

/// Function that is true unless the given mutant, which deletes a match arm, is active.
pub(crate) const KEEP_FN: &str = "crate::__cargo_mutants_schemata::keep";

/// The module appended to every crate root of a package with embedded mutants,
/// whose mutant ids are at most `max_id`.
///
/// Besides returning the active id, it records facts for cargo-mutants in files in
/// `marker_dir`:
///
/// - `reached-<id>` when the active mutant's code runs: the mutant can only change
///   test results if this exists.
/// - `unset-<pid>`, containing the executable's path, when a process runs schema
///   code without the id variable. cargo-mutants always sets it, so its absence means
///   the environment was cleared, for example by a test that runs a binary with
///   `env_clear()`, or that the code ran at build time. Such processes run the
///   unmutated code whatever mutant is being tested.
/// - `baseline-<pid>`, created when a process with the id variable set to 0 first
///   runs schema code, so that its existence shows that test processes see the
///   variable and can write markers. Each time a site first runs in that process, a
///   line with the ids of its mutants is appended, so the file lists the sites that
///   ran with no mutant active. Each line is written at once, before the site's code
///   runs, so it survives the process being killed.
///
/// Failing to create a marker is ignored, so that the tests behave the same.
///
/// `extern crate std` makes it work in `#![no_std]` crates when they are built for a
/// host that has `std`, as tests are. Paths are written in full, without `use`,
/// so that they resolve in any edition and without the prelude.
pub(crate) fn helper_module(marker_dir: &str, max_id: MutantId) -> String {
    let ids = max_id as usize + 1;
    format!(
        r#"
#[doc(hidden)]
#[allow(dead_code, unused_imports, unused_extern_crates)]
pub(crate) mod __cargo_mutants_schemata {{
    extern crate std;
    const MARKER_DIR: &str = {marker_dir:?};
    const NOT_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// For each mutant id, whether its site ran in this process with no mutant active.
    static SEEN: [std::sync::atomic::AtomicBool; {ids}] = [NOT_SEEN; {ids}];
    /// With no mutant active, the file listing the sites that ran.
    static BASELINE: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();
    /// Return the active mutant id from `{ID_ENV_VAR}`, or 0 for none.
    #[inline]
    pub(crate) fn id() -> u32 {{
        static ID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
        *ID.get_or_init(|| match std::env::var_os("{ID_ENV_VAR}") {{
            std::option::Option::Some(value) => {{
                let id = value.to_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                if id == 0 {{
                    open_baseline();
                }}
                id
            }}
            std::option::Option::None => {{
                let exe = std::env::current_exe().unwrap_or_default();
                let mut name = std::string::String::from("unset-");
                name.push_str(&std::string::ToString::to_string(&std::process::id()));
                mark(&name, exe.to_string_lossy().as_bytes());
                0
            }}
        }})
    }}
    /// Return the active mutant id, first recording that the site of mutants `ids`
    /// ran, if no mutant is active.
    #[inline]
    pub(crate) fn site(ids: &[u32]) -> u32 {{
        let id = id();
        if id == 0 {{
            seen(ids);
        }}
        id
    }}
    /// Record that the active mutant was reached; always true, for use as a match guard.
    #[inline]
    pub(crate) fn reached() -> bool {{
        static REACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !REACHED.load(std::sync::atomic::Ordering::Relaxed)
            && !REACHED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {{
            let mut name = std::string::String::from("reached-");
            name.push_str(&std::string::ToString::to_string(&id()));
            mark(&name, b"");
        }}
        true
    }}
    /// False if mutant `n`, which deletes a match arm, is active.
    #[inline]
    pub(crate) fn keep(n: u32) -> bool {{
        site(&[n]) != n || !reached()
    }}
    /// Record the first time the site of mutants `ids` runs, with no mutant active.
    #[inline]
    fn seen(ids: &[u32]) {{
        if let std::option::Option::Some(flag) = ids.first().and_then(|n| SEEN.get(*n as usize)) {{
            if !flag.load(std::sync::atomic::Ordering::Relaxed)
                && !flag.swap(true, std::sync::atomic::Ordering::Relaxed)
            {{
                record_seen(ids);
            }}
        }}
    }}
    #[cold]
    fn record_seen(ids: &[u32]) {{
        if let std::option::Option::Some(mut file) = BASELINE.get() {{
            let mut line = std::string::String::new();
            for n in ids {{
                line.push_str(&std::string::ToString::to_string(n));
                line.push(' ');
            }}
            line.push('\n');
            let _ = std::io::Write::write_all(&mut file, line.as_bytes());
        }}
    }}
    fn open_baseline() {{
        let mut name = std::string::String::from("baseline-");
        name.push_str(&std::string::ToString::to_string(&std::process::id()));
        if let std::result::Result::Ok(file) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(std::path::Path::new(MARKER_DIR).join(name))
        {{
            let _ = BASELINE.set(file);
        }}
    }}
    fn mark(name: &str, contents: &[u8]) {{
        let _ = std::fs::write(std::path::Path::new(MARKER_DIR).join(name), contents);
    }}
}}
"#
    )
}

/// The expression at a site that returns the active mutant id, recording that the
/// site of mutants `ids` ran.
fn site_expr(ids: impl Iterator<Item = MutantId>) -> String {
    format!("{SITE_FN}(&[{}])", ids.map(|id| id.to_string()).join(", "))
}

/// How a site is rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum SiteKind {
    /// An expression, rewritten as
    /// `(match ID { n if REACHED => (alt), .., _ => (original) })`.
    Expr,
    /// The statements of a function body, rewritten as
    /// `match ID { n if REACHED => { alt } .. _ => { original } }`.
    FnBody,
    /// Deletion of a match arm without a guard: a zero-width site just after the
    /// arm's pattern, where ` if KEEP(n)` is inserted.
    ArmGuardInsert,
    /// Deletion of a match arm that has a guard: the site is the guard expression,
    /// rewritten as `(KEEP(n)) && (original)`.
    ArmGuardWrap,
}

/// One mutant at a site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alternative {
    pub id: MutantId,
    /// The whole site text with this mutant applied, on one line. Unused for arm deletions.
    pub text: String,
}

/// A mutation site: a byte range of the original text and the mutants applied there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Site {
    pub start: usize,
    pub end: usize,
    pub kind: SiteKind,
    pub alternatives: Vec<Alternative>,
}

/// A byte range of the generated text that belongs to one mutant's arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArmRange {
    pub start: usize,
    pub end: usize,
    pub id: MutantId,
}

/// A byte range of the generated text produced by one site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SiteRange {
    pub start: usize,
    pub end: usize,
    /// Ids of the mutants at this site itself.
    pub own: Vec<MutantId>,
    /// Ids of the mutants at sites nested inside this one.
    pub nested: Vec<MutantId>,
}

/// The generated text for one file, and a map from generated ranges to mutants.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Schema {
    pub text: String,
    pub arms: Vec<ArmRange>,
    pub sites: Vec<SiteRange>,
    /// Ids of mutants that could not be embedded because their site partially
    /// overlaps another site.
    pub overlapping: Vec<MutantId>,
}

/// What part of a schema a generated byte offset falls in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Attribution {
    /// Inside the arm of exactly one mutant.
    Arm(MutantId),
    /// Inside a site but not inside any arm: the mutants of the innermost such site,
    /// or if it has none of its own (because they were dropped), the mutants nested
    /// inside it.
    Site(Vec<MutantId>),
    /// Outside every site.
    File,
}

impl Schema {
    /// Find which mutants are responsible for a generated byte offset.
    pub(crate) fn attribute(&self, offset: usize) -> Attribution {
        if let Some(arm) = self
            .arms
            .iter()
            .filter(|arm| arm.start <= offset && offset < arm.end)
            .min_by_key(|arm| arm.end - arm.start)
        {
            Attribution::Arm(arm.id)
        } else if let Some(site) = self
            .sites
            .iter()
            .filter(|site| site.start <= offset && offset < site.end)
            .min_by_key(|site| site.end - site.start)
        {
            Attribution::Site(if site.own.is_empty() {
                site.nested.clone()
            } else {
                site.own.clone()
            })
        } else {
            Attribution::File
        }
    }
}

/// Order of sites with identical spans: lower ranks enclose higher ranks.
fn nesting_rank(kind: SiteKind) -> u8 {
    match kind {
        SiteKind::FnBody => 0,
        SiteKind::ArmGuardWrap => 1,
        SiteKind::Expr => 2,
        SiteKind::ArmGuardInsert => 3,
    }
}

/// A site placed in the nesting tree.
struct Node {
    site: Site,
    children: Vec<usize>,
}

/// Render the schema for a file.
///
/// `sites` may be in any order. If `lint_allow_at` is given, [`LINT_ALLOW`] is
/// inserted at that byte offset of the original, which should precede every site.
pub(crate) fn render(code: &str, mut sites: Vec<Site>, lint_allow_at: Option<usize>) -> Schema {
    sites.sort_by_key(|site| {
        (
            site.start,
            std::cmp::Reverse(site.end),
            nesting_rank(site.kind),
        )
    });
    let mut nodes: Vec<Node> = Vec::with_capacity(sites.len());
    let mut roots: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut overlapping = Vec::new();
    'sites: for site in sites {
        while let Some(&top) = stack.last() {
            let enclosing = &nodes[top].site;
            if site.start >= enclosing.start && site.end <= enclosing.end {
                break;
            } else if site.start < enclosing.end {
                // Starts inside the enclosing site but ends after it.
                overlapping.extend(site.alternatives.iter().map(|alt| alt.id));
                continue 'sites;
            }
            stack.pop();
        }
        let idx = nodes.len();
        match stack.last() {
            Some(&parent) => nodes[parent].children.push(idx),
            None => roots.push(idx),
        }
        nodes.push(Node {
            site,
            children: Vec::new(),
        });
        stack.push(idx);
    }
    let mut renderer = Renderer {
        code,
        nodes: &nodes,
        schema: Schema {
            text: String::with_capacity(code.len() * 2),
            overlapping,
            ..Schema::default()
        },
    };
    let body_start = match lint_allow_at {
        Some(at) => {
            let at = roots.first().map_or(at, |&r| at.min(nodes[r].site.start));
            renderer.schema.text.push_str(&code[..at]);
            renderer.schema.text.push_str(LINT_ALLOW);
            at
        }
        None => 0,
    };
    renderer.render_range(body_start, code.len(), &roots);
    renderer.schema
}

struct Renderer<'a> {
    code: &'a str,
    nodes: &'a [Node],
    schema: Schema,
}

impl Renderer<'_> {
    /// Emit the original text in `start..end`, with the given child sites rewritten.
    fn render_range(&mut self, start: usize, end: usize, children: &[usize]) {
        let mut pos = start;
        for &child in children {
            let site = &self.nodes[child].site;
            self.schema.text.push_str(&self.code[pos..site.start]);
            self.render_site(child);
            pos = site.end;
        }
        self.schema.text.push_str(&self.code[pos..end]);
    }

    /// Emit an arm's text, recording its range.
    fn push_arm(&mut self, id: MutantId, text: &str) {
        let start = self.schema.text.len();
        self.schema.text.push_str(text);
        self.schema.arms.push(ArmRange {
            start,
            end: self.schema.text.len(),
            id,
        });
    }

    fn render_site(&mut self, idx: usize) {
        let node = &self.nodes[idx];
        let site = &node.site;
        let start = self.schema.text.len();
        if site.alternatives.is_empty() {
            // Nothing to embed here: just the original text, with nested sites.
            self.render_range(site.start, site.end, &node.children);
            self.push_site_range(idx, start);
            return;
        }
        match site.kind {
            SiteKind::Expr => {
                let scrutinee = site_expr(site.alternatives.iter().map(|alt| alt.id));
                write!(self.schema.text, "(match {scrutinee} {{").unwrap();
                for alt in &site.alternatives {
                    let (id, text) = (alt.id, &alt.text);
                    self.push_arm(id, &format!(" {id} if {REACHED_EXPR} => ({text}),"));
                }
                self.schema.text.push_str(" _ => (");
                self.render_range(site.start, site.end, &node.children);
                self.schema.text.push_str(") })");
            }
            SiteKind::FnBody => {
                let scrutinee = site_expr(site.alternatives.iter().map(|alt| alt.id));
                write!(self.schema.text, "match {scrutinee} {{").unwrap();
                for alt in &site.alternatives {
                    let (id, text) = (alt.id, &alt.text);
                    self.push_arm(id, &format!(" {id} if {REACHED_EXPR} => {{ {text} }}"));
                }
                self.schema.text.push_str(" _ => {");
                self.render_range(site.start, site.end, &node.children);
                self.schema.text.push_str("} }");
            }
            SiteKind::ArmGuardInsert => {
                for alt in &site.alternatives {
                    self.push_arm(alt.id, &format!(" if {KEEP_FN}({})", alt.id));
                }
            }
            SiteKind::ArmGuardWrap => {
                for alt in &site.alternatives {
                    self.push_arm(alt.id, &format!("({KEEP_FN}({})) && ", alt.id));
                }
                self.schema.text.push('(');
                self.render_range(site.start, site.end, &node.children);
                self.schema.text.push(')');
            }
        }
        self.push_site_range(idx, start);
    }

    /// Record the generated range of a site that started at `start`, if it has any mutants.
    fn push_site_range(&mut self, idx: usize, start: usize) {
        let node = &self.nodes[idx];
        let own = node
            .site
            .alternatives
            .iter()
            .map(|alt| alt.id)
            .collect_vec();
        let mut nested = Vec::new();
        for &child in &node.children {
            self.collect_ids(child, &mut nested);
        }
        if !own.is_empty() || !nested.is_empty() {
            self.schema.sites.push(SiteRange {
                start,
                end: self.schema.text.len(),
                own,
                nested,
            });
        }
    }

    /// Collect the ids of all mutants at a site and its nested sites.
    fn collect_ids(&self, idx: usize, ids: &mut Vec<MutantId>) {
        let node = &self.nodes[idx];
        ids.extend(node.site.alternatives.iter().map(|alt| alt.id));
        for &child in &node.children {
            self.collect_ids(child, ids);
        }
    }
}

#[cfg(test)]
pub(super) mod test {
    use std::ops::Range;

    use pretty_assertions::assert_eq;
    use quote::ToTokens;
    use syn::spanned::Spanned;
    use syn::visit::Visit;

    use super::*;
    use crate::span::{LineColumn, Span};

    /// Byte range of the `nth` occurrence of `needle` in `code`.
    fn find(code: &str, needle: &str, nth: usize) -> (usize, usize) {
        let start = code
            .match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("{needle:?} #{nth} not found"))
            .0;
        (start, start + needle.len())
    }

    fn site(code: &str, needle: &str, kind: SiteKind, alternatives: &[(MutantId, &str)]) -> Site {
        let (start, end) = find(code, needle, 0);
        Site {
            start,
            end,
            kind,
            alternatives: alternatives
                .iter()
                .map(|&(id, text)| Alternative {
                    id,
                    text: text.to_owned(),
                })
                .collect(),
        }
    }

    fn arm_insert_after(code: &str, pattern: &str, id: MutantId) -> Site {
        let (_, end) = find(code, pattern, 0);
        Site {
            start: end,
            end,
            kind: SiteKind::ArmGuardInsert,
            alternatives: vec![Alternative {
                id,
                text: String::new(),
            }],
        }
    }

    /// Finds schema constructs in generated text, using `syn` independently of the renderer.
    ///
    /// Each construct is recorded as an edit that replaces it with its default (id 0) branch.
    struct ConstructFinder<'t> {
        text: &'t str,
        edits: Vec<(Range<usize>, String)>,
    }

    /// True for a call of the function named `name`.
    fn is_call_of(expr: &syn::Expr, name: &str) -> bool {
        let function: syn::Expr = syn::parse_str(name).unwrap();
        matches!(expr, syn::Expr::Call(call)
            if call.func.to_token_stream().to_string() == function.to_token_stream().to_string())
    }

    /// True for a call of [`SITE_FN`], the scrutinee of a schema `match`.
    fn is_id_expr(expr: &syn::Expr) -> bool {
        is_call_of(expr, SITE_FN)
    }

    /// True for a call of [`KEEP_FN`].
    fn is_keep(expr: &syn::Expr) -> bool {
        is_call_of(expr, KEEP_FN)
    }

    /// Convert a `proc_macro2` span into a byte range of `text`.
    fn byte_range(text: &str, span: proc_macro2::Span) -> Range<usize> {
        let span = Span::from(span);
        let offset = |lc: LineColumn| {
            let line_start: usize = text
                .split_inclusive('\n')
                .take(lc.line - 1)
                .map(str::len)
                .sum();
            line_start
                + text[line_start..]
                    .chars()
                    .take(lc.column - 1)
                    .map(char::len_utf8)
                    .sum::<usize>()
        };
        offset(span.start)..offset(span.end)
    }

    impl ConstructFinder<'_> {
        fn text_of(&self, span: proc_macro2::Span) -> String {
            self.text[byte_range(self.text, span)].to_owned()
        }
    }

    /// Assert that every arm of a schema `match` but the last is guarded by
    /// [`REACHED_EXPR`], and that its [`SITE_FN`] scrutinee records the ids of exactly
    /// those arms.
    fn assert_arms_record_reaching(m: &syn::ExprMatch) {
        let reached: syn::Expr = syn::parse_str(REACHED_EXPR).unwrap();
        let mutant_arms = &m.arms[..m.arms.len() - 1];
        for arm in mutant_arms {
            let (_if, guard) = arm.guard.as_ref().expect("mutant arm has a guard");
            assert_eq!(
                guard.to_token_stream().to_string(),
                reached.to_token_stream().to_string()
            );
        }
        let syn::Expr::Call(site) = &*m.expr else {
            panic!("scrutinee is a call")
        };
        let ids = mutant_arms
            .iter()
            .map(|arm| arm.pat.to_token_stream().to_string())
            .join(" , ");
        assert_eq!(
            site.args.to_token_stream().to_string(),
            format!("& [{ids}]")
        );
    }

    impl<'ast> Visit<'ast> for ConstructFinder<'_> {
        fn visit_expr(&mut self, expr: &'ast syn::Expr) {
            if let syn::Expr::Match(m) = expr
                && is_id_expr(&m.expr)
            {
                assert_arms_record_reaching(m);
            }
            match expr {
                syn::Expr::Paren(paren) => {
                    if let syn::Expr::Match(m) = &*paren.expr
                        && is_id_expr(&m.expr)
                    {
                        let last = m.arms.last().unwrap();
                        assert!(matches!(last.pat, syn::Pat::Wild(_)));
                        let syn::Expr::Paren(body) = &*last.body else {
                            panic!("default arm body should be parenthesized")
                        };
                        self.edits.push((
                            byte_range(self.text, expr.span()),
                            self.text_of(body.expr.span()),
                        ));
                    }
                }
                syn::Expr::Match(m) if is_id_expr(&m.expr) => {
                    // A match whose default arm is parenthesized is the inside of an
                    // expression site, handled above; a block means a function body site.
                    let last = m.arms.last().unwrap();
                    if let syn::Expr::Block(block) = &*last.body {
                        let stmts = &block.block.stmts;
                        let inner = byte_range(self.text, stmts.first().unwrap().span()).start
                            ..byte_range(self.text, stmts.last().unwrap().span()).end;
                        self.edits.push((
                            byte_range(self.text, expr.span()),
                            self.text[inner].to_owned(),
                        ));
                    }
                }
                _ => (),
            }
            syn::visit::visit_expr(self, expr);
        }

        fn visit_arm(&mut self, arm: &'ast syn::Arm) {
            if let Some((_if, guard)) = &arm.guard {
                if is_keep(guard) {
                    // Inserted guard: delete from the end of the pattern to the end of the guard.
                    let start = byte_range(self.text, arm.pat.span()).end;
                    let end = byte_range(self.text, guard.span()).end;
                    self.edits.push((start..end, String::new()));
                } else if let syn::Expr::Binary(and) = &**guard
                    && matches!(and.op, syn::BinOp::And(_))
                    && let syn::Expr::Paren(left) = &*and.left
                    && is_keep(&left.expr)
                {
                    let syn::Expr::Paren(right) = &*and.right else {
                        panic!("wrapped guard should be parenthesized")
                    };
                    self.edits.push((
                        byte_range(self.text, guard.span()),
                        self.text_of(right.expr.span()),
                    ));
                }
            }
            syn::visit::visit_arm(self, arm);
        }
    }

    /// Replace the innermost schema construct with its default branch, if there is one.
    fn reduce_one(text: &str) -> Option<String> {
        let file = syn::parse_file(text).expect("schema parses");
        let mut finder = ConstructFinder {
            text,
            edits: Vec::new(),
        };
        finder.visit_file(&file);
        let (range, replacement) = finder.edits.iter().find(|(a, _)| {
            !finder
                .edits
                .iter()
                .any(|(b, _)| b != a && b.start >= a.start && b.end <= a.end)
        })?;
        let mut text = text.to_owned();
        text.replace_range(range.clone(), replacement);
        Some(text)
    }

    /// Assert that the schema, with id 0, is token-for-token the original code.
    pub(crate) fn assert_default_is_original(code: &str, schema: &Schema) {
        let mut text = schema.text.replace(LINT_ALLOW, "");
        while let Some(reduced) = reduce_one(&text) {
            text = reduced;
        }
        let tokens = |s: &str| {
            syn::parse_file(s)
                .expect("parse")
                .to_token_stream()
                .to_string()
        };
        assert_eq!(tokens(&text), tokens(code));
    }

    fn arm_texts(schema: &Schema) -> Vec<(MutantId, &str)> {
        schema
            .arms
            .iter()
            .map(|arm| (arm.id, &schema.text[arm.start..arm.end]))
            .collect()
    }

    #[test]
    fn render_without_sites_returns_original_text() {
        let code = "fn f() -> u32 {\n    1 + 2\n}\n";
        let schema = render(code, Vec::new(), None);
        assert_eq!(schema.text, code);
        assert!(schema.arms.is_empty());
    }

    #[test]
    fn render_binary_operator_site_as_parenthesized_match() {
        let code = "fn f(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let schema = render(
            code,
            vec![site(
                code,
                "a + b",
                SiteKind::Expr,
                &[(1, "a - b"), (2, "a * b")],
            )],
            None,
        );
        assert_eq!(
            schema.text,
            "fn f(a: u32, b: u32) -> u32 {\n    (match crate::__cargo_mutants_schemata::site(&[1, 2]) { 1 if crate::__cargo_mutants_schemata::reached() => (a - b), 2 if crate::__cargo_mutants_schemata::reached() => (a * b), _ => (a + b) })\n}\n"
        );
        assert_eq!(
            arm_texts(&schema),
            [
                (
                    1,
                    " 1 if crate::__cargo_mutants_schemata::reached() => (a - b),"
                ),
                (
                    2,
                    " 2 if crate::__cargo_mutants_schemata::reached() => (a * b),"
                )
            ]
        );
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_compound_assignment_site() {
        let code = "fn f(mut a: u32) -> u32 {\n    a += 2;\n    a\n}\n";
        let schema = render(
            code,
            vec![site(code, "a += 2", SiteKind::Expr, &[(4, "a -= 2")])],
            None,
        );
        assert_eq!(
            schema.text,
            "fn f(mut a: u32) -> u32 {\n    (match crate::__cargo_mutants_schemata::site(&[4]) { 4 if crate::__cargo_mutants_schemata::reached() => (a -= 2), _ => (a += 2) });\n    a\n}\n"
        );
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_unary_operator_site() {
        let code = "fn f(a: bool) -> bool {\n    !a\n}\n";
        let schema = render(
            code,
            vec![site(code, "!a", SiteKind::Expr, &[(1, "a")])],
            None,
        );
        assert!(
            schema
                .text
                .contains("(match crate::__cargo_mutants_schemata::site(&[1]) { 1 if crate::__cargo_mutants_schemata::reached() => (a), _ => (!a) })")
        );
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_fn_body_site_as_match_with_blocks() {
        let code = "fn f(a: u32) -> u32 {\n    let b = a;\n    b\n}\n";
        let schema = render(
            code,
            vec![site(
                code,
                "let b = a;\n    b",
                SiteKind::FnBody,
                &[(1, "0"), (2, "1")],
            )],
            None,
        );
        assert_eq!(
            schema.text,
            "fn f(a: u32) -> u32 {\n    match crate::__cargo_mutants_schemata::site(&[1, 2]) { 1 if crate::__cargo_mutants_schemata::reached() => { 0 } 2 if crate::__cargo_mutants_schemata::reached() => { 1 } _ => {let b = a;\n    b} }\n}\n"
        );
        assert_eq!(
            arm_texts(&schema),
            [
                (
                    1,
                    " 1 if crate::__cargo_mutants_schemata::reached() => { 0 }"
                ),
                (
                    2,
                    " 2 if crate::__cargo_mutants_schemata::reached() => { 1 }"
                )
            ]
        );
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_async_fn_body_site() {
        let code = "async fn f(a: u32) -> u32 {\n    g(a).await\n}\n";
        let schema = render(
            code,
            vec![site(code, "g(a).await", SiteKind::FnBody, &[(1, "0")])],
            None,
        );
        assert_eq!(
            schema.text,
            "async fn f(a: u32) -> u32 {\n    match crate::__cargo_mutants_schemata::site(&[1]) { 1 if crate::__cargo_mutants_schemata::reached() => { 0 } _ => {g(a).await} }\n}\n"
        );
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_binary_site_nested_in_fn_body_keeps_original_text_in_fn_value_arm() {
        let code = "fn f(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let schema = render(
            code,
            vec![
                site(code, "a + b", SiteKind::FnBody, &[(1, "0")]),
                site(code, "a + b", SiteKind::Expr, &[(2, "a - b")]),
            ],
            None,
        );
        assert_eq!(
            schema.text,
            "fn f(a: u32, b: u32) -> u32 {\n    match crate::__cargo_mutants_schemata::site(&[1]) { 1 if crate::__cargo_mutants_schemata::reached() => { 0 } _ => {(match crate::__cargo_mutants_schemata::site(&[2]) { 2 if crate::__cargo_mutants_schemata::reached() => (a - b), _ => (a + b) })} }\n}\n"
        );
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_binary_site_with_binary_operand_uses_original_operand_in_outer_arms() {
        let code = "fn f(a: u32, b: u32, c: u32) -> u32 {\n    a + b * c\n}\n";
        let schema = render(
            code,
            vec![
                site(code, "a + b * c", SiteKind::Expr, &[(1, "a - b * c")]),
                site(code, "b * c", SiteKind::Expr, &[(2, "b / c")]),
            ],
            None,
        );
        assert_eq!(
            schema.text,
            "fn f(a: u32, b: u32, c: u32) -> u32 {\n    (match crate::__cargo_mutants_schemata::site(&[1]) { 1 if crate::__cargo_mutants_schemata::reached() => (a - b * c), _ => (a + (match crate::__cargo_mutants_schemata::site(&[2]) { 2 if crate::__cargo_mutants_schemata::reached() => (b / c), _ => (b * c) })) })\n}\n"
        );
        assert_eq!(schema.sites.len(), 2);
        let outer = schema.sites.iter().find(|s| s.own == [1]).unwrap();
        assert_eq!(outer.nested, [2]);
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_match_arm_without_guard_inserts_guard() {
        let code =
            "fn f(x: u32) -> u32 {\n    match x {\n        1 => 2,\n        _ => 3,\n    }\n}\n";
        let schema = render(code, vec![arm_insert_after(code, "        1", 5)], None);
        assert_eq!(
            schema.text,
            "fn f(x: u32) -> u32 {\n    match x {\n        1 if crate::__cargo_mutants_schemata::keep(5) => 2,\n        _ => 3,\n    }\n}\n"
        );
        assert_eq!(
            arm_texts(&schema),
            [(5, " if crate::__cargo_mutants_schemata::keep(5)")]
        );
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_match_arm_with_guard_wraps_guard_and_guard_replacements() {
        let code = "fn f(x: u32) -> u32 {\n    match x {\n        1 if x > 0 => 2,\n        _ => 3,\n    }\n}\n";
        let guard = site(code, "x > 0", SiteKind::ArmGuardWrap, &[(7, "")]);
        let replace = site(code, "x > 0", SiteKind::Expr, &[(8, "true"), (9, "false")]);
        let schema = render(code, vec![replace, guard], None);
        assert_eq!(
            schema.text,
            "fn f(x: u32) -> u32 {\n    match x {\n        1 if (crate::__cargo_mutants_schemata::keep(7)) && ((match crate::__cargo_mutants_schemata::site(&[8, 9]) { 8 if crate::__cargo_mutants_schemata::reached() => (true), 9 if crate::__cargo_mutants_schemata::reached() => (false), _ => (x > 0) })) => 2,\n        _ => 3,\n    }\n}\n"
        );
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_preserves_line_count() {
        let code = "fn f(a: u32,\n     b: u32) -> u32 {\n    let c = a\n        + b;\n    c\n}\n";
        let schema = render(
            code,
            vec![
                site(
                    code,
                    "let c = a\n        + b;\n    c",
                    SiteKind::FnBody,
                    &[(1, "0")],
                ),
                site(code, "a\n        + b", SiteKind::Expr, &[(2, "a - b")]),
            ],
            None,
        );
        assert_eq!(schema.text.lines().count(), code.lines().count());
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_inserts_lint_allow_at_given_offset() {
        let code = "#![deny(warnings)]\nfn f(a: u32) -> u32 {\n    a + 1\n}\n";
        let offset = "#![deny(warnings)]".len();
        let schema = render(
            code,
            vec![site(code, "a + 1", SiteKind::Expr, &[(1, "a - 1")])],
            Some(offset),
        );
        assert!(
            schema
                .text
                .starts_with(&format!("#![deny(warnings)]{LINT_ALLOW}\nfn f"))
        );
        assert_eq!(
            arm_texts(&schema),
            [(
                1,
                " 1 if crate::__cargo_mutants_schemata::reached() => (a - 1),"
            )]
        );
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn render_rejects_partially_overlapping_site() {
        let code = "fn f(a: u32, b: u32, c: u32) -> u32 {\n    a + b + c\n}\n";
        let (start, _) = find(code, "b + c", 0);
        let schema = render(
            code,
            vec![
                site(code, "a + b", SiteKind::Expr, &[(1, "a - b")]),
                Site {
                    start,
                    end: start + "b + c".len(),
                    kind: SiteKind::Expr,
                    alternatives: vec![Alternative {
                        id: 2,
                        text: "b - c".to_owned(),
                    }],
                },
            ],
            None,
        );
        assert_eq!(schema.overlapping, [2]);
        assert_eq!(
            arm_texts(&schema),
            [(
                1,
                " 1 if crate::__cargo_mutants_schemata::reached() => (a - b),"
            )]
        );
    }

    #[test]
    fn attribute_finds_innermost_arm_then_site_then_file() {
        let code = "fn f(a: u32, b: u32, c: u32) -> u32 {\n    a + b * c\n}\n";
        let schema = render(
            code,
            vec![
                site(code, "a + b * c", SiteKind::Expr, &[(1, "a - b * c")]),
                site(code, "b * c", SiteKind::Expr, &[(2, "b / c")]),
            ],
            None,
        );
        let at = |needle: &str, nth: usize| find(&schema.text, needle, nth).0;
        assert_eq!(schema.attribute(at("a - b * c", 0)), Attribution::Arm(1));
        assert_eq!(schema.attribute(at("b / c", 0)), Attribution::Arm(2));
        // The inner site's header is inside the outer site's default arm.
        assert_eq!(
            schema.attribute(at("(match", 1)),
            Attribution::Site(vec![2])
        );
        // Outside nested sites, a site's own mutants are blamed first.
        assert_eq!(
            schema.attribute(at("_ => (a + ", 0)),
            Attribution::Site(vec![1])
        );
        assert_eq!(schema.attribute(0), Attribution::File);
    }

    #[test]
    fn attribute_blames_nested_mutants_of_site_without_alternatives() {
        // After the outer site's own mutants are dropped, errors in its text blame the
        // mutants nested inside it, rather than the whole file.
        let code = "fn f(a: u32, b: u32, c: u32) -> u32 {\n    let d = c;\n    a + b * d\n}\n";
        let schema = render(
            code,
            vec![
                site(code, "let d = c;\n    a + b * d", SiteKind::FnBody, &[]),
                site(code, "b * d", SiteKind::Expr, &[(2, "b / d")]),
            ],
            None,
        );
        let at = |needle: &str| find(&schema.text, needle, 0).0;
        assert_eq!(schema.attribute(at("let d")), Attribution::Site(vec![2]));
        assert_eq!(schema.attribute(0), Attribution::File);
    }

    #[test]
    fn reduce_to_default_undoes_nested_schema() {
        // Sanity check of the test helper itself: it must undo nesting too.
        let code = "fn f(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let text = "fn f(a: u32, b: u32) -> u32 {\n    match crate::__cargo_mutants_schemata::site(&[1]) { 1 if crate::__cargo_mutants_schemata::reached() => { 0 } _ => {(match crate::__cargo_mutants_schemata::site(&[2]) { 2 if crate::__cargo_mutants_schemata::reached() => (a - b), _ => (a + b) })} }\n}\n";
        let schema = Schema {
            text: text.to_owned(),
            ..Schema::default()
        };
        assert_default_is_original(code, &schema);
    }
}
