use std::cell::{RefCell, RefMut};
use std::default::Default;
use std::fmt;
use std::sync::Arc;

use itertools::Itertools;

use union_find::{QuickFindUf, Union, UnionByRank, UnionFind, UnionResult};

use base::fnv::FnvMap;
use base::fixed::{FixedMap, FixedVec};
use base::types;
use base::types::{ArcType, Type, Walker};
use base::symbol::Symbol;

#[derive(Debug, PartialEq)]
pub enum Error<T> {
    Occurs(T, T),
    Constraint(T, Arc<Vec<T>>),
}

impl<T> fmt::Display for Error<T>
where
    T: fmt::Display,
    T: for<'a> types::ToDoc<'a, ::pretty::Arena<'a>, ()>,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match *self {
            Occurs(ref var, ref typ) => write!(f, "Variable `{}` occurs in `{}`.", var, typ),
            Constraint(ref typ, ref constraints) => {
                writeln!(
                    f,
                    "Type `{}` could not fullfill a constraint.\nPossible resolves:",
                    typ,
                )?;
                for constraint in &constraints[..] {
                    writeln!(f, "{}", constraint)?;
                }
                Ok(())
            }
        }
    }
}

use typecheck::unroll_typ;

pub type Constraints<T> = Arc<Vec<T>>;

pub struct Substitution<T>
where
    T: Substitutable,
{
    /// Union-find data structure used to store the relationships of all variables in the
    /// substitution
    union: RefCell<QuickFindUf<UnionByLevel<T>>>,
    /// Vector containing all created variables for this substitution. Needed for the `real` method
    /// which needs to always be able to return a `&T` reference
    variables: FixedVec<T>,
    /// For variables which have been infered to have a real type (not a variable) their types are
    /// stored here. As the type stored will never changed we use a `FixedMap` lets `real` return
    /// `&T` from this map safely.
    types: FixedMap<u32, T>,
    factory: T::Factory,
}

impl<T> Default for Substitution<T>
where
    T: Substitutable,
    T::Factory: Default,
{
    fn default() -> Substitution<T> {
        Substitution::new(Default::default())
    }
}

/// Trait which variables need to implement to allow the substitution to get to the u32 identifying
/// the variable
pub trait Variable {
    fn get_id(&self) -> u32;
}

impl Variable for u32 {
    fn get_id(&self) -> u32 {
        *self
    }
}

pub trait VariableFactory {
    type Variable: Variable;

    fn new(&self, x: u32) -> Self::Variable;
}

impl VariableFactory for () {
    type Variable = u32;

    fn new(&self, x: u32) -> Self::Variable {
        x
    }
}

/// Trait implemented on types which may contain substitutable variables
pub trait Substitutable: Sized {
    type Variable: Variable;
    type Factory: VariableFactory<Variable = Self::Variable>;
    /// Constructs a new object from its variable type
    fn from_variable(x: Self::Variable) -> Self;
    /// Retrieves the variable if `self` is a variable otherwise returns `None`
    fn get_var(&self) -> Option<&Self::Variable>;
    fn traverse<F>(&self, f: &mut F)
    where
        F: Walker<Self>;
    fn instantiate(
        &self,
        subs: &Substitution<Self>,
        constraints: &FnvMap<Symbol, Constraints<Self>>,
    ) -> Self;
}

fn occurs<T>(typ: &T, subs: &Substitution<T>, var: &T::Variable) -> bool
where
    T: Substitutable,
{
    struct Occurs<'a, T: Substitutable + 'a> {
        occurs: bool,
        var: &'a T::Variable,
        subs: &'a Substitution<T>,
    }
    impl<'a, T> Walker<T> for Occurs<'a, T>
    where
        T: Substitutable,
    {
        fn walk(&mut self, typ: &T) {
            if self.occurs {
                return;
            }
            let typ = self.subs.real(typ);
            if let Some(other) = typ.get_var() {
                if self.var.get_id() == other.get_id() {
                    self.occurs = true;
                    typ.traverse(self);
                    return;
                }
                self.subs.update_level(self.var.get_id(), other.get_id());
            }
            typ.traverse(self);
        }
    }
    let mut occurs = Occurs {
        occurs: false,
        var: var,
        subs: subs,
    };
    occurs.walk(typ);
    occurs.occurs
}

/// Specialized union implementation which makes sure that variables with a higher level always
/// point to the lower level variable.
///
/// map.union(1, 2);
/// map.find(2) -> 1
/// map.find(1) -> 1
#[derive(Debug)]
struct UnionByLevel<T> {
    rank: UnionByRank,
    level: u32,
    constraints: FnvMap<Symbol, Arc<Vec<T>>>,
}

impl<T> Default for UnionByLevel<T> {
    fn default() -> UnionByLevel<T> {
        UnionByLevel {
            rank: UnionByRank::default(),
            level: ::std::u32::MAX,
            constraints: FnvMap::default(),
        }
    }
}

impl<T> Union for UnionByLevel<T> {
    #[inline]
    fn union(mut left: UnionByLevel<T>, right: UnionByLevel<T>) -> UnionResult<UnionByLevel<T>> {
        use std::cmp::Ordering;
        left.constraints.extend(right.constraints);
        let (rank_result, rank) = match Union::union(left.rank, right.rank) {
            UnionResult::Left(l) => (
                UnionResult::Left(UnionByLevel {
                    rank: l,
                    level: left.level,
                    constraints: left.constraints,
                }),
                l,
            ),
            UnionResult::Right(r) => (
                UnionResult::Right(UnionByLevel {
                    rank: r,
                    level: left.level,
                    constraints: left.constraints,
                }),
                r,
            ),
        };
        match left.level.cmp(&right.level) {
            Ordering::Less => UnionResult::Left(UnionByLevel {
                rank: rank,
                level: left.level,
                constraints: match rank_result {
                    UnionResult::Left(x) | UnionResult::Right(x) => x.constraints,
                },
            }),
            Ordering::Greater => UnionResult::Right(UnionByLevel {
                rank: rank,
                level: right.level,
                constraints: match rank_result {
                    UnionResult::Left(x) | UnionResult::Right(x) => x.constraints,
                },
            }),
            Ordering::Equal => rank_result,
        }
    }
}

impl<T> fmt::Debug for Substitution<T>
where
    T: fmt::Debug + Substitutable,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Substitution {{ map: {:?}, var_id: {:?} }}",
            self.union.borrow(),
            self.var_id()
        )
    }
}

impl<T: Substitutable> Substitution<T> {
    pub fn new(factory: T::Factory) -> Substitution<T> {
        Substitution {
            union: RefCell::new(QuickFindUf::new(0)),
            variables: FixedVec::new(),
            types: FixedMap::new(),
            factory: factory,
        }
    }

    pub fn var_id(&self) -> u32 {
        self.variables.borrow().len() as u32
    }

    pub fn clear(&mut self) {
        self.types.clear();
        self.variables.clear();
    }

    pub fn insert(&self, var: u32, t: T) {
        match t.get_var() {
            Some(_) => panic!(
                "Tried to insert variable which is not allowed as that would cause memory \
                 unsafety"
            ),
            None => match self.types.try_insert(var, t) {
                Ok(()) => (),
                Err(_) => panic!("Expected variable to not have a type associated with it"),
            },
        }
    }

    /// Creates a new variable
    pub fn new_var(&self) -> T
    where
        T: Clone,
    {
        self.new_constrained_var(None)
    }

    pub fn new_constrained_var(&self, constraint: Option<(Symbol, Constraints<T>)>) -> T
    where
        T: Clone,
    {
        let var_id = self.variables.len() as u32;
        let id = self.union.borrow_mut().insert(UnionByLevel {
            constraints: constraint.into_iter().collect(),
            ..UnionByLevel::default()
        });
        assert!(id == self.variables.len());

        let var = T::from_variable(self.factory.new(var_id));
        self.variables.push(var.clone());
        var
    }

    /// If `typ` is a variable this returns the real unified value of that variable. Otherwise it
    /// just returns the type itself. Note that the returned type may contain terms which also need
    /// to have `real` called on them.
    pub fn real<'r>(&'r self, typ: &'r T) -> &'r T {
        match typ.get_var() {
            Some(var) => match self.find_type_for_var(var.get_id()) {
                Some(t) => t,
                None => typ,
            },
            _ => typ,
        }
    }

    pub fn find_type_for_var(&self, var: u32) -> Option<&T> {
        let mut union = self.union.borrow_mut();
        if var as usize >= union.size() {
            return None;
        }
        let index = union.find(var as usize) as u32;
        self.types.get(&index).or_else(|| if var == index {
            None
        } else {
            Some(&self.variables[index as usize])
        })
    }

    /// Updates the level of `other` to be the minimum level value of `var` and `other`
    pub fn update_level(&self, var: u32, other: u32) {
        let level = ::std::cmp::min(self.get_level(var), self.get_level(other));
        let mut union = self.union.borrow_mut();
        union.get_mut(other as usize).level = level;
    }

    pub fn get_level(&self, mut var: u32) -> u32 {
        if let Some(v) = self.find_type_for_var(var) {
            var = v.get_var().map_or(var, |v| v.get_id());
        }
        let mut union = self.union.borrow_mut();
        let level = &mut union.get_mut(var as usize).level;
        *level = ::std::cmp::min(*level, var);
        *level
    }

    pub fn get_constraints(&self, var: u32) -> Option<RefMut<FnvMap<Symbol, Constraints<T>>>> {
        let union = self.union.borrow_mut();
        let set = RefMut::map(union, |x| &mut x.get_mut(var as usize).constraints);
        if set.is_empty() {
            None
        } else {
            Some(set)
        }
    }

    pub fn replace_variable(&self, typ: &T) -> Option<T>
    where
        T: Clone,
    {
        match typ.get_var() {
            Some(id) => self.find_type_for_var(id.get_id()).cloned(),
            None => None,
        }
    }
}

impl Substitution<ArcType> {
    fn replace_variable_(&self, typ: &Type<Symbol>) -> Option<ArcType> {
        match *typ {
            Type::Variable(ref id) => self.find_type_for_var(id.id).cloned(),
            _ => None,
        }
    }
    pub fn set_type(&self, t: ArcType) -> ArcType {
        types::walk_move_type(t, &mut |typ| {
            let replacement = self.replace_variable_(typ);
            let result = {
                let mut typ = typ;
                if let Some(ref t) = replacement {
                    typ = t;
                }
                unroll_typ(typ)
            };
            result.or(replacement)
        })
    }
}

impl<T: Substitutable + Clone> Substitution<T> {
    pub fn make_real(&self, typ: &mut T) {
        *typ = self.real(typ).clone();
    }
}
impl<T: Substitutable + PartialEq + Clone> Substitution<T> {
    /// Takes `id` and updates the substitution to say that it should have the same type as `typ`
    pub fn union<P, S>(&self, state: P, id: &T::Variable, typ: &T) -> Result<Option<T>, Error<T>>
    where
        T::Variable: Clone,
        T: Unifiable<S> + fmt::Display,
        P: FnMut() -> S,
    {
        // Nothing needs to be done if both are the same variable already (also prevents the occurs
        // check from failing)
        if typ.get_var()
            .map_or(false, |other| other.get_id() == id.get_id())
        {
            return Ok(None);
        }
        if occurs(typ, self, id) {
            return Err(Error::Occurs(T::from_variable(id.clone()), typ.clone()));
        }
        {
            let id_type = self.find_type_for_var(id.get_id());
            let other_type = self.real(typ);
            if id_type.map_or(false, |x| x == other_type) ||
                other_type.get_var().map(|y| y.get_id()) == Some(id.get_id())
            {
                return Ok(None);
            }
        }
        let resolved_type = if typ.get_var().is_none() {
            self.resolve_constraints(state, id, typ)?
        } else {
            None
        };
        {
            let typ = resolved_type.as_ref().unwrap_or(typ);
            match typ.get_var().map(|id| id.get_id()) {
                Some(other_id) => {
                    self.union
                        .borrow_mut()
                        .union(id.get_id() as usize, other_id as usize);
                    self.update_level(id.get_id(), other_id);
                    self.update_level(other_id, id.get_id());
                }
                _ => {
                    self.insert(id.get_id(), typ.clone());
                }
            }
        }
        Ok(resolved_type)
    }

    pub fn resolve_constraints<P, S>(
        &self,
        mut state: P,
        id: &T::Variable,
        typ: &T,
    ) -> Result<Option<T>, Error<T>>
    where
        T::Variable: Clone,
        T: Unifiable<S> + fmt::Display,
        P: FnMut() -> S,
    {
        use std::borrow::Cow;

        let constraints = self.union
            .borrow_mut()
            .get(id.get_id() as usize)
            .constraints
            .clone();

        let mut typ = Cow::Borrowed(typ);
        for (constraint_name, constraint) in &constraints {
            debug!(
                "Attempting to resolve {} to the constraints {}:\n{}",
                typ,
                constraint_name,
                constraint.iter().format("\n")
            );
            let resolved = constraint
                .iter()
                .filter_map(|constraint_type| {
                    let constraint_type = constraint_type.instantiate(self, &FnvMap::default());
                    match equivalent(state(), self, &constraint_type, &typ) {
                        Ok(()) => Some(constraint_type),
                        Err(()) => None,
                    }
                })
                .next();
            match resolved {
                None => {
                    debug!("Unable to resolve {}", typ);
                    return Err(Error::Constraint(typ.into_owned(), constraint.clone()));
                }
                Some(resolved) => {
                    // Only replace the type if it is replaced by a lower level variable or a
                    // concrete type
                    match (
                        typ.get_var().map(|x| x.get_id()),
                        resolved.get_var().map(|x| x.get_id()),
                    ) {
                        (Some(x), Some(y)) if x > y => {
                            typ = Cow::Owned(resolved);
                        }
                        (_, None) => {
                            typ = Cow::Owned(resolved);
                        }
                        _ => (),
                    }
                }
            }
        }
        if !constraints.is_empty() {
            debug!("Resolved {}", typ);
        }
        Ok(match typ {
            Cow::Borrowed(_) => None,
            Cow::Owned(typ) => Some(typ),
        })
    }
}

use unify::{Error as UnifyError, Unifiable, Unifier, UnifierState};

pub fn equivalent<S, T>(
    state: S,
    subs: &Substitution<T>,
    actual: &T,
    inferred: &T,
) -> Result<(), ()>
where
    T: Unifiable<S> + PartialEq + Clone,
    T::Variable: Clone,
{
    let mut unifier = UnifierState {
        state,
        unifier: Equivalent {
            equiv: true,
            subs,
            temp_subs: FnvMap::default(),
        },
    };
    unifier.try_match(actual, inferred);
    if !unifier.unifier.equiv {
        Err(())
    } else {
        Ok(())
    }
}

struct Equivalent<'e, T: Substitutable + 'e> {
    equiv: bool,
    subs: &'e Substitution<T>,
    temp_subs: FnvMap<u32, T>,
}

impl<'e, S, T> Unifier<S, T> for Equivalent<'e, T>
where
    T: Unifiable<S> + PartialEq + Clone + 'e,
    T::Variable: Clone,
{
    fn report_error(unifier: &mut UnifierState<S, Self>, _error: UnifyError<T, T::Error>) {
        unifier.unifier.equiv = false;
    }

    fn try_match_res(
        unifier: &mut UnifierState<S, Self>,
        l: &T,
        r: &T,
    ) -> Result<Option<T>, UnifyError<T, T::Error>> {
        let (l, r) = {
            use std::borrow::Cow;

            let subs = unifier.unifier.subs;
            let temp_subs = &mut unifier.unifier.temp_subs;

            let l = subs.real(l);
            let l = l.get_var()
                .and_then(|l| temp_subs.get(&l.get_id()).cloned().map(Cow::Owned))
                .unwrap_or(Cow::Borrowed(l));

            let r = subs.real(r);
            let r = r.get_var()
                .and_then(|r| temp_subs.get(&r.get_id()).cloned().map(Cow::Owned))
                .unwrap_or(Cow::Borrowed(r));

            match (l.get_var(), r.get_var()) {
                (Some(l), Some(r)) if l.get_id() == r.get_id() => return Ok(None),
                (_, Some(r)) => {
                    temp_subs.insert(r.get_id(), l.clone().into_owned());
                    return Ok(None);
                }
                (Some(l), _) => {
                    temp_subs.insert(l.get_id(), r.clone().into_owned());
                    return Ok(None);
                }
                (_, _) => {}
            }
            (l, r)
        };
        // Both sides are concrete types, the only way they can be equal is if
        // the matcher finds their top level to be equal (and their sub-terms
        // unify)
        l.zip_match(&r, unifier)
    }

    fn error_type(_unifier: &mut UnifierState<S, Self>) -> Option<T> {
        None
    }
}
