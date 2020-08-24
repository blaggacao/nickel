//! Program transformations.

use crate::error::ImportError;
use crate::eval::{Closure, Environment, IdentKind};
use crate::identifier::Ident;
use crate::program::ImportResolver;
use crate::term::{RichTerm, Term};
use crate::types::{AbsType, Types};
use codespan::FileId;
use simple_counter::*;
use std::cell::RefCell;
use std::rc::Rc;

generate_counter!(FreshVarCounter, usize);

/// Share normal form.
///
/// Replace the subexpressions of WHNFs that are not functions by thunks, such that they can be
/// shared. It is similar to the behavior of other lazy languages with respect to data
/// constructors.  To do so, subexpressions are replaced by fresh variables, introduced by new let
/// bindings put at the beginning of the WHNF.
///
/// For example, take the expression
/// ```
/// let x = {a = (1 + 1);} in x.a + x.a
/// ```
///
/// The term `{a = 1 + 1;}` is a record, and hence a WHNF. In consequence, the thunk allocated to x
/// is never updated. Without additional machinery, `a` will be recomputed each time is it used,
/// two times here.
///
/// The transformation replaces such subexpressions, namely the content of the fields
/// of records and the elements of lists - `(1 + 1)` in our example -, with fresh variables
/// introduced by `let`  added at the head of the term:
///
/// ```
/// let x = (let var = 1 + 1 in {a = var;}) in x.a + x.a
/// ```
///
/// Now, the field `a` points to the thunk introduced by `var`: at the evaluation of the first
/// occurrence of `x.a`, this thunk is updated with `2`, and is not recomputed the second time.
///
/// Newly introduced variables begin with a special character to avoid clashing with user-defined
/// variables.
pub mod share_normal_form {
    use super::fresh_var;
    use crate::term::{RichTerm, Term};

    /// Transform the top-level term of an AST to a share normal form, if it can.
    ///
    /// This function is not recursive: it just tries to apply one step of the transformation to
    /// the top-level node of the AST. For example, it transforms `[1 + 1, [1 + 2]]` to `let %0 = 1
    /// + 1 in [%0, [1 + 2]]`: the nested subterm `[1 + 2]` is left as it was. If the term is
    /// neither a record, a list nor an enriched value, it is returned the same.  In other words,
    /// the transformation is implemented as a *recursion scheme*, and must be used in conjunction
    /// a traversal to obtain a full transformation.
    pub fn transform_one(rt: RichTerm) -> RichTerm {
        let RichTerm { term, pos } = rt;
        let pos = pos.clone();
        match *term {
            Term::Record(map) => {
                let mut bindings = Vec::with_capacity(map.len());

                let map = map
                    .into_iter()
                    .map(|(id, t)| {
                        if should_share(&t.term) {
                            let fresh_var = fresh_var();
                            bindings.push((fresh_var.clone(), t));
                            (id, Term::Var(fresh_var).into())
                        } else {
                            (id, t)
                        }
                    })
                    .collect();

                let result = bindings.into_iter().fold(
                    RichTerm {
                        term: Box::new(Term::Record(map)),
                        pos,
                    },
                    |acc, (id, t)| Term::Let(id, t, acc).into(),
                );

                result.into()
            }
            Term::List(ts) => {
                let mut bindings = Vec::with_capacity(ts.len());

                let ts = ts
                    .into_iter()
                    .map(|t| {
                        if should_share(&t.term) {
                            let fresh_var = fresh_var();
                            bindings.push((fresh_var.clone(), t));
                            Term::Var(fresh_var).into()
                        } else {
                            t
                        }
                    })
                    .collect();

                let result = bindings.into_iter().fold(
                    RichTerm {
                        term: Box::new(Term::List(ts)),
                        pos,
                    },
                    |acc, (id, t)| Term::Let(id, t, acc).into(),
                );

                result.into()
            }
            Term::DefaultValue(t) => {
                if should_share(&t.term) {
                    let fresh_var = fresh_var();
                    let inner = RichTerm {
                        term: Box::new(Term::DefaultValue(Term::Var(fresh_var.clone()).into())),
                        pos,
                    };
                    Term::Let(fresh_var, t, inner).into()
                } else {
                    RichTerm {
                        term: Box::new(Term::DefaultValue(t)),
                        pos,
                    }
                }
            }
            Term::ContractWithDefault(ty, lbl, t) => {
                if should_share(&t.term) {
                    let fresh_var = fresh_var();
                    let inner = RichTerm {
                        term: Box::new(Term::ContractWithDefault(
                            ty,
                            lbl,
                            Term::Var(fresh_var.clone()).into(),
                        )),
                        pos,
                    };
                    Term::Let(fresh_var, t, inner).into()
                } else {
                    RichTerm {
                        term: Box::new(Term::ContractWithDefault(ty, lbl, t)),
                        pos,
                    }
                }
            }
            Term::Docstring(s, t) => {
                if should_share(&t.term) {
                    let fresh_var = fresh_var();
                    let inner = RichTerm {
                        term: Box::new(Term::Docstring(s, Term::Var(fresh_var.clone()).into())),
                        pos,
                    };
                    Term::Let(fresh_var, t, inner).into()
                } else {
                    RichTerm {
                        term: Box::new(Term::Docstring(s, t)),
                        pos,
                    }
                }
            }
            t => RichTerm {
                term: Box::new(t),
                pos,
            },
        }
    }

    /// Determine if a subterm of a WHNF should be wrapped in a thunk in order to be shared.
    ///
    /// Sharing is typically useless if the subterm is already a WHNF which can be copied without
    /// duplicating any work. On the other hand, a WHNF which can contain other shareable
    /// subexpressions, such as a record, should be shared.
    fn should_share(t: &Term) -> bool {
        match t {
            Term::Bool(_)
            | Term::Num(_)
            | Term::Str(_)
            | Term::Lbl(_)
            | Term::Sym(_)
            | Term::Var(_)
            | Term::Enum(_)
            | Term::Fun(_, _) => false,
            _ => true,
        }
    }
}

pub mod import_resolution {
    use super::{FileId, ImportResolver, RichTerm, Term};
    use crate::error::ImportError;
    use crate::program::ResolvedTerm;

    /// Resolve the import if the term is an unresolved import, or return the term unchanged.
    ///
    /// If an import was resolved, the corresponding `FileId` is returned in the second component
    /// of the result. It the import has been already resolved, or if the term was not an import,
    /// `None` is returned. As [`share_normal_form::transform_one`](./mod.?), this function is not
    /// recursive.
    pub fn transform_one<R>(
        rt: RichTerm,
        resolver: &mut R,
    ) -> Result<(RichTerm, Option<(RichTerm, FileId)>), ImportError>
    where
        R: ImportResolver,
    {
        let RichTerm { term, pos } = rt;
        match *term {
            Term::Import(path) => {
                let (res_term, file_id) = resolver.resolve(&path, &pos)?;
                let ret = match res_term {
                    ResolvedTerm::FromCache() => None,
                    ResolvedTerm::FromFile(t) => Some((t, file_id)),
                };

                Ok((
                    RichTerm {
                        term: Box::new(Term::ResolvedImport(file_id)),
                        pos,
                    },
                    ret,
                ))
            }
            t => Ok((
                RichTerm {
                    term: Box::new(t),
                    pos,
                },
                None,
            )),
        }
    }
}

/// The state passed around during the program transformation. It holds a reference to the import
/// resolver and to a stack of pending imported term to be transformed.
struct TransformState<'a, R> {
    resolver: &'a mut R,
    stack: &'a mut Vec<(RichTerm, FileId)>,
}

/// Apply all program transformations, which are currently the share normal form transformation and
/// import resolution.
///
/// All resolved imports are stacked during the transformation. Once the term has been traversed,
/// the elements of this stack are processed (and so on, if these elements also have non resolved
/// imports).
pub fn transform<R>(rt: RichTerm, resolver: &mut R) -> Result<RichTerm, ImportError>
where
    R: ImportResolver,
{
    let mut stack = Vec::new();

    let result = transform_pass(rt, resolver, &mut stack);

    while let Some((t, file_id)) = stack.pop() {
        let result = transform_pass(t, resolver, &mut stack)?;
        resolver.insert(file_id, result);
    }

    result
}

/// Perform one full transformation pass. Put all imports encountered for the first time in
/// `stack`, but do not process them.
fn transform_pass<R>(
    rt: RichTerm,
    resolver: &mut R,
    stack: &mut Vec<(RichTerm, FileId)>,
) -> Result<RichTerm, ImportError>
where
    R: ImportResolver,
{
    let mut state = TransformState { resolver, stack };

    // Apply one step of each transformation. If an import is resolved, then stack it.
    rt.traverse(
        &mut |rt: RichTerm, state: &mut TransformState<R>| -> Result<RichTerm, ImportError> {
            let rt = share_normal_form::transform_one(rt);
            let (rt, to_queue) = import_resolution::transform_one(rt, state.resolver)?;

            if let Some((t, file_id)) = to_queue {
                state.stack.push((t, file_id));
            }

            Ok(rt)
        },
        &mut state,
    )
}

/// Generate a new fresh variable which do not clash with user-defined variables.
fn fresh_var() -> Ident {
    Ident(format!("%{}", FreshVarCounter::next()))
}

/// Structures which can be packed together with their environment as a closure.
///
/// The typical implementer is [`RichTerm`](../term/enum.RichTerm.html), but structures containing
/// terms can also be closurizable, such as the contract in a [`Types`](../types/typ.Types.html).
/// In this case, the inner term is closurized.
pub trait Closurizable {
    /// Pack a closurizable together with its environment `with_env` as a closure in the main
    /// environment `env`.
    fn closurize(self, env: &mut Environment, with_env: Environment) -> Self;
}

impl Closurizable for RichTerm {
    /// Pack a term together with an environment as a closure.
    ///
    /// Generate a fresh variable, bind it to the corresponding closure `(t,with_env)` in `env`,
    /// and return this variable as a fresh term.
    fn closurize(self, env: &mut Environment, with_env: Environment) -> RichTerm {
        let var = fresh_var();
        let c = Closure {
            body: self,
            env: with_env,
        };

        env.insert(var.clone(), (Rc::new(RefCell::new(c)), IdentKind::Record()));

        Term::Var(var).into()
    }
}

impl Closurizable for Types {
    /// Pack the contract of a type together with an environment as a closure.
    ///
    /// Extract the underlying contract, closurize it and wrap it back as a flat type (an opaque
    /// type defined by a custom contract).
    fn closurize(self, env: &mut Environment, with_env: Environment) -> Types {
        Types(AbsType::Flat(self.contract().closurize(env, with_env)))
    }
}
