// Copyright 2026 Martin Pool

//! Decide where each mutant of one source file goes in its schema, or why it can't
//! be embedded and must be tested the classic way.

#![warn(clippy::pedantic)]

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use anyhow::Context;
use quote::ToTokens;
use serde::Serialize;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{AttrStyle, BinOp, Expr};

use super::generate::{Alternative, MutantId, Site, SiteKind};
use crate::Result;
use crate::mutant::{EnclosingSyntax, Genre, Mutant};
use crate::span::{LineColumn, Span};

/// Why a mutant is not embedded in the schema, so is tested the classic way instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FallbackReason {
    /// The generator doesn't support this genre (e.g. `StructField`).
    UnsupportedGenre,
    /// Inside a const context (const/static item, `const fn`, array length, ...),
    /// where the runtime id can't be read.
    ConstContext,
    /// A `&&` whose operands include `let`: a let chain can't be parenthesized.
    LetChain,
    /// The replacement operator's precedence is unknown, so it's not known which
    /// site would group the operands as the classic textual substitution does.
    OperatorPrecedence,
    /// A replacement for the value of a function returning `impl Trait`: each
    /// replacement has its own concrete type, but the function can return only one.
    ImplTraitReturn,
    /// The package is a proc-macro, whose code runs at compile time.
    ProcMacroCrate,
    /// The package is a dependency of a build script or a proc-macro, perhaps
    /// indirectly, so its code can run at compile time, where the id can't be seen.
    CompileTimeDependency,
    /// The alternative text could not be re-tokenized onto one line.
    UntokenizableAlternative,
    /// The site partially overlaps another site.
    OverlappingSite,
    /// A compile error was attributed to this mutant's arm.
    CompileError,
    /// A compile error in an enclosing site could not be attributed to one arm.
    EnclosingSiteCompileError,
    /// A compile error in this file could not be attributed to any site.
    FileCompileError,
    /// A compile error could not be attributed to any schema file.
    UnattributedCompileError,
    /// The check loop hit its iteration limit.
    CheckIterationsExhausted,
    /// Tests failed with the schema and no mutant active, but passed without it.
    SchemaChangesBehavior,
    /// Tests failed with the schema and no mutant active, and the mutant is in a file
    /// named by a string literal in a package, so tests might read its text: the file
    /// was left out of the schema, so that tests read its original text.
    SourceReadByTests,
    /// Like [`FallbackReason::SourceReadByTests`], but tests might read a crate root
    /// of the mutant's package, which carries the schema's helper module that every
    /// embedded mutant of the crate needs, so the whole package was left out.
    CrateRootReadByTests,
    /// The mutant was missed, but it's in a file named by a string literal in a
    /// package, so tests might read its text: with the schema they read the same
    /// text for every mutant, but the classic way they read the mutated text, which
    /// might make them fail.
    SourceReadByTestsMissedRetest,
    /// The mutant was missed, but some schema code ran without the mutant id,
    /// because a test cleared the environment or it ran at build time, so that
    /// can't be trusted.
    EnvironmentCleared,
    /// No test process recorded that it ran schema code seeing the mutant id, so
    /// it's not known that tests see the id, as they might not when run in a sandbox.
    MarkersNotRecorded,
}

/// True if the function containing `mutant` returns an `impl Trait` type, anywhere
/// in its return type.
fn returns_impl_trait(mutant: &Mutant) -> bool {
    mutant.function.as_ref().is_some_and(|function| {
        function
            .return_type
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|word| word == "impl")
    })
}

/// Identifies one site: mutants with the same key share a `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SiteKey {
    start: usize,
    end: usize,
    kind: SiteKind,
}

/// Where one mutant goes in the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Placement {
    key: SiteKey,
    /// Single-line text of the whole site with this mutant applied.
    alternative: String,
}

/// The placement of every mutant of one file, and where to insert the lint allowance.
#[derive(Debug)]
pub(crate) struct FilePlan {
    /// One entry per input mutant, in order.
    pub placements: Vec<std::result::Result<Placement, FallbackReason>>,
    /// Byte offset after the file's inner attributes.
    pub lint_allow_at: usize,
}

/// Place each mutant of one file into a schema site, or say why it can't be.
///
/// All `mutants` must come from `code`.
pub(crate) fn plan_file(code: &str, mutants: &[&Mutant]) -> Result<FilePlan> {
    let file = syn::parse_file(code).context("parse source file for schemata")?;
    let lines = LineIndex::new(code);
    let mut facts = SyntaxFacts {
        code,
        lines: &lines,
        const_ranges: Vec::new(),
        binaries: HashMap::new(),
        parents: HashMap::new(),
    };
    facts.visit_file(&file);
    let placements = mutants
        .iter()
        .map(|mutant| place(code, &lines, &facts, mutant))
        .collect();
    Ok(FilePlan {
        placements,
        lint_allow_at: lint_allow_offset(code, &file, &lines),
    })
}

/// Decide where one mutant goes, or why it can't be embedded.
fn place(
    code: &str,
    lines: &LineIndex,
    facts: &SyntaxFacts,
    mutant: &Mutant,
) -> std::result::Result<Placement, FallbackReason> {
    let span = lines.range(code, mutant.span);
    // The site, and the text of the site with this mutant applied, before re-tokenizing.
    let (key, alternative) = match (&mutant.genre, mutant.enclosing) {
        (Genre::FnValue, _) if returns_impl_trait(mutant) => {
            return Err(FallbackReason::ImplTraitReturn);
        }
        (Genre::FnValue, _) => (
            SiteKey::new(span.clone(), SiteKind::FnBody),
            Some(mutant.replacement.clone()),
        ),
        (Genre::MatchArmGuard, _) => (
            SiteKey::new(span.clone(), SiteKind::Expr),
            Some(mutant.replacement.clone()),
        ),
        (Genre::BinaryOperator, Some(EnclosingSyntax::BinaryExpr(expr_span)))
        | (Genre::UnaryOperator, Some(EnclosingSyntax::UnaryExpr(expr_span))) => {
            let mut expr = lines.range(code, expr_span);
            if mutant.genre == Genre::BinaryOperator {
                let info = facts
                    .binaries
                    .get(&(expr.start, expr.end))
                    .ok_or(FallbackReason::UnsupportedGenre)?;
                if info.let_chain {
                    return Err(FallbackReason::LetChain);
                }
                let (start, end) =
                    facts.classic_site((expr.start, expr.end), &mutant.replacement)?;
                expr = start..end;
            }
            let alternative = format!(
                "{}{}{}",
                &code[expr.start..span.start],
                mutant.replacement,
                &code[span.end..expr.end]
            );
            (SiteKey::new(expr, SiteKind::Expr), Some(alternative))
        }
        (Genre::MatchArm, Some(EnclosingSyntax::MatchArm { pat, guard })) => match guard {
            None => {
                let pat_end = lines.range(code, pat).end;
                (
                    SiteKey::new(pat_end..pat_end, SiteKind::ArmGuardInsert),
                    None,
                )
            }
            Some(guard) => (
                SiteKey::new(lines.range(code, guard), SiteKind::ArmGuardWrap),
                None,
            ),
        },
        _ => return Err(FallbackReason::UnsupportedGenre),
    };
    if facts.in_const_context(key.start) {
        return Err(FallbackReason::ConstContext);
    }
    let alternative = match alternative {
        Some(text) => single_line(&text).ok_or(FallbackReason::UntokenizableAlternative)?,
        None => String::new(),
    };
    Ok(Placement { key, alternative })
}

impl SiteKey {
    fn new(range: Range<usize>, kind: SiteKind) -> SiteKey {
        SiteKey {
            start: range.start,
            end: range.end,
            kind,
        }
    }
}

/// Re-tokenize Rust code onto a single line, dropping comments.
///
/// This keeps alternatives from adding lines, so the schema preserves line numbers.
fn single_line(text: &str) -> Option<String> {
    text.parse::<proc_macro2::TokenStream>()
        .ok()
        .map(|tokens| tokens.to_string())
}

/// Byte offset at which to insert the lint allowance: after the file's inner
/// attributes (so it takes precedence over e.g. `#![deny(warnings)]`) without
/// adding a line.
fn lint_allow_offset(code: &str, file: &syn::File, lines: &LineIndex) -> usize {
    let next_line = |from: usize| code[from..].find('\n').map_or(code.len(), |i| from + i + 1);
    if let Some(last) = file
        .attrs
        .iter()
        .rfind(|attr| matches!(attr.style, AttrStyle::Inner(_)))
    {
        let range = lines.range(code, last.span().into());
        if code[range.start..].starts_with("//") {
            // A line doc comment would swallow the rest of its line.
            next_line(range.end)
        } else {
            range.end
        }
    } else {
        let bom = if code.starts_with('\u{feff}') { 3 } else { 0 };
        if code[bom..].starts_with("#!") && !code[bom..].starts_with("#![") {
            next_line(bom) // after a shebang line
        } else {
            bom
        }
    }
}

/// Converts line/column positions to byte offsets.
struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(code: &str) -> LineIndex {
        let mut line_starts = vec![0];
        line_starts.extend(code.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { line_starts }
    }

    /// Byte offset of a 1-based line and 1-based character column.
    fn offset(&self, code: &str, lc: LineColumn) -> usize {
        let line_start = self.line_starts[lc.line - 1];
        for (column, (i, c)) in (1..).zip(code[line_start..].char_indices()) {
            if column == lc.column || c == '\n' {
                return line_start + i;
            }
        }
        code.len()
    }

    fn range(&self, code: &str, span: Span) -> Range<usize> {
        self.offset(code, span.start)..self.offset(code, span.end)
    }
}

/// Binding strength of a binary operator given as text, like `+` or `+=`.
///
/// Higher binds tighter; see <https://doc.rust-lang.org/reference/expressions.html#expression-precedence>.
fn precedence(op: &str) -> Option<u8> {
    match op.replace(' ', "").as_str() {
        "*" | "/" | "%" => Some(11),
        "+" | "-" => Some(10),
        "<<" | ">>" => Some(9),
        "&" => Some(8),
        "^" => Some(7),
        "|" => Some(6),
        "==" | "!=" | "<" | ">" | "<=" | ">=" => Some(COMPARISON),
        "&&" => Some(4),
        "||" => Some(3),
        "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>=" => Some(1),
        _ => None,
    }
}

/// Precedence of the (non-associative) comparison operators.
const COMPARISON: u8 = 5;

fn binop_precedence(op: &BinOp) -> Option<u8> {
    precedence(&op.to_token_stream().to_string())
}

/// Which operand of its parent a binary expression is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// The byte range of an expression, as a map key.
type ExprKey = (usize, usize);

/// Syntactic context of one binary expression.
#[derive(Debug, Clone, Default)]
struct BinaryInfo {
    precedence: u8,
    /// The parent binary expression, and which operand this is, if this is an
    /// unparenthesized operand of one.
    parent: Option<(ExprKey, Side)>,
    /// The left and right operands that are unparenthesized binary expressions.
    operands: [Option<ExprKey>; 2],
    /// True for a `&&` that chains a `let`.
    let_chain: bool,
}

/// Facts about the syntax of a file that decide whether mutants can be embedded.
struct SyntaxFacts<'a> {
    code: &'a str,
    lines: &'a LineIndex,
    /// Byte ranges evaluated at compile time.
    const_ranges: Vec<Range<usize>>,
    /// Binary expressions, by byte range.
    binaries: HashMap<ExprKey, BinaryInfo>,
    /// The parent of each binary expression that is an operand of another.
    parents: HashMap<ExprKey, (ExprKey, Side)>,
}

impl SyntaxFacts<'_> {
    fn range_of(&self, node: &impl Spanned) -> Range<usize> {
        self.lines.range(self.code, node.span().into())
    }

    fn key_of(&self, node: &impl Spanned) -> (usize, usize) {
        let range = self.range_of(node);
        (range.start, range.end)
    }

    fn add_const(&mut self, node: &impl Spanned) {
        let range = self.range_of(node);
        self.const_ranges.push(range);
    }

    fn in_const_context(&self, offset: usize) -> bool {
        self.const_ranges
            .iter()
            .any(|range| range.start <= offset && offset < range.end)
    }

    /// The site for replacing the operator of the binary expression `key` by
    /// `replacement`: the smallest binary expression containing it whose text, with
    /// the operator replaced, Rust parses as one expression in the same place.
    ///
    /// Classic mutants substitute the operator's text, so a new operator with a
    /// different precedence can regroup the operands: `a - b * c` with `*` replaced
    /// by `+` means `(a - b) + c`. The schema's alternative for a site is its text
    /// with the operator replaced, parsed in parentheses, so it's the same mutant if
    /// the site is a whole expression of the mutated text.
    fn classic_site(
        &self,
        key: ExprKey,
        replacement: &str,
    ) -> std::result::Result<ExprKey, FallbackReason> {
        let new = precedence(replacement).ok_or(FallbackReason::OperatorPrecedence)?;
        if new == self.binaries[&key].precedence {
            return Ok(key);
        }
        let mut site = key;
        loop {
            // In a chain of binary operators, the operator of lowest precedence in
            // the site (the rightmost, among equals) heads it if the neighbors bind
            // more loosely; all but the non-chaining comparisons are left-associative.
            let head = self.lowest_precedence(site, key, new);
            let (left, right) = self.neighbors(site);
            if left.is_none_or(|left| left < head) && right.is_none_or(|right| right <= head) {
                return Ok(site);
            }
            let (parent, _) = self.binaries[&site]
                .parent
                .expect("an expression with a neighboring operator has a parent");
            if self.binaries[&parent].let_chain {
                return Err(FallbackReason::LetChain);
            }
            site = parent;
        }
    }

    /// The lowest precedence of the operators in the binary chain under `site`,
    /// taking the operator of `mutated` to have precedence `new`.
    fn lowest_precedence(&self, site: ExprKey, mutated: ExprKey, new: u8) -> u8 {
        let info = &self.binaries[&site];
        let own = if site == mutated {
            new
        } else {
            info.precedence
        };
        info.operands
            .iter()
            .flatten()
            .map(|&operand| self.lowest_precedence(operand, mutated, new))
            .fold(own, u8::min)
    }

    /// The precedences of the binary operators just before and after the text of
    /// `site`, if it is inside a chain of them.
    fn neighbors(&self, site: ExprKey) -> (Option<u8>, Option<u8>) {
        let (mut left, mut right) = (None, None);
        let mut node = site;
        while let Some((parent, side)) = self.binaries[&node].parent {
            let precedence = self.binaries[&parent].precedence;
            match side {
                Side::Right => left = left.or(Some(precedence)),
                Side::Left => right = right.or(Some(precedence)),
            }
            if left.is_some() && right.is_some() {
                break;
            }
            node = parent;
        }
        (left, right)
    }
}

fn is_let_chain(expr: &Expr) -> bool {
    match expr {
        Expr::Let(_) => true,
        Expr::Binary(binary) if matches!(binary.op, BinOp::And(_)) => {
            is_let_chain(&binary.left) || is_let_chain(&binary.right)
        }
        _ => false,
    }
}

impl<'ast> Visit<'ast> for SyntaxFacts<'_> {
    fn visit_expr_binary(&mut self, i: &'ast syn::ExprBinary) {
        let key = self.key_of(i);
        let mut info = BinaryInfo {
            precedence: binop_precedence(&i.op).unwrap_or_default(),
            parent: self.parents.get(&key).copied(),
            operands: [None, None],
            let_chain: matches!(i.op, BinOp::And(_))
                && (is_let_chain(&i.left) || is_let_chain(&i.right)),
        };
        for (index, (operand, side)) in [(&*i.left, Side::Left), (&*i.right, Side::Right)]
            .into_iter()
            .enumerate()
        {
            if let Expr::Binary(_) = operand {
                let operand_key = self.key_of(operand);
                self.parents.insert(operand_key, (key, side));
                info.operands[index] = Some(operand_key);
            }
        }
        self.binaries.insert(key, info);
        syn::visit::visit_expr_binary(self, i);
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.add_const(&i.expr);
        syn::visit::visit_item_const(self, i);
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.add_const(&i.expr);
        syn::visit::visit_item_static(self, i);
    }

    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        self.add_const(&i.expr);
        syn::visit::visit_impl_item_const(self, i);
    }

    fn visit_trait_item_const(&mut self, i: &'ast syn::TraitItemConst) {
        if let Some((_eq, expr)) = &i.default {
            self.add_const(expr);
        }
        syn::visit::visit_trait_item_const(self, i);
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if i.sig.constness.is_some() {
            self.add_const(&i.block);
        }
        syn::visit::visit_item_fn(self, i);
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        if i.sig.constness.is_some() {
            self.add_const(&i.block);
        }
        syn::visit::visit_impl_item_fn(self, i);
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        if let Some(block) = &i.default
            && i.sig.constness.is_some()
        {
            self.add_const(block);
        }
        syn::visit::visit_trait_item_fn(self, i);
    }

    fn visit_expr_const(&mut self, i: &'ast syn::ExprConst) {
        self.add_const(i);
        syn::visit::visit_expr_const(self, i);
    }

    fn visit_type_array(&mut self, i: &'ast syn::TypeArray) {
        self.add_const(&i.len);
        syn::visit::visit_type_array(self, i);
    }

    fn visit_expr_repeat(&mut self, i: &'ast syn::ExprRepeat) {
        self.add_const(&i.len);
        syn::visit::visit_expr_repeat(self, i);
    }

    fn visit_variant(&mut self, i: &'ast syn::Variant) {
        if let Some((_eq, discriminant)) = &i.discriminant {
            self.add_const(discriminant);
        }
        syn::visit::visit_variant(self, i);
    }

    fn visit_generic_argument(&mut self, i: &'ast syn::GenericArgument) {
        if let syn::GenericArgument::Const(expr) = i {
            self.add_const(expr);
        }
        syn::visit::visit_generic_argument(self, i);
    }

    fn visit_const_param(&mut self, i: &'ast syn::ConstParam) {
        if let Some(default) = &i.default {
            self.add_const(default);
        }
        syn::visit::visit_const_param(self, i);
    }
}

/// Group placed mutants into sites for rendering.
pub(crate) fn sites<'p>(placed: impl IntoIterator<Item = (MutantId, &'p Placement)>) -> Vec<Site> {
    let mut by_key: BTreeMap<SiteKey, Vec<Alternative>> = BTreeMap::new();
    for (id, placement) in placed {
        by_key.entry(placement.key).or_default().push(Alternative {
            id,
            text: placement.alternative.clone(),
        });
    }
    by_key
        .into_iter()
        .map(|(key, alternatives)| Site {
            start: key.start,
            end: key.end,
            kind: key.kind,
            alternatives,
        })
        .collect()
}

#[cfg(test)]
mod test {
    use indoc::indoc;
    use itertools::Itertools;
    use pretty_assertions::assert_eq;

    use super::super::generate::render;
    use super::super::generate::test::assert_default_is_original;
    use super::*;
    use crate::Options;
    use crate::visit::mutate_source_str;

    /// Plan all mutants of `code`, returning (mutant name, fallback reason or None).
    fn plan_names(code: &str) -> Vec<(String, Option<FallbackReason>)> {
        let mutants = mutate_source_str(code, &Options::default()).unwrap();
        let refs = mutants.iter().collect_vec();
        let plan = plan_file(code, &refs).unwrap();
        mutants
            .iter()
            .zip(plan.placements)
            .map(|(m, p)| (m.name(false), p.err()))
            .collect()
    }

    fn fallbacks(code: &str) -> Vec<(String, FallbackReason)> {
        plan_names(code)
            .into_iter()
            .filter_map(|(name, reason)| reason.map(|r| (name, r)))
            .collect()
    }

    /// Plan and render all embeddable mutants of `code`, numbering them from 1.
    fn render_all(code: &str) -> super::super::generate::Schema {
        let mutants = mutate_source_str(code, &Options::default()).unwrap();
        let refs = mutants.iter().collect_vec();
        let plan = plan_file(code, &refs).unwrap();
        let placed = plan
            .placements
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.as_ref().ok().map(|p| (u32::try_from(i + 1).unwrap(), p)));
        render(code, sites(placed), Some(plan.lint_allow_at))
    }

    #[test]
    fn plan_file_embeds_every_supported_genre() {
        let code = indoc! {"
            fn f(mut a: u32, b: bool) -> u32 {
                a += 1;
                match a {
                    1 => 2,
                    3 if !b => 4,
                    _ => a * 2,
                }
            }
        "};
        let names = plan_names(code);
        assert!(
            names.iter().all(|(_, reason)| reason.is_none()),
            "{names:#?}"
        );
        let genres = mutate_source_str(code, &Options::default())
            .unwrap()
            .iter()
            .map(|m| format!("{:?}", m.genre))
            .unique()
            .sorted()
            .collect_vec();
        assert_eq!(
            genres,
            [
                "BinaryOperator",
                "FnValue",
                "MatchArm",
                "MatchArmGuard",
                "UnaryOperator"
            ]
        );
    }

    #[test]
    fn plan_file_renders_schema_equivalent_to_original_at_id_zero() {
        let code = indoc! {"
            //! Doc.
            fn f(mut a: u32, b: bool, c: u32) -> u32 {
                a += 1;
                let d = a + c * 2 - (a / 3);
                match a {
                    1 => 2,
                    3 if !b && c > 1 => 4,
                    _ => a * d,
                }
            }

            async fn g(x: i64) -> i64 {
                -x + 1
            }
        "};
        let schema = render_all(code);
        assert_eq!(schema.text.lines().count(), code.lines().count());
        assert_default_is_original(code, &schema);
    }

    #[test]
    fn plan_file_falls_back_for_struct_field() {
        let code = indoc! {"
            fn f() -> S {
                S { a: 1, ..Default::default() }
            }
        "};
        assert_eq!(
            fallbacks(code),
            [(
                "src/main.rs: delete field a from struct S expression in f".to_owned(),
                FallbackReason::UnsupportedGenre
            )]
        );
    }

    #[test]
    fn plan_file_falls_back_in_const_contexts() {
        let code = indoc! {"
            const K: u32 = 1 + 2;
            static S: u32 = 3 * 4;
            const fn cf(a: u32) -> u32 {
                a + 1
            }
            fn arr() -> [u8; 2 + 2] {
                [0; 1 + 3]
            }
        "};
        let reasons = fallbacks(code);
        assert_eq!(
            reasons.iter().map(|(name, _)| name.as_str()).collect_vec(),
            [
                "src/main.rs: replace + with -",
                "src/main.rs: replace + with *",
                "src/main.rs: replace * with +",
                "src/main.rs: replace * with /",
                "src/main.rs: replace cf -> u32 with 0",
                "src/main.rs: replace cf -> u32 with 1",
                "src/main.rs: replace + with - in cf",
                "src/main.rs: replace + with * in cf",
                "src/main.rs: replace + with - in arr",
                "src/main.rs: replace + with * in arr",
                "src/main.rs: replace + with - in arr",
                "src/main.rs: replace + with * in arr",
            ]
        );
        assert!(
            reasons
                .iter()
                .all(|(_, reason)| *reason == FallbackReason::ConstContext)
        );
        // The FnValue mutants of `arr` are not in a const context.
        assert_eq!(plan_names(code).len() - reasons.len(), 2);
    }

    #[test]
    fn plan_file_falls_back_for_fn_value_of_impl_trait_return() {
        // Each replacement has its own concrete type, but a `match` has only one.
        let code = indoc! {"
            fn odds(limit: u32) -> impl Iterator<Item = u32> {
                (0..limit).filter(|x| x % 2 == 1)
            }
        "};
        let names = plan_names(code);
        let (fn_value, other): (Vec<_>, Vec<_>) = names
            .iter()
            .partition(|(name, _)| name.contains("replace odds ->"));
        assert!(!fn_value.is_empty());
        assert!(
            fn_value
                .iter()
                .all(|(_, reason)| *reason == Some(FallbackReason::ImplTraitReturn)),
            "{fn_value:#?}"
        );
        // Operators inside the body keep their type, so they are still embedded.
        assert!(!other.is_empty());
        assert!(
            other.iter().all(|(_, reason)| reason.is_none()),
            "{other:#?}"
        );
    }

    #[test]
    fn plan_file_falls_back_for_let_chain() {
        let code = indoc! {"
            fn f(x: Option<u32>) -> bool {
                if let Some(y) = x && y > 3 { true } else { false }
            }
        "};
        assert_eq!(
            fallbacks(code),
            [(
                "src/main.rs: replace && with || in f".to_owned(),
                FallbackReason::LetChain
            )]
        );
    }

    /// The text of a mutant's site and its alternative, or why it isn't embedded.
    type SiteText = Result<(String, String), FallbackReason>;

    /// The name of each mutant and its site text.
    fn site_texts(code: &str) -> Vec<(String, SiteText)> {
        let mutants = mutate_source_str(code, &Options::default()).unwrap();
        let refs = mutants.iter().collect_vec();
        let plan = plan_file(code, &refs).unwrap();
        mutants
            .iter()
            .zip(plan.placements)
            .map(|(m, p)| {
                (
                    m.name(false),
                    p.map(|p| (code[p.key.start..p.key.end].to_owned(), p.alternative)),
                )
            })
            .collect()
    }

    fn site_of<'a>(texts: &'a [(String, SiteText)], name: &str) -> &'a SiteText {
        &texts
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no mutant {name:?} in {texts:#?}"))
            .1
    }

    #[test]
    fn plan_file_widens_site_of_regrouping_operator_to_keep_classic_grouping() {
        // Classic `a - b * c` with `*` -> `+` is the text `a - b + c`, meaning
        // `(a - b) + c`; a site of just `b * c` would mean `a - (b + c)`.
        let code = indoc! {"
            fn f(a: u32, b: u32, c: u32, d: u32) -> bool {
                a - b * c > d
            }
        "};
        let texts = site_texts(code);
        assert_eq!(
            site_of(&texts, "src/main.rs: replace * with + in f"),
            &Ok(("a - b * c".to_owned(), "a - b + c".to_owned())),
            "the site stops below `>`, which binds looser than the new `+`"
        );
        // An operator that doesn't regroup keeps its own site.
        assert_eq!(
            site_of(&texts, "src/main.rs: replace * with / in f"),
            &Ok(("b * c".to_owned(), "b / c".to_owned()))
        );
        // Regrouping only inside the site needs no wider site: rustc parses the
        // alternative `a / b * c` as `(a / b) * c`, as it does the classic mutant.
        assert_eq!(
            site_of(&texts, "src/main.rs: replace - with / in f"),
            &Ok(("a - b * c".to_owned(), "a / b * c".to_owned()))
        );
    }

    #[test]
    fn plan_file_widens_site_of_regrouping_operator_to_root_of_binary_chain() {
        // `a - b + c` with `-` -> `*` is classically `(a * b) + c`: the `-` site
        // `a - b` would be right, but `+` -> `*` gives `a - (b * c)`, which needs
        // the whole chain.
        let code = indoc! {"
            fn f(a: u32, b: u32, c: u32) -> u32 {
                a - b + c
            }
        "};
        let texts = site_texts(code);
        assert_eq!(
            site_of(&texts, "src/main.rs: replace + with * in f"),
            &Ok(("a - b + c".to_owned(), "a - b * c".to_owned()))
        );
        assert_eq!(
            site_of(&texts, "src/main.rs: replace - with + in f"),
            &Ok(("a - b".to_owned(), "a + b".to_owned()))
        );
    }

    #[test]
    fn plan_file_widens_site_of_regrouping_operator_within_let_chain_operand() {
        let code = indoc! {"
            fn f(o: Option<u32>, b: u32, c: u32) -> bool {
                if let Some(x) = o && x - b * c > 1 { true } else { false }
            }
        "};
        let texts = site_texts(code);
        assert_eq!(
            site_of(&texts, "src/main.rs: replace * with + in f"),
            &Ok(("x - b * c".to_owned(), "x - b + c".to_owned()))
        );
        assert_eq!(
            site_of(&texts, "src/main.rs: replace && with || in f"),
            &Err(FallbackReason::LetChain)
        );
    }

    #[test]
    fn plan_file_places_lint_allow_after_inner_attributes() {
        let code = "#![deny(warnings)]\n//! Doc.\nfn f(a: u32) -> u32 {\n    a + 1\n}\n";
        let mutants = mutate_source_str(code, &Options::default()).unwrap();
        let plan = plan_file(code, &mutants.iter().collect_vec()).unwrap();
        // After a line doc comment, the attribute starts the next line.
        assert_eq!(
            &code[plan.lint_allow_at..],
            "fn f(a: u32) -> u32 {\n    a + 1\n}\n"
        );
    }

    #[test]
    fn plan_file_places_lint_allow_after_last_attribute_on_same_line() {
        let code = "#![deny(warnings)] fn f(a: u32) -> u32 {\n    a + 1\n}\n";
        let mutants = mutate_source_str(code, &Options::default()).unwrap();
        let plan = plan_file(code, &mutants.iter().collect_vec()).unwrap();
        assert_eq!(plan.lint_allow_at, "#![deny(warnings)]".len());
    }
}
