//! An implementation of the algorithm described at
//! <https://julesjacobs.com/notes/patternmatching/patternmatching.pdf>.
//!
//! Adapted from Yorick Peterse's implementation at
//! <https://github.com/yorickpeterse/pattern-matching-in-rust>. Thank you Yorick!
//!
//! > This module comment (and all the following doc comments) are a rough
//! > explanation. It's great to set some expectations on what to expect from
//! > the following code and why the data looks the way it does.
//! > If you want a more detailed explanation, the original paper is a lot more
//! > detailed!
//!
//! A case to be compiled looks a bit different from the case expressions we're
//! used to in Gleam: instead of having a variable to match on and a series of
//! branches, a `CaseToCompile` is made up of a series of branches that can each
//! contain multiple pattern checks. With a psedo-Gleam syntax, this is what it
//! would look like:
//!
//! ```text
//! case {
//!   a is Some, b is 1, c is _  -> todo
//!   a is wibble -> todo
//! }
//! ```
//!
//! > You may wonder, why are we writing branches like this? Usually a case
//! > expression matches on a single variable and each branch refers to it. For
//! > example in gleam you'd write:
//! >
//! > ```gleam
//! > case a {
//! >   Some(_) -> todo
//! >   None -> todo
//! > }
//! > ```
//! >
//! > In out representation that would turn into:
//! >
//! > ```text
//! > case {
//! >   a is Some(_) -> todo
//! >   a is None -> todo
//! > }
//! > ```
//! >
//! > This change makes it way easier to compile the pattern matching into a
//! > decision tree, because now we can add multiple checks on different
//! > variables in each branch.
//!
//! Starting from this data structure, we'll be splitting all the branches into
//! a decision tree that can be used to perform exhaustiveness checking and code
//! generation.
//!
//! At the moment this tree is not suitable for use in code generation yet as it
//! is incomplete. The tree is not correctly formed for:
//! - Bit strings
//! - String prefixes
//!
//! These were not implemented as they are more complex and I've not worked out
//! a good way to do them yet. The tricky bit is that unlike the others they are
//! not an exact match and they can overlap with other patterns. Take this
//! example:
//!
//! ```text
//! case x {
//!    "1" <> _ -> ...
//!    "12" <> _ -> ...
//!    "123" -> ...
//!    _ -> ...
//! }
//! ```
//!
//! The decision tree needs to take into account that the first pattern is a
//! super-pattern of the second, and the second is a super-pattern of the third.
//!

mod missing_patterns;
pub mod printer;

use crate::{
    ast::{AssignName, TypedClause, TypedPattern},
    type_::{
        Environment, Type, TypeValueConstructor, TypeValueConstructorField, TypeVar,
        TypeVariantConstructors, collapse_links, error::UnreachableCaseClauseReason,
        is_prelude_module, string,
    },
};
use ecow::EcoString;
use id_arena::{Arena, Id};
use itertools::Itertools;
use radix_trie::{Trie, TrieCommon};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    hash::Hash,
    sync::Arc,
};

/// A single branch composing a `case` expression to be compiled into a decision
/// tree.
///
/// As shown in the module documentation, branches are a bit different from the
/// usual branches we see in Gleam's case expressions. Each branch can perform
/// multiple checks (each on a different variable, which appears in the check
/// itself!):
///
/// ```text
/// a is Some, b is 1 if condition -> todo
/// ─┬───────  ─┬──── ─┬──────────    ─┬──
///  │          │      │               ╰── body: an arbitrary expression
///  │          │      ╰── guard: an additional boolean condition
///  ╰──────────┴── checks: check that a variable matches with a pattern
/// ─┬────────────────────────────────────
///  ╰── branch: one of the branches making up a pattern matching expression
/// ```
///
/// As shown here a branch can also optionally include a guard with a boolean
/// condition and is followed by a body that is to be executed if all the checks
/// match (and the guard evaluates to true).
///
#[derive(Clone, Eq, PartialEq, Debug)]
struct Branch {
    /// Each branch is identified by a numeric index, so we can nicely
    /// report errors once we find something's wrong with a branch.
    ///
    clause_index: usize,

    /// Each alternative pattern in an alternative pattern matching (e.g.
    /// `one | two | three -> todo`) gets turned into its own branch in this
    /// internal representation. So we also keep track of the index of the
    /// alternative this comes from (0 being the first one and so on...)
    ///
    alternative_index: usize,
    checks: Vec<PatternCheck>,
    guard: Option<usize>,
    body: Body,
}

impl Branch {
    fn new(
        clause_index: usize,
        alternative_index: usize,
        checks: Vec<PatternCheck>,
        has_guard: bool,
    ) -> Self {
        Self {
            clause_index,
            alternative_index,
            checks,
            guard: if has_guard { Some(clause_index) } else { None },
            body: Body::new(clause_index),
        }
    }

    /// Removes and returns a `PatternCheck` on the given variable from this
    /// branch.
    ///
    fn pop_check_on_var(&mut self, var: &Variable) -> Option<PatternCheck> {
        let index = self.checks.iter().position(|check| check.var == *var)?;
        Some(self.checks.remove(index))
    }

    fn add_check(&mut self, check: PatternCheck) {
        self.checks.push(check);
    }

    /// To simplify compiling the pattern we can get rid of all catch-all
    /// patterns that are guaranteed to match by turning those into assignments.
    ///
    /// What does this look like in practice?  Let's go over an example.
    /// Let's say we have this case to compile:
    ///
    /// ```gleam
    /// case a {
    ///   Some(1) -> Some(2)
    ///   otherwise -> otherwise
    /// }
    /// ```
    ///
    /// In our internal representation this would become:
    ///
    /// ```text
    /// case {
    ///   a is Some(1) -> Some(2)
    ///   a is otherwise -> otherwise
    ///   ─┬────────────
    ///    ╰── `a` will always match with this "catch all" variable pattern
    /// }
    /// ```
    ///
    /// Focusing on the last branch, we can remove that check that always matches
    /// by keeping track in its body of the correspondence. So it would end up
    /// looking like this:
    ///
    /// ```text
    /// case {
    ///   a is Some(1) -> Some(2)
    ///   ∅ -> {
    ///   ┬
    ///   ╰── This represents the fact that there's no checks left for this branch!
    ///       So we can make another observation: if there's no checks left in a
    ///       branch we know it will always match and we can produce a leaf in the
    ///       decision tree (there's an exception when we have guards, but we'll
    ///       get to it later)!
    ///
    ///     let otherwise = a
    ///     ─┬───────────────
    ///      ╰── And now we can understand what those `bindings` at the start of
    ///          a body are: as we remove variable patterns, we will rewrite those
    ///          as assignments at the top of the body of the corresponding branch.
    ///
    ///     otherwise
    ///   }
    /// }
    /// ```
    ///
    fn move_unconditional_patterns(&mut self, compiler: &Compiler<'_>) {
        self.checks.retain_mut(|check| {
            loop {
                match compiler.pattern(check.pattern) {
                    // Variable patterns always match, so we move those to the body
                    // and remove them from the branch's checks.
                    Pattern::Variable { name } => {
                        self.body.assign(name.clone(), check.var.clone());
                        return false;
                    }
                    // A discard pattern always matches, but since the value is not
                    // used we can just remove it without even adding an assignment
                    // to the body!
                    Pattern::Discard => return false,
                    // Assigns are kind of special: they get turned into assignments
                    // (shocking) but then we can't discard the pattern they wrap.
                    // So we replace the assignment pattern with the one it's wrapping
                    // and try again.
                    Pattern::Assign { name, pattern } => {
                        self.body.assign(name.clone(), check.var.clone());
                        check.pattern = *pattern;
                        continue;
                    }
                    // All other patterns are not unconditional, so we just keep them.
                    _ => return true,
                }
            }
        });
    }
}

/// The body of a branch. It always starts with a series of variable assignments
/// in the form: `let a = b`. As explained in `move_unconditional_patterns`' doc,
/// each body starts with a series of assignments we keep track of as we're
/// compiling each branch.
///
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Body {
    /// Any variables to bind before running the code.
    ///
    /// The tuples are in the form `(name, value)`, so `(wibble, var)`
    /// corresponds to `let wibble = var`.
    ///
    bindings: Vec<(EcoString, Variable)>,

    /// The index of the clause in the case expression that should be run.
    ///
    clause_index: usize,
}

impl Body {
    pub fn new(clause_index: usize) -> Self {
        Self {
            bindings: vec![],
            clause_index,
        }
    }

    /// Adds a new assignment to the body, binding `let var = value`
    ///
    pub fn assign(&mut self, var: EcoString, value: Variable) {
        self.bindings.push((var, value));
    }
}

/// A user defined pattern such as `Some((x, 10))`.
/// This is a bit simpler than the full fledged `TypedPattern` used for code analysis
/// and only focuses on the relevant bits needed to perform exhaustiveness checking
/// and code generation.
///
/// Using this simplified version of a pattern for the case compiler makes it a
/// whole lot simpler and more efficient (patterns will have to be cloned, so
/// we use an arena to allocate those and only store ids to make this operation
/// extra cheap).
///
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum Pattern {
    Discard,
    Int {
        value: EcoString,
    },
    Float {
        value: EcoString,
    },
    String {
        value: EcoString,
    },
    StringPrefix {
        prefix: EcoString,
        rest: Id<Pattern>,
    },
    Assign {
        name: EcoString,
        pattern: Id<Pattern>,
    },
    Variable {
        name: EcoString,
    },
    Tuple {
        elements: Vec<Id<Pattern>>,
    },
    Variant {
        index: usize,
        fields: Vec<Id<Pattern>>,
    },
    NonEmptyList {
        first: Id<Pattern>,
        rest: Id<Pattern>,
    },
    EmptyList,
    // TODO: Compile the matching within the bit strings
    BitArray {
        value: EcoString,
    },
}

impl Pattern {
    /// Each pattern (with a couple exceptions) can be turned into a
    /// simpler `RuntimeCheck`: that is a check that can be performed at runtime
    /// to make sure a `PatternCheck` can succeed on a specific value.
    ///
    fn to_runtime_check_kind(&self) -> Option<RuntimeCheckKind> {
        let kind = match self {
            // These patterns are unconditional: they will always match and be moved
            // out of a branch's checks. So there's no corresponding runtime check
            // we can perform for them.
            Pattern::Discard | Pattern::Variable { .. } | Pattern::Assign { .. } => return None,
            Pattern::Int { value } => RuntimeCheckKind::Int {
                value: value.clone(),
            },
            Pattern::Float { value } => RuntimeCheckKind::Float {
                value: value.clone(),
            },
            Pattern::String { value } => RuntimeCheckKind::String {
                value: value.clone(),
            },
            Pattern::StringPrefix { prefix, .. } => RuntimeCheckKind::StringPrefix {
                prefix: prefix.clone(),
            },
            Pattern::Tuple { elements } => RuntimeCheckKind::Tuple {
                size: elements.len(),
            },
            Pattern::Variant { index, .. } => RuntimeCheckKind::Variant { index: *index },
            Pattern::BitArray { value } => RuntimeCheckKind::BitArray {
                value: value.clone(),
            },
            Pattern::NonEmptyList { .. } => RuntimeCheckKind::NonEmptyList,
            Pattern::EmptyList => RuntimeCheckKind::EmptyList,
        };

        Some(kind)
    }

    fn is_matching_on_unreachable_variant(&self, branch_mode: &BranchMode) -> bool {
        match (self, branch_mode) {
            (
                Self::Variant { index, .. },
                BranchMode::NamedType {
                    inferred_variant: Some(variant),
                    ..
                },
            ) if index != variant => true,
            _ => false,
        }
    }
}

/// A single check making up a branch, checking that a variable matches with a
/// given pattern. For example, the following branch has 2 checks:
///
/// ```text
/// a is Some, b is 1 -> todo
/// ┬    ─┬──
/// │     ╰── This is the pattern being checked
/// ╰── This is the variable being pattern matched on
/// ─┬─────── ─┬────
///  ╰─────────┴── Two `PatternCheck`s
/// ```
///
#[derive(Clone, Eq, PartialEq, Debug)]
struct PatternCheck {
    var: Variable,
    pattern: Id<Pattern>,
}

/// This is one of the checks we can take at runtime to decide how to move
/// forward in the decision tree.
///
/// After performing a successful check on a value we will discover something
/// about its shape: it might be an int, an variant of a custom type, ...
/// Some values (like variants and lists) might hold onto additional data we
/// will have to pattern match on: in order to do that we need a name to refer
/// to those new variables we've discovered after performing a check. That's
/// what `args` is for.
///
/// Let's have a look at an example. Imagine we have a pattern like this one:
/// `a is Wibble(1, _, [])`; after performing a runtime check to make sure `a`
/// is indeed a `Wibble`, we'll need to perform additional checks on it's
/// arguments: that pattern will be replaced by three new ones `a0 is 1`,
/// `a1 is _` and `a2 is []`. Those new variables are the `args`.
///
#[derive(Clone, Debug)]
pub enum RuntimeCheck {
    Int {
        value: EcoString,
    },
    Float {
        value: EcoString,
    },
    String {
        value: EcoString,
    },
    StringPrefix {
        prefix: EcoString,
        rest: Variable,
    },
    Tuple {
        size: usize,
        elements: Vec<Variable>,
    },
    BitArray {
        value: EcoString,
    },
    Variant {
        index: usize,
        fields: Vec<Variable>,
    },
    NonEmptyList {
        first: Variable,
        rest: Variable,
    },
    EmptyList,
}

impl RuntimeCheck {
    fn kind(&self) -> RuntimeCheckKind {
        match self {
            RuntimeCheck::Int { value } => RuntimeCheckKind::Int {
                value: value.clone(),
            },
            RuntimeCheck::Float { value } => RuntimeCheckKind::Float {
                value: value.clone(),
            },
            RuntimeCheck::String { value } => RuntimeCheckKind::String {
                value: value.clone(),
            },
            RuntimeCheck::StringPrefix { prefix, rest: _ } => RuntimeCheckKind::StringPrefix {
                prefix: prefix.clone(),
            },
            RuntimeCheck::Tuple { size, elements: _ } => RuntimeCheckKind::Tuple { size: *size },
            RuntimeCheck::BitArray { value } => RuntimeCheckKind::BitArray {
                value: value.clone(),
            },
            RuntimeCheck::Variant { index, fields: _ } => {
                RuntimeCheckKind::Variant { index: *index }
            }
            RuntimeCheck::EmptyList => RuntimeCheckKind::EmptyList,
            RuntimeCheck::NonEmptyList { first: _, rest: _ } => RuntimeCheckKind::NonEmptyList,
        }
    }
}

#[derive(Eq, PartialEq, Clone, Hash, Debug)]
pub enum RuntimeCheckKind {
    Int { value: EcoString },
    Float { value: EcoString },
    String { value: EcoString },
    StringPrefix { prefix: EcoString },
    Tuple { size: usize },
    BitArray { value: EcoString },
    Variant { index: usize },
    EmptyList,
    NonEmptyList,
}

/// A variable that can be matched on in a branch.
///
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct Variable {
    id: usize,
    type_: Arc<Type>,
}

impl Variable {
    fn new(id: usize, type_: Arc<Type>) -> Self {
        Self { id, type_ }
    }

    /// Builds a `PatternCheck` that checks this variable matches the given pattern.
    /// So we can build pattern checks the same way we informally describe them:
    /// ```text
    /// var is pattern
    /// ```
    /// With this builder method would become:
    /// ```rs
    /// var.is(pattern)
    /// ```
    ///
    fn is(&self, pattern: Id<Pattern>) -> PatternCheck {
        PatternCheck {
            var: self.clone(),
            pattern,
        }
    }
}

#[derive(Debug)]
/// Different types need to be handled differently when compiling a case expression
/// into a decision tree. There's some types that have infinite matching patterns
/// (like ints, strings, ...) and thus will always need a fallback option.
///
/// Other types, like custom types, only have a well defined and finite number
/// of patterns that could match: when matching on a `Result` we know that we can
/// only have an `Ok(_)` and an `Error(_)`, anything else would end up being a
/// type error!
///
/// So this enum is used to pick the correct strategy to compile a case that's
/// performing a `PatternCheck` on a variable with a specific type.
///
enum BranchMode {
    /// This covers numbers, functions, variables, and bitarrays.
    ///
    /// TODO)) In the future it won't be the case: bitarrays will be special
    /// cased to improve on exhaustiveness checking and to be used for code
    /// generation.
    ///
    Infinite,
    Tuple {
        elements: Vec<Arc<Type>>,
    },
    List {
        inner_type: Arc<Type>,
    },
    NamedType {
        constructors: Vec<TypeValueConstructor>,
        inferred_variant: Option<usize>,
    },
}

impl BranchMode {
    fn needs_fallback(&self) -> bool {
        match self {
            BranchMode::Infinite => true,
            BranchMode::Tuple { .. } | BranchMode::List { .. } | BranchMode::NamedType { .. } => {
                false
            }
        }
    }
}

impl Variable {
    fn branch_mode(&self, env: &Environment<'_>) -> BranchMode {
        match collapse_links(self.type_.clone()).as_ref() {
            Type::Fn { .. } | Type::Var { .. } => BranchMode::Infinite,
            Type::Named { module, name, .. }
                if is_prelude_module(module)
                    && (name == "Int"
                        || name == "Float"
                        || name == "BitArray"
                        || name == "String") =>
            {
                BranchMode::Infinite
            }

            Type::Named {
                module, name, args, ..
            } if is_prelude_module(module) && name == "List" => BranchMode::List {
                inner_type: args.first().expect("list has a type argument").clone(),
            },

            Type::Tuple { elems } => BranchMode::Tuple {
                elements: elems.clone(),
            },

            Type::Named {
                module,
                name,
                args,
                inferred_variant,
                ..
            } => {
                let constructors = ConstructorSpecialiser::specialise_constructors(
                    env.get_constructors_for_type(module, name)
                        .expect("Custom type variants must exist"),
                    args.as_slice(),
                );

                let inferred_variant = inferred_variant.map(|i| i as usize);
                BranchMode::NamedType {
                    constructors,
                    inferred_variant,
                }
            }
        }
    }
}

/// This is the decision tree that a pattern matching expression gets turned
/// into: it's a tree-like structure where each path to a root node contains a
/// series of checks to perform at runtime to understand if a value matches with
/// a given pattern.
///
pub enum Decision {
    /// This is the final node of the tree, once we get to this one we know we
    /// have a body to run because a given pattern matched.
    ///
    Run {
        // todo)) since the tree is not used for code generation, this field is unused.
        // But it will be useful once we also use this for code gen purposes and not
        // just for exhaustiveness checking
        #[allow(dead_code)]
        body: Body,
    },

    /// We have to make this decision when we run into a branch that also has a
    /// guard: if it is true we can finally run the body of the branch, stored in
    /// `if_true`.
    /// If it is false we might still have to take other decisions and so we might
    /// have another `DecisionTree` to traverse, stored in `if_false`.
    ///
    Guard {
        // todo)) since the tree is not used for code generation, the `guard` and
        // `if_true` fields are unused.
        // But they will be useful once we also use this for code gen purposes and not
        // just for exhaustiveness checking
        #[allow(dead_code)]
        guard: usize,
        #[allow(dead_code)]
        if_true: Body,
        if_false: Box<Decision>,
    },

    /// When reaching this node we'll have to see if any of the possible checks
    /// in `choices` will succeed on `var`. If it does, we know that's the path
    /// we have to go down to. If none of the checks matches, then we'll have to
    /// go down the `fallback` branch.
    ///
    Switch {
        var: Variable,
        choices: Vec<(RuntimeCheck, Box<Decision>)>,
        fallback: Box<Decision>,
    },

    /// This is similar to a `Switch` node: we're still picking a possible path
    /// to follow based on a runtime check. The key difference is that we know
    /// that one of those is always going to match and so there's no use for a
    /// fallback branch.
    ///
    /// This is used when matching on custom types (and lists!) when we know
    /// that there's a limited number of choices and exhaustiveness checking
    /// ensures we'll always deal with all the possible cases.
    ///
    ExhaustiveSwitch {
        var: Variable,
        choices: Vec<(RuntimeCheck, Box<Decision>)>,
    },

    /// This is a special node: it represents a missing pattern. If a tree
    /// contains such a node, then we know that the patterns it was compiled
    /// from are not exhaustive and the path leading to this node will describe
    /// what kind of pattern doesn't match!
    ///
    Fail,
}

impl Decision {
    pub fn run(body: Body) -> Self {
        Decision::Run { body }
    }

    pub fn guard(guard: usize, if_true: Body, if_false: Self) -> Self {
        Decision::Guard {
            guard,
            if_true,
            if_false: Box::new(if_false),
        }
    }

    pub fn switch(
        var: Variable,
        choices: Vec<(RuntimeCheck, Box<Decision>)>,
        fallback: Decision,
    ) -> Self {
        Self::Switch {
            var,
            choices,
            fallback: Box::new(fallback),
        }
    }

    fn exhaustive_switch(var: Variable, choices: Vec<(RuntimeCheck, Box<Decision>)>) -> Decision {
        Self::ExhaustiveSwitch { var, choices }
    }
}

/// The `case` compiler itself (shocking, I know).
///
#[derive(Debug)]
struct Compiler<'a> {
    environment: &'a Environment<'a>,
    patterns: Arena<Pattern>,
    variable_id: usize,
    diagnostics: Diagnostics,
}

/// The result of compiling a pattern match expression.
///
pub struct Match {
    pub tree: Decision,
    pub diagnostics: Diagnostics,
    pub subject_variables: Vec<Variable>,
}

/// Whether a clause is reachable, or why it is unreachable.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    Reachable,
    Unreachable(UnreachableCaseClauseReason),
}

impl Match {
    pub fn is_reachable(&self, clause: usize) -> Reachability {
        if self.diagnostics.reachable.contains(&clause) {
            Reachability::Reachable
        } else if self.diagnostics.match_impossible_variants.contains(&clause) {
            Reachability::Unreachable(UnreachableCaseClauseReason::ImpossibleVariant)
        } else {
            Reachability::Unreachable(UnreachableCaseClauseReason::DuplicatePattern)
        }
    }

    pub fn missing_patterns(&self, environment: &Environment<'_>) -> Vec<EcoString> {
        missing_patterns::missing_patterns(self, environment)
    }
}

/// A type for storing diagnostics produced by the decision tree compiler.
///
#[derive(Debug)]
pub struct Diagnostics {
    /// A flag indicating the match is missing one or more pattern.
    pub missing: bool,

    /// The right-hand sides that are reachable.
    /// If a right-hand side isn't in this list it means its pattern is
    /// redundant.
    pub reachable: HashSet<usize>,

    /// Clauses which match on variants of a type which the compiler
    /// can tell will never be present, due to variant inference.
    pub match_impossible_variants: HashSet<usize>,
}

impl<'a> Compiler<'a> {
    fn new(environment: &'a Environment<'a>, variable_id: usize, patterns: Arena<Pattern>) -> Self {
        Self {
            environment,
            patterns,
            variable_id,
            diagnostics: Diagnostics {
                missing: false,
                reachable: HashSet::new(),
                match_impossible_variants: HashSet::new(),
            },
        }
    }

    fn pattern(&self, pattern_id: Id<Pattern>) -> &Pattern {
        self.patterns.get(pattern_id).expect("unknown pattern id")
    }

    /// Returns a new fresh variable (i.e. guaranteed to have a unique `variable_id`)
    /// with the given type.
    ///
    fn fresh_variable(&mut self, type_: Arc<Type>) -> Variable {
        let var = Variable::new(self.variable_id, type_);
        self.variable_id += 1;
        var
    }

    fn mark_as_reached(&mut self, branch: &Branch) {
        let _ = self.diagnostics.reachable.insert(branch.clause_index);
    }

    fn mark_as_matching_impossible_variant(&mut self, branch: &Branch) {
        let _ = self.diagnostics.reachable.remove(&branch.clause_index);
        let _ = self
            .diagnostics
            .match_impossible_variants
            .insert(branch.clause_index);
    }

    fn compile(&mut self, mut branches: VecDeque<Branch>) -> Decision {
        branches
            .iter_mut()
            .for_each(|branch| branch.move_unconditional_patterns(self));

        let Some(first_branch) = branches.front() else {
            // If there's no branches, that means we have a pattern that is not
            // exhaustive as there's nothing that could match!
            self.diagnostics.missing = true;
            return Decision::Fail;
        };

        self.mark_as_reached(first_branch);

        // In order to compile the branches, we need to pick a `PatternCheck` from
        // the first branch, and use the variable it's pattern matching on to create
        // a new node in the tree. All the branches will be split into different
        // possible paths of this tree.
        match find_pivot_check(first_branch, &branches) {
            Some(PatternCheck { var, .. }) => self.split_and_compile_with_pivot_var(var, branches),

            // If the branch has no remaining checks, it means that we've moved all
            // its variable patterns as assignments into the body and there's no
            // additional checks remaining. So the only thing left that could result
            // in the match failing is the additional guard.
            None => match first_branch.guard {
                // If there's no guard we're in the following situation:
                // `∅ -> body`. It means that this branch will always match no
                // matter what, all the remaining branches are just discarded and
                // we can produce a terminating node to run the body
                // unconditionally.
                None => Decision::run(first_branch.body.clone()),
                // If we have a guard we're in this scenario:
                // `∅ if condition -> body`. We can produce a `Guard` node:
                // if the condition evaluates to `True` we can run its body.
                // Otherwise, we'll have to keep looking at the remaining branches
                // to know what to do if this branch doesn't match.
                Some(guard) => {
                    let if_true = first_branch.body.clone();
                    // All the remaining branches will be compiled and end up
                    // in the path of the tree to choose if the guard is false.
                    let _ = branches.pop_front();
                    let if_false = self.compile(branches);
                    Decision::guard(guard, if_true, if_false)
                }
            },
        }
    }

    fn split_and_compile_with_pivot_var(
        &mut self,
        pivot_var: Variable,
        branches: VecDeque<Branch>,
    ) -> Decision {
        // We first try and come up with a list of all the runtime checks we might
        // have to perform on the variable at runtime. In most cases it's a limited
        // number of checks that we know before hand (for example, when matching
        // on a list, or on a custom type).
        let branch_mode = pivot_var.branch_mode(self.environment);
        let known_checks = match &branch_mode {
            // If the type being matched on is infinite there's no known runtime
            // check we could come up with in advance. So we'll build them as
            // we go.
            BranchMode::Infinite => vec![],

            // If the type is a tuple there's only one runtime check we could
            // perform that actually makes sense.
            BranchMode::Tuple { elements } => vec![self.is_tuple_check(elements)],

            // If the type being matched on is a list we know the resulting
            // decision tree node is only ever going to have two different paths:
            // one to follow if the list is empty, and one to follow if it's not.
            BranchMode::List { inner_type } => {
                vec![
                    RuntimeCheck::EmptyList,
                    self.is_list_check(inner_type.clone()),
                ]
            }

            // If we know that a specific variant is inferred we will require just
            // that one and not all the other ones we know for sure are not going to
            // be there.
            BranchMode::NamedType {
                constructors,
                inferred_variant: Some(index),
            } => {
                let constructor = constructors
                    .get(*index)
                    .expect("wrong index for inferred variant");
                vec![self.is_variant_check(*index, constructor)]
            }

            // Otherwise we know we'll need a check for each of its possible variants.
            BranchMode::NamedType { constructors, .. } => constructors
                .iter()
                .enumerate()
                .map(|(index, constructor)| self.is_variant_check(index, constructor))
                .collect_vec(),
        };

        // We then split all the branches using these checks and compile the
        // choices they've been split up into.
        let mut splitter = BranchSplitter::from_checks(known_checks);
        self.split_branches(&mut splitter, branches, pivot_var.clone(), &branch_mode);
        let choices = splitter
            .choices
            .into_iter()
            .map(|(check, branches)| (check, Box::new(self.compile(branches))))
            .collect_vec();

        if branch_mode.needs_fallback() {
            // If the branching is infinite, that means we always need to also have
            // a fallback (imagine you're pattern matching on an `Int` and put no
            // `_` at the end of the case expression).
            let fallback = self.compile(splitter.fallback);
            Decision::switch(pivot_var, choices, fallback)
        } else if choices.is_empty() {
            // If the branching doesn't need any fallback but we ended up with no
            // checks it means we're trying to pattern match on an external type
            // but haven't provided a catch-all case.
            // That's never going to match, so we produce a failure node.
            self.diagnostics.missing = true;
            Decision::Fail
        } else {
            // Otherwise we know that one of the possible runtime checks is always
            // going to succeed and there's no need to also have a fallback branch.
            Decision::exhaustive_switch(pivot_var, choices)
        }
    }

    fn split_branches(
        &mut self,
        splitter: &mut BranchSplitter,
        branches: VecDeque<Branch>,
        pivot_var: Variable,
        branch_mode: &BranchMode,
    ) {
        for mut branch in branches {
            let Some(pattern_check) = branch.pop_check_on_var(&pivot_var) else {
                // If the branch doesn't perform any check on the pivot variable, it means
                // it could still match no matter what shape `pivot_var` has. So we must
                // add it as a fallback branch, that is a branch that is still relevant
                // for all possible paths in the decision tree.
                splitter.add_fallback_branch(branch);
                continue;
            };

            let checked_pattern = self.pattern(pattern_check.pattern).clone();
            if checked_pattern.is_matching_on_unreachable_variant(branch_mode) {
                self.mark_as_matching_impossible_variant(&branch);
                continue;
            }

            splitter.add_checked_branch(checked_pattern, branch, branch_mode, self);
        }
    }

    /// Turns a `RuntimeCheckKind` into a new `RuntimeCheck` by coming up with
    /// the needed new fresh variables.
    /// All the type information needed to create these variables is in the
    /// `branch_mode` arg.
    ///
    fn fresh_runtime_check(
        &mut self,
        kind: RuntimeCheckKind,
        branch_mode: &BranchMode,
    ) -> RuntimeCheck {
        match (kind, branch_mode) {
            (RuntimeCheckKind::Int { value }, _) => RuntimeCheck::Int {
                value: value.clone(),
            },
            (RuntimeCheckKind::Float { value }, _) => RuntimeCheck::Float {
                value: value.clone(),
            },
            (RuntimeCheckKind::String { value }, _) => RuntimeCheck::String {
                value: value.clone(),
            },
            (RuntimeCheckKind::BitArray { value }, _) => RuntimeCheck::BitArray {
                value: value.clone(),
            },
            (RuntimeCheckKind::StringPrefix { prefix }, _) => RuntimeCheck::StringPrefix {
                prefix: prefix.clone(),
                rest: self.fresh_variable(string()),
            },
            (RuntimeCheckKind::Tuple { .. }, BranchMode::Tuple { elements }) => {
                self.is_tuple_check(elements)
            }
            (RuntimeCheckKind::Variant { index }, BranchMode::NamedType { constructors, .. }) => {
                self.is_variant_check(
                    index,
                    constructors.get(index).expect("unknown variant index"),
                )
            }
            (RuntimeCheckKind::EmptyList, _) => RuntimeCheck::EmptyList,
            (RuntimeCheckKind::NonEmptyList, BranchMode::List { inner_type }) => {
                self.is_list_check(inner_type.clone())
            }
            (_, _) => unreachable!("type checking should make this impossible"),
        }
    }

    /// Comes up with new pattern cecks that have to match in case a given
    /// runtime check succeeds for the given pattern.
    ///
    /// Let's make an example: when we have a pattern - say `a is Wibble(1, [])` -
    /// we come up with a runtime check to perform on it. For our example the
    /// runtime check is to make sure that `a` is indeed a `Wibble` variant.
    /// However, after successfully performing that check we're left with much to
    /// do! We know that `a` is `Wibble` but now we'll have to make sure that its
    /// inner arguments also match the given patterns. So the new additional checks
    /// we have to add are `a0 is 1, a1 is []` (where `a0` and `a1` are the fresh
    /// variable names we use to refer to the constructor's arguments).
    ///
    fn new_checks(
        &mut self,
        for_pattern: &Pattern,
        after_succeding_check: &RuntimeCheck,
    ) -> Vec<PatternCheck> {
        match (for_pattern, after_succeding_check) {
            // These patterns never result in adding new checks. After a runtime
            // check matches on them there's nothing else to discover.
            (
                Pattern::Discard
                | Pattern::Assign { .. }
                | Pattern::Variable { .. }
                | Pattern::Int { .. }
                | Pattern::Float { .. }
                | Pattern::BitArray { .. }
                | Pattern::EmptyList,
                _,
            )
            | (Pattern::String { .. }, RuntimeCheck::String { .. }) => vec![],

            // After making sure a value is not an empty list we'll have to perform
            // additional checks on its first item and on the tail.
            (
                Pattern::NonEmptyList {
                    first: first_pattern,
                    rest: rest_pattern,
                },
                RuntimeCheck::NonEmptyList {
                    first: first_variable,
                    rest: rest_variable,
                },
            ) => vec![
                first_variable.is(*first_pattern),
                rest_variable.is(*rest_pattern),
            ],

            // After making sure a value is a specific variant we'll have to check each
            // of its arguments respects the given patterns (as shown in the doc example for
            // this function!)
            (
                Pattern::Variant {
                    fields: patterns, ..
                },
                RuntimeCheck::Variant {
                    fields: variables, ..
                },
            ) => (variables.iter().zip(patterns))
                .map(|(field, pattern)| field.is(*pattern))
                .collect_vec(),

            // Tuples are exactly the same as variants: after making sure we're dealing with
            // a tuple, we will have to check that each of its elements matches the given
            // pattern: `a is #(1, _)` will result in the following checks
            // `a0 is 1, a1 is _` (where `a0` and `a1` are fresh variable names we use to
            // refer to each of the tuple's elements).
            (
                Pattern::Tuple { elements: patterns },
                RuntimeCheck::Tuple {
                    elements: variables,
                    ..
                },
            ) => (variables.iter().zip(patterns))
                .map(|(element, pattern)| element.is(*pattern))
                .collect_vec(),

            // Strings are quite fun: if we've checked at runtime a string starts with a given
            // prefix and we want to check that it's some overlapping literal value we'll still
            // have some amount of work to perform.
            //
            // Let's have a look at an example: the pattern we care about is `a is "wibble"`
            // and we've just successfully ran the runtime check for `a is "wib" <> rest`.
            // So we know the string already starts with `"wib"` what we have to check now
            // is that the remaining part `rest` is `"ble"`.
            (Pattern::String { value }, RuntimeCheck::StringPrefix { prefix, rest, .. }) => {
                let remaining = value.strip_prefix(prefix.as_str()).unwrap_or(value);
                vec![rest.is(self.string_pattern(remaining))]
            }

            // String prefixes are similar to strings, but a bit more involved. Let's say we're
            // checking the pattern:
            //
            // ```text
            // "wibblest" <> rest1
            // ─┬────────
            //  ╰── We will refer to this as `prefix1`
            // ```
            //
            // And we know that the following overlapping runtime check has already succeeded:
            //
            // ```text
            // "wibble" <> rest0
            // ─┬──────
            //  ╰── We will refer to this as `prefix0`
            // ```
            //
            // We're lucky because we now know quite a bit about the shape of the string. Since
            // we know it already starts with `"wibble"` we can just check that the remaining
            // part after that starts with the missing part of the prefix:
            // `prefix0 is "st" <> rest1`.
            (
                Pattern::StringPrefix {
                    prefix: prefix1,
                    rest: rest1,
                },
                RuntimeCheck::StringPrefix {
                    prefix: prefix0,
                    rest: rest0,
                },
            ) => {
                let remaining = prefix1.strip_prefix(prefix0.as_str()).unwrap_or(prefix1);
                vec![rest0.is(self.string_prefix_pattern(remaining, *rest1))]
            }

            (_, _) => unreachable!("invalid pattern overlapping"),
        }
    }

    /// Builds an `IsVariant` runtime check, coming up with new fresh variable names
    /// for its arguments.
    ///
    fn is_variant_check(
        &mut self,
        index: usize,
        constructor: &TypeValueConstructor,
    ) -> RuntimeCheck {
        RuntimeCheck::Variant {
            index,
            fields: constructor
                .parameters
                .iter()
                .map(|parameter| parameter.type_.clone())
                .map(|type_| self.fresh_variable(type_))
                .collect_vec(),
        }
    }

    /// Builds an `IsNonEmptyList` runtime check, coming up with fresh variable
    /// names for its arguments.
    ///
    fn is_list_check(&mut self, inner_type: Arc<Type>) -> RuntimeCheck {
        RuntimeCheck::NonEmptyList {
            first: self.fresh_variable(inner_type.clone()),
            rest: self.fresh_variable(Arc::new(Type::list(inner_type))),
        }
    }

    /// Builds an `IsTuple` runtime check, coming up with fresh variable
    /// names for its arguments.
    ///
    fn is_tuple_check(&mut self, elements: &[Arc<Type>]) -> RuntimeCheck {
        RuntimeCheck::Tuple {
            size: elements.len(),
            elements: elements
                .iter()
                .map(|type_| self.fresh_variable(type_.clone()))
                .collect_vec(),
        }
    }

    /// Allocates a new `StringPattern` with the given value.
    ///
    fn string_pattern(&mut self, value: &str) -> Id<Pattern> {
        self.patterns.alloc(Pattern::String {
            value: EcoString::from(value),
        })
    }

    /// Allocates a new `StringPrefix` pattern with the given prefix and pattern
    /// for the rest of the string.
    ///
    fn string_prefix_pattern(&mut self, prefix: &str, rest: Id<Pattern>) -> Id<Pattern> {
        self.patterns.alloc(Pattern::StringPrefix {
            prefix: EcoString::from(prefix),
            rest,
        })
    }
}

/// Returns a pattern check from `first_branch` to be used as a pivot to split all
/// the `branches`.
///
fn find_pivot_check(first_branch: &Branch, branches: &VecDeque<Branch>) -> Option<PatternCheck> {
    // To try and minimise code duplication, we use the following heuristic: we
    // choose the check matching on the variable that is referenced the most
    // across all checks in all branches.
    let mut var_references = HashMap::new();
    for branch in branches {
        for check in &branch.checks {
            let _ = var_references
                .entry(check.var.id)
                .and_modify(|references| *references += 1)
                .or_insert(0);
        }
    }

    first_branch
        .checks
        .iter()
        .max_by_key(|check| var_references.get(&check.var.id).cloned().unwrap_or(0))
        .cloned()
}

/// A handy data structure we use to split branches in different possible paths
/// based on a check.
///
struct BranchSplitter {
    pub choices: Vec<(RuntimeCheck, VecDeque<Branch>)>,
    pub fallback: VecDeque<Branch>,
    /// This is used to allow quickly looking up a choice in the `choices`
    /// vector, without loosing track of the checks' order.
    indices: HashMap<RuntimeCheckKind, usize>,

    /// This is used to store the indices of just the prefix checks as they have
    /// different rules from all the other `RuntimeCheckKinds` whose indices are
    /// instead stored in the `indices` field.
    ///
    /// We discuss this in more detail in the `index_of_overlapping_runtime_check`
    /// function!
    prefix_indices: Trie<String, usize>,
}

impl BranchSplitter {
    /// Creates a new splitter with the given starting checks.
    ///
    fn from_checks(checks: Vec<RuntimeCheck>) -> Self {
        let mut choices = Vec::with_capacity(checks.len());
        let mut indices = HashMap::new();

        for (index, runtime_check) in checks.into_iter().enumerate() {
            let _ = indices.insert(runtime_check.kind(), index);
            choices.push((runtime_check, VecDeque::new()));
        }

        Self {
            fallback: VecDeque::new(),
            choices,
            indices,
            prefix_indices: Trie::new(),
        }
    }

    /// Add a fallback branch: this is a branch that is relevant to all possible
    /// paths as it could still run, no matter the result of any of the `Check`s
    /// we've stored!
    ///
    fn add_fallback_branch(&mut self, branch: Branch) {
        self.choices
            .iter_mut()
            .for_each(|(_, branches)| branches.push_back(branch.clone()));
        self.fallback.push_back(branch);
    }

    /// Given a branch and the pattern its using to check on the pivot variable,
    /// adds it to the paths where it's relevant, that is where we know from
    /// previous checks that this pattern has a chance of matching.
    ///
    fn add_checked_branch(
        &mut self,
        pattern: Pattern,
        branch: Branch,
        branch_mode: &BranchMode,
        compiler: &mut Compiler<'_>,
    ) {
        let kind = pattern
            .to_runtime_check_kind()
            .expect("no unconditional patterns left");

        let indices_of_overlapping_checks = self.indices_of_overlapping_checks(&kind);
        if indices_of_overlapping_checks.is_empty() {
            // This is a new choice we haven't yet discovered as it is not overlapping
            // with any of the existing ones. So we add it as a possible new path
            // we might have to go down to in the decision tree.
            self.save_index_of_new_choice(kind.clone());
            let mut branches = self.fallback.clone();
            branches.push_back(branch);
            let check = compiler.fresh_runtime_check(kind, branch_mode);
            self.choices.push((check, branches));
        } else {
            // Otherwise, we know that the check for this branch overlaps with
            // (possibly more than one) existing checks and so is relevant only
            // as part of those existing paths.
            // We'll add the branch with its newly discovered checks only to those
            // paths.
            for index in indices_of_overlapping_checks {
                let (overlapping_check, branches) = self
                    .choices
                    .get_mut(index)
                    .expect("check to already be a choice");

                let mut branch = branch.clone();
                for new_check in compiler.new_checks(&pattern, overlapping_check) {
                    branch.add_check(new_check);
                }
                branches.push_back(branch);
            }
        }
    }

    fn save_index_of_new_choice(&mut self, kind: RuntimeCheckKind) {
        let _ = match kind {
            RuntimeCheckKind::Int { .. }
            | RuntimeCheckKind::Float { .. }
            | RuntimeCheckKind::String { .. }
            | RuntimeCheckKind::Tuple { .. }
            | RuntimeCheckKind::BitArray { .. }
            | RuntimeCheckKind::Variant { .. }
            | RuntimeCheckKind::EmptyList
            | RuntimeCheckKind::NonEmptyList => self.indices.insert(kind, self.choices.len()),

            RuntimeCheckKind::StringPrefix { prefix } => self
                .prefix_indices
                .insert(prefix.to_string(), self.choices.len()),
        };
    }

    fn indices_of_overlapping_checks(&self, kind: &RuntimeCheckKind) -> Vec<usize> {
        match kind {
            // All these checks will only overlap with a check that is exactly the
            // same, so we just look up their index in the `indices` map using the
            // kind as the lookup.
            RuntimeCheckKind::Int { .. }
            | RuntimeCheckKind::Float { .. }
            | RuntimeCheckKind::Tuple { .. }
            | RuntimeCheckKind::BitArray { .. }
            | RuntimeCheckKind::Variant { .. }
            | RuntimeCheckKind::EmptyList
            | RuntimeCheckKind::NonEmptyList => {
                self.indices.get(kind).cloned().into_iter().collect_vec()
            }

            // String patterns are a bit more tricky as they might end up overlapping
            // even if they're not exactly the same kind of check! Let's have a look
            // at an example. Say we're compiling these branches:
            //
            // ```
            // a is "wibble" <> rest -> todo
            // a is "wibbler" <> rest -> todo
            // ```
            //
            // We use the first (and only) check in the first branch as the pivot and
            // now we have to decide where to put the next branch. Is it matching with
            // the first one or completely unrelated?
            // Since `"wibbler"` starts with `"wibble"` we know it's overlapping and
            // it cannot possibly match if the previous one doesn't!
            //
            // So when we find a `String`/`StringPrefix` pattern we look for a prefix
            // among the ones we have discovered so far that could match with it.
            // That is, we look for a prefix of the pattern we're checking in the prefix
            // trie.
            RuntimeCheckKind::StringPrefix { prefix: value } => {
                ancestors_values(&self.prefix_indices, value).collect_vec()
            }

            // Strings are almost exactly the same, except they could also have an exact
            // match with other string patterns. So a string pattern could overlap with
            // another string pattern (if they're matching on the same value), or with
            // one or more string prefix patterns with a matching prefix.
            RuntimeCheckKind::String { value } => {
                let first_index = self.indices.get(kind).cloned();
                first_index
                    .into_iter()
                    .chain(ancestors_values(&self.prefix_indices, value))
                    .collect_vec()
            }
        }
    }
}

fn ancestors_values(trie: &Trie<String, usize>, key: &str) -> impl Iterator<Item = usize> {
    trie.get_ancestor(key)
        .into_iter()
        .flat_map(|ancestor| ancestor.values().copied())
}

pub struct ConstructorSpecialiser {
    specialised_types: HashMap<u64, Arc<Type>>,
}

impl ConstructorSpecialiser {
    fn specialise_constructors(
        constructors: &TypeVariantConstructors,
        type_arguments: &[Arc<Type>],
    ) -> Vec<TypeValueConstructor> {
        let specialiser = Self::new(constructors.type_parameters_ids.as_slice(), type_arguments);
        constructors
            .variants
            .iter()
            .map(|v| specialiser.specialise_type_value_constructor(v))
            .collect_vec()
    }

    fn new(parameters: &[u64], type_arguments: &[Arc<Type>]) -> Self {
        let specialised_types = parameters
            .iter()
            .copied()
            .zip(type_arguments.iter().cloned())
            .collect();
        Self { specialised_types }
    }

    fn specialise_type_value_constructor(&self, v: &TypeValueConstructor) -> TypeValueConstructor {
        let TypeValueConstructor {
            name,
            parameters,
            documentation,
        } = v;
        let parameters = parameters
            .iter()
            .map(|p| TypeValueConstructorField {
                type_: self.specialise_type(p.type_.as_ref()),
                label: p.label.clone(),
            })
            .collect_vec();
        TypeValueConstructor {
            name: name.clone(),
            parameters,
            documentation: documentation.clone(),
        }
    }

    fn specialise_type(&self, type_: &Type) -> Arc<Type> {
        Arc::new(match type_ {
            Type::Named {
                publicity,
                package,
                module,
                name,
                args,
                inferred_variant,
            } => Type::Named {
                publicity: *publicity,
                package: package.clone(),
                module: module.clone(),
                name: name.clone(),
                args: args.iter().map(|a| self.specialise_type(a)).collect(),
                inferred_variant: *inferred_variant,
            },

            Type::Fn { args, retrn } => Type::Fn {
                args: args.iter().map(|a| self.specialise_type(a)).collect(),
                retrn: retrn.clone(),
            },

            Type::Var { type_ } => Type::Var {
                type_: Arc::new(RefCell::new(self.specialise_var(type_))),
            },

            Type::Tuple { elems } => Type::Tuple {
                elems: elems.iter().map(|e| self.specialise_type(e)).collect(),
            },
        })
    }

    fn specialise_var(&self, type_: &RefCell<TypeVar>) -> TypeVar {
        match &*type_.borrow() {
            TypeVar::Unbound { id } => TypeVar::Unbound { id: *id },

            TypeVar::Link { type_ } => TypeVar::Link {
                type_: self.specialise_type(type_.as_ref()),
            },

            TypeVar::Generic { id } => match self.specialised_types.get(id) {
                Some(type_) => TypeVar::Link {
                    type_: type_.clone(),
                },
                None => TypeVar::Generic { id: *id },
            },
        }
    }
}

/// Intermiate data structure that's used to set up everything that's needed by
/// the pattern matching compiler and get a case expression ready to be compiled,
/// while hiding the intricacies of handling an arena to record different patterns.
///
pub struct CaseToCompile {
    patterns: Arena<Pattern>,
    branches: Vec<Branch>,
    subject_variables: Vec<Variable>,
    /// The number of clauses in this case to compile.
    number_of_clauses: usize,
    variable_id: usize,
}

impl CaseToCompile {
    pub fn new(subject_types: &[Arc<Type>]) -> Self {
        let mut variable_id = 0;
        let subject_variables = subject_types
            .iter()
            .map(|type_| {
                let id = variable_id;
                variable_id += 1;
                Variable::new(id, type_.clone())
            })
            .collect_vec();

        Self {
            patterns: Arena::new(),
            branches: vec![],
            number_of_clauses: 0,
            subject_variables,
            variable_id,
        }
    }

    /// Registers a `TypedClause` as one of the branches to be compiled.
    ///
    /// If you don't have a clause and just have a simple `TypedPattern` you want
    /// to generate a decision tree for you can use `add_pattern`.
    ///
    pub fn add_clause(&mut self, branch: &TypedClause) {
        let all_patterns =
            std::iter::once(&branch.pattern).chain(branch.alternative_patterns.iter());

        for (alternative_index, patterns) in all_patterns.enumerate() {
            let mut checks = Vec::with_capacity(patterns.len());

            // We're doing the zipping ourselves instead of using iters.zip as the
            // borrow checker would complain and the only workaround would be to
            // allocate an entire new vector each time.
            for i in 0..patterns.len() {
                let pattern = self.register(patterns.get(i).expect("pattern index"));
                let var = self
                    .subject_variables
                    .get(i)
                    .expect("wrong number of subjects");
                checks.push(var.is(pattern))
            }

            let has_guard = branch.guard.is_some();
            let branch = Branch::new(self.number_of_clauses, alternative_index, checks, has_guard);
            self.branches.push(branch);
        }

        self.number_of_clauses += 1;
    }

    /// Add a single pattern as a branch to be compiled.
    ///
    /// This is useful in case one wants to check exhaustiveness of a single
    /// pattern without having a fully fledged `TypedClause` to pass to the `add_clause`
    /// method. For example, in `let` destructurings.
    ///
    pub fn add_pattern(&mut self, pattern: &TypedPattern) {
        let pattern = self.register(pattern);
        let var = self
            .subject_variables
            .first()
            .expect("wrong number of subject variables for pattern");
        let branch = Branch::new(self.number_of_clauses, 0, vec![var.is(pattern)], false);
        self.number_of_clauses += 1;
        self.branches.push(branch);
    }

    pub fn compile(self, env: &Environment<'_>) -> Match {
        let mut compiler = Compiler::new(env, self.variable_id, self.patterns);

        let decision = if self.branches.is_empty() {
            let var = self
                .subject_variables
                .first()
                .expect("case with no subjects")
                .clone();

            compiler.split_and_compile_with_pivot_var(var, VecDeque::new())
        } else {
            compiler.compile(self.branches.into())
        };

        Match {
            tree: decision,
            diagnostics: compiler.diagnostics,
            subject_variables: self.subject_variables,
        }
    }

    /// Registers a typed pattern (and all its sub-patterns) into this
    /// `CaseToCompile`'s pattern arena, returning an id to get the pattern back.
    ///
    fn register(&mut self, pattern: &TypedPattern) -> Id<Pattern> {
        match pattern {
            TypedPattern::Invalid { .. } => self.insert(Pattern::Discard),
            TypedPattern::Discard { .. } => self.insert(Pattern::Discard),

            TypedPattern::Int { value, .. } => {
                let value = value.clone();
                self.insert(Pattern::Int { value })
            }

            TypedPattern::Float { value, .. } => {
                let value = value.clone();
                self.insert(Pattern::Float { value })
            }

            TypedPattern::String { value, .. } => {
                let value = value.clone();
                self.insert(Pattern::String { value })
            }

            TypedPattern::Variable { name, .. } => {
                let name = name.clone();
                self.insert(Pattern::Variable { name })
            }

            TypedPattern::Assign { name, pattern, .. } => {
                let name = name.clone();
                let pattern = self.register(pattern);
                self.insert(Pattern::Assign { name, pattern })
            }

            TypedPattern::Tuple { elems, .. } => {
                let elements = elems.iter().map(|elem| self.register(elem)).collect_vec();
                self.insert(Pattern::Tuple { elements })
            }

            TypedPattern::List { elements, tail, .. } => {
                let mut list = match tail {
                    Some(tail) => self.register(tail),
                    None => self.insert(Pattern::EmptyList),
                };
                for element in elements.iter().rev() {
                    let first = self.register(element);
                    list = self.insert(Pattern::NonEmptyList { first, rest: list });
                }
                list
            }

            TypedPattern::Constructor {
                arguments,
                constructor,
                ..
            } => {
                let index = constructor.expect_ref("must be inferred").constructor_index as usize;
                let fields = arguments
                    .iter()
                    .map(|argument| self.register(&argument.value))
                    .collect_vec();
                self.insert(Pattern::Variant { index, fields })
            }

            TypedPattern::BitArray { location, .. } => {
                // TODO: in future support bit strings fully and check the
                // exhaustiveness of their segment patterns.
                // For now we use the location to give each bit string a pattern
                // a unique value.
                self.insert(Pattern::BitArray {
                    value: format!("{}:{}", location.start, location.end).into(),
                })
            }

            TypedPattern::StringPrefix {
                left_side_string,
                right_side_assignment,
                ..
            } => {
                let prefix = left_side_string.clone();
                let rest_pattern = match right_side_assignment {
                    AssignName::Variable(name) => Pattern::Variable { name: name.clone() },
                    AssignName::Discard(_) => Pattern::Discard,
                };
                let rest = self.insert(rest_pattern);
                self.insert(Pattern::StringPrefix { prefix, rest })
            }

            TypedPattern::VarUsage { .. } => {
                unreachable!("Cannot convert VarUsage to exhaustiveness pattern")
            }
        }
    }

    fn insert(&mut self, pattern: Pattern) -> Id<Pattern> {
        self.patterns.alloc(pattern)
    }
}
